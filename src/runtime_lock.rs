use std::env;
use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use rustix::fs::{FlockOperation, Mode, OFlags, fchmod, flock, mkdirat, open, openat};
use rustix::process::getuid;

use crate::error::AppError;

pub struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    pub fn acquire() -> Result<Self, AppError> {
        let root = runtime_root()?;
        let root_descriptor = open(
            &root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| AppError::usage(format!("cannot safely open XDG_RUNTIME_DIR: {error}")))?;
        let root_directory = File::from(root_descriptor);
        verify_private_directory(&root_directory, "XDG_RUNTIME_DIR")?;

        match mkdirat(&root_directory, "observatory", Mode::RWXU) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::EXIST => {}
            Err(error) => {
                return Err(AppError::internal(format!(
                    "cannot create runtime authority directory: {error}"
                )));
            }
        }
        let authority_descriptor = openat(
            &root_directory,
            "observatory",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| {
            AppError::usage(format!(
                "runtime authority must be a non-symlink directory: {error}"
            ))
        })?;
        let authority_directory = File::from(authority_descriptor);
        verify_private_directory(&authority_directory, "runtime authority")?;
        fchmod(&authority_directory, Mode::RWXU).map_err(|error| {
            AppError::internal(format!("cannot protect runtime authority: {error}"))
        })?;

        let lock_descriptor = openat(
            &authority_directory,
            "daemon.lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| AppError::internal(format!("cannot open daemon lock: {error}")))?;
        let file = File::from(lock_descriptor);
        let metadata = file
            .metadata()
            .map_err(|error| AppError::internal(format!("cannot inspect daemon lock: {error}")))?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != getuid().as_raw()
            || metadata.nlink() != 1
        {
            return Err(AppError::usage(
                "daemon lock must be one user-owned regular file",
            ));
        }
        fchmod(&file, Mode::RUSR | Mode::WUSR)
            .map_err(|error| AppError::internal(format!("cannot protect daemon lock: {error}")))?;
        flock(&file, FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| AppError::already_running())?;
        Ok(Self { _file: file })
    }
}

fn runtime_root() -> Result<PathBuf, AppError> {
    let root = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| AppError::usage("XDG_RUNTIME_DIR is required for obs serve"))?;
    if !root.is_absolute() {
        return Err(AppError::usage("XDG_RUNTIME_DIR must be absolute"));
    }
    Ok(root)
}

fn verify_private_directory(directory: &File, description: &str) -> Result<(), AppError> {
    let metadata = directory
        .metadata()
        .map_err(|error| AppError::usage(format!("cannot inspect {description}: {error}")))?;
    if !metadata.is_dir()
        || metadata.uid() != getuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(AppError::usage(format!(
            "{description} must be a user-owned protected directory"
        )));
    }
    Ok(())
}
