//! The atur-term-cli skill, which teaches AI coding agents to use this command,
//! and installing it where agents look for skills.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SKILL: &str = include_str!("../skills/atur-term-cli/SKILL.md");

/// Skill folder name written by releases before the rename; replaced on install.
const LEGACY_FOLDER: &str = "muxy-cli";

/// Agent folders in the home folder; each keeps its skills in `skills`.
const AGENTS: [&str; 3] = [".claude", ".codex", ".agents"];

/// Writes the skill into every agent folder in `home` and into each of
/// `directories`, replacing an older copy, and returns the files written.
pub(crate) fn install(home: Option<&Path>, directories: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = home
        .into_iter()
        .flat_map(|home| AGENTS.iter().map(move |agent| home.join(agent)))
        .filter(|agent| agent.is_dir())
        .map(|agent| agent.join("skills"))
        .collect();
    for directory in directories {
        roots.push(std::path::absolute(directory)?);
    }
    if roots.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "found no AI agent folder (~/.claude, ~/.codex or ~/.agents); choose one with --dir",
        ));
    }
    roots
        .into_iter()
        .map(|root| {
            let legacy = root.join(LEGACY_FOLDER);
            if legacy.join("SKILL.md").is_file() {
                fs::remove_dir_all(&legacy)?;
            }
            let folder = root.join("atur-term-cli");
            fs::create_dir_all(&folder)?;
            let file = folder.join("SKILL.md");
            fs::write(&file, SKILL)?;
            Ok(file)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_into_agent_folders_that_exist_and_chosen_folders() -> io::Result<()> {
        let home = tempfile::tempdir()?;
        assert!(install(Some(home.path()), &[]).is_err());
        fs::create_dir(home.path().join(".claude"))?;
        let chosen = home.path().join("chosen");
        let written = install(Some(home.path()), std::slice::from_ref(&chosen))?;
        assert_eq!(
            written,
            [
                home.path().join(".claude/skills/atur-term-cli/SKILL.md"),
                chosen.join("atur-term-cli/SKILL.md"),
            ]
        );
        assert!(!home.path().join(".codex").exists());
        assert_eq!(fs::read_to_string(&written[0])?, SKILL);
        Ok(())
    }

    #[test]
    fn replaces_a_skill_folder_left_by_the_old_name() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let old = directory.path().join(LEGACY_FOLDER);
        fs::create_dir(&old)?;
        fs::write(old.join("SKILL.md"), "old")?;
        install(None, &[directory.path().to_path_buf()])?;
        assert!(!old.exists());
        assert!(directory.path().join("atur-term-cli/SKILL.md").is_file());
        Ok(())
    }

    #[test]
    fn installs_into_a_chosen_folder_without_a_home() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let chosen = directory.path().join("skills");
        let written = install(None, std::slice::from_ref(&chosen))?;
        assert_eq!(written, [chosen.join("atur-term-cli/SKILL.md")]);
        assert_eq!(fs::read_to_string(&written[0])?, SKILL);
        assert!(
            matches!(install(None, &[]), Err(error) if error.kind() == io::ErrorKind::NotFound)
        );
        Ok(())
    }

    /// Splits a shell command line into words, keeping quoted text together
    /// and standing in a session ID for each `$VARIABLE`.
    fn words(line: &str) -> Vec<std::ffi::OsString> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut quoted = false;
        for character in line.chars() {
            match character {
                '"' => quoted = !quoted,
                ' ' if !quoted => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                _ => word.push(character),
            }
        }
        words.push(word);
        words
            .into_iter()
            .filter(|word| !word.is_empty())
            .map(|word| {
                if word.starts_with('$') {
                    "7".into()
                } else {
                    word.into()
                }
            })
            .collect()
    }

    #[test]
    fn every_command_in_the_skill_parses() {
        assert!(SKILL.starts_with("---\nname: atur-term-cli\n"));
        let commands: Vec<_> = SKILL
            .lines()
            .filter_map(|line| line.split_once("atur-term ").map(|(_, command)| command))
            .filter(|command| !command.starts_with("<command>") && !command.starts_with("--help"))
            .map(|command| {
                let command = command.split("  #").next().unwrap_or(command);
                command.trim_end().trim_end_matches(')')
            })
            .filter(|command| !command.contains('`'))
            .collect();
        assert!(commands.len() > 15, "{commands:?}");
        for command in commands {
            assert!(
                crate::args::parse(&words(command)).is_ok(),
                "atur-term {command}"
            );
        }
    }
}
