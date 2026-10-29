use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use muxy_protocol::{ProjectDescriptor, WorktreeHook};
use serde::Deserialize;

use super::{Result, command, error, path};

#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    setup: Vec<Hook>,
    teardown: Vec<Hook>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Hook {
    Command(String),
    Named {
        command: String,
        name: Option<String>,
    },
}

fn global_path() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| error("Home directory is unavailable"))?;
    Ok(preferred(
        root.join("atur-terminal/worktree.json"),
        root.join("muxy/worktree.json"),
    ))
}

/// `current`, unless only the file earlier builds read exists.
fn preferred(current: PathBuf, legacy: PathBuf) -> PathBuf {
    if current.exists() || !legacy.exists() {
        current
    } else {
        legacy
    }
}

fn load(path: &Path, teardown: bool, project: bool) -> Result<Vec<WorktreeHook>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(cause) => return Err(error(format!("Could not read {}: {cause}", path.display()))),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    if bytes.len() > 1024 * 1024 {
        return Err(error("Worktree hook configuration is too large"));
    }
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|cause| error(format!("Invalid {}: {cause}", path.display())))?;
    let hooks = if teardown {
        config.teardown
    } else {
        config.setup
    };
    let hooks: Vec<_> = hooks
        .into_iter()
        .filter_map(|hook| {
            let (command, name) = match hook {
                Hook::Command(command) => (command, None),
                Hook::Named { command, name } => (command, name),
            };
            let command = command.trim().to_owned();
            (!command.is_empty()).then_some(WorktreeHook {
                command,
                name,
                project,
            })
        })
        .collect();
    if hooks.len() > 128
        || hooks.iter().any(|hook| {
            hook.command.len() > 16 * 1024
                || hook.command.contains('\0')
                || hook.name.as_ref().is_some_and(|name| name.len() > 1024)
        })
    {
        return Err(error(
            "Worktree hook configuration exceeds the command limits",
        ));
    }
    Ok(hooks)
}

pub(super) fn resolve(
    repository: &Path,
    global: Option<&Path>,
    teardown: bool,
) -> Result<Vec<WorktreeHook>> {
    match global {
        Some(global) => resolve_at(repository, global, teardown),
        None => resolve_at(repository, &global_path()?, teardown),
    }
}

fn resolve_at(repository: &Path, global: &Path, teardown: bool) -> Result<Vec<WorktreeHook>> {
    let mut global = load(global, teardown, false)?;
    let project = preferred(
        repository.join(".atur-terminal/worktree.json"),
        repository.join(".muxy/worktree.json"),
    );
    let mut project = load(&project, teardown, true)?;
    let hooks = if teardown {
        project.append(&mut global);
        project
    } else {
        global.append(&mut project);
        global
    };
    if hooks.len() > 128 {
        return Err(error("Too many worktree hooks"));
    }
    Ok(hooks)
}

pub(super) fn approved(
    repository: &Path,
    global: Option<&Path>,
    teardown: bool,
    approved: &[WorktreeHook],
) -> Result<()> {
    if resolve(repository, global, teardown)? != approved {
        return Err(error(
            "Worktree hooks changed after approval. Review them and try again.",
        ));
    }
    Ok(())
}

pub(super) fn run(
    repository: &Path,
    project: &ProjectDescriptor,
    branch: &str,
    hooks: &[WorktreeHook],
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(300);
    let shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|shell| shell.is_absolute() && shell.is_file())
        .unwrap_or_else(|| "/bin/sh".into());
    for hook in hooks {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| error("Worktree hooks timed out"))?;
        let mut process = Command::new(&shell);
        process
            .args(["-c", &hook.command])
            .current_dir(path(&project.directory))
            .env("ATUR_TERM_PROJECT_PATH", repository)
            .env("ATUR_TERM_WORKTREE_ID", project.id.to_string())
            .env("ATUR_TERM_WORKTREE_PATH", path(&project.directory))
            .env("ATUR_TERM_WORKTREE_NAME", &project.name)
            .env("ATUR_TERM_WORKTREE_BRANCH", branch);
        if let Some(path) = crate::exec::login_path() {
            process.env("PATH", path);
        }
        command::capture_with(process, remaining, None, false).map_err(|cause| {
            error(format!(
                "Worktree hook failed ({}): {cause}",
                hook.name.as_deref().unwrap_or(&hook.command)
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_legacy_file_is_used_only_when_the_current_one_is_missing() {
        let root =
            std::env::temp_dir().join(format!("atur-hooks-{}", muxy_protocol::OperationId::new()));
        let current = root.join("atur-terminal/worktree.json");
        let legacy = root.join("muxy/worktree.json");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();

        assert_eq!(preferred(current.clone(), legacy.clone()), current);
        std::fs::write(&legacy, "{}").unwrap();
        assert_eq!(preferred(current.clone(), legacy.clone()), legacy);
        std::fs::write(&current, "{}").unwrap();
        assert_eq!(preferred(current.clone(), legacy), current);
        std::fs::remove_dir_all(root).unwrap();
    }
}
