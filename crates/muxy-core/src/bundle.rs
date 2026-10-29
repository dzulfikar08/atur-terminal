use crate::file_lock::FileLock;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct BundleLease {
    _file: File,
}

pub fn lock_replacement(directory: &Path) -> io::Result<FileLock> {
    crate::file_lock::try_lock(&directory.join(".atur-term-beta-update.lock"))
}

/// The app bundle whose `Contents/MacOS` holds `executable`.
pub fn containing(executable: &Path) -> Option<&Path> {
    executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|path| path.extension().is_some_and(|extension| extension == "app"))
}

pub fn acquire_runtime(
    executable: &Path,
    expected_metadata: &[u8],
) -> io::Result<Option<BundleLease>> {
    #[cfg(target_os = "linux")]
    if let Some(root) = linux_root(executable) {
        let directory = executable
            .parent()
            .ok_or_else(|| io::Error::other("Missing executable directory"))?;
        let _replacement = wait_for_replacement(root)?;
        if crate::executable::build_metadata(executable)? != expected_metadata {
            return Err(io::Error::other(
                "The installed Atur Terminal changed during startup. Run the command again.",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.join(".lease"))?;
        file.lock_shared()?;
        return Ok(Some(BundleLease { _file: file }));
    }
    let Some(bundle) = containing(executable) else {
        return Ok(None);
    };
    let identity = identity(bundle)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let _replacement = loop {
        match lock_replacement(
            bundle
                .parent()
                .ok_or_else(|| io::Error::other("Missing application directory"))?,
        ) {
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            result => break result?,
        }
    };
    if self::identity(bundle)? != identity
        || crate::executable::build_metadata(executable)? != expected_metadata
    {
        return Err(io::Error::other(
            "The installed Atur Terminal changed during startup. Run the command again.",
        ));
    }
    let file = lease_file(&lease_directory()?, &identity)?;
    file.lock_shared()?;
    Ok(Some(BundleLease { _file: file }))
}

/// The root of a versioned, user-installed Linux desktop, including its matching helpers.
#[cfg(target_os = "linux")]
pub fn linux_root(executable: &Path) -> Option<&Path> {
    let version = executable.parent()?;
    let versions = version.parent()?;
    let root = versions.parent()?;
    (versions.file_name()? == "versions"
        && crate::release::Version::parse(version.file_name()?.to_str()?).is_some()
        && root.join("installation.json").is_file())
    .then_some(root)
}

#[cfg(target_os = "linux")]
fn wait_for_replacement(root: &Path) -> io::Result<FileLock> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match lock_replacement(root) {
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            result => return result,
        }
    }
}

pub fn lock_unused(bundle: &Path) -> io::Result<Option<File>> {
    let file = lease_file(&lease_directory()?, &identity(bundle)?)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

fn identity(bundle: &Path) -> io::Result<String> {
    let metadata = fs::metadata(bundle)?;
    Ok(format!("{:x}-{:x}", metadata.dev(), metadata.ino()))
}

fn lease_directory() -> io::Result<PathBuf> {
    let home =
        std::env::home_dir().ok_or_else(|| io::Error::other("Home directory unavailable"))?;
    Ok(home.join("Library/Application Support/Atur Terminal Runtime/leases"))
}

fn lease_file(directory: &Path, identity: &str) -> io::Result<File> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join(format!("{identity}.lock")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_executables_inside_an_app_bundle_have_one() {
        assert_eq!(
            containing(Path::new(
                "/Applications/Atur Terminal.app/Contents/MacOS/atur-terminal"
            )),
            Some(Path::new("/Applications/Atur Terminal.app"))
        );
        assert_eq!(
            containing(Path::new("/repo/target/debug/atur-terminal")),
            None
        );
    }

    #[test]
    fn leases_follow_bundle_identity_through_replacement_without_holding_installation_lock()
    -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let current = root.path().join("Atur Terminal Beta.app");
        let retired = root.path().join("previous.app");
        fs::create_dir(&current)?;
        let lease = lease_file(root.path(), &identity(&current)?)?;
        lease.lock_shared()?;
        let _installation = lock_replacement(root.path())?;
        fs::rename(&current, &retired)?;
        fs::create_dir(&current)?;
        let old = lease_file(root.path(), &identity(&retired)?)?;
        assert!(matches!(old.try_lock(), Err(fs::TryLockError::WouldBlock)));
        let new = lease_file(root.path(), &identity(&current)?)?;
        new.try_lock()?;
        drop(lease);
        old.try_lock()?;
        Ok(())
    }
}
