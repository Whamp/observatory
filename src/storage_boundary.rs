use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use rustix::fs::{Dir, Mode, OFlags, RenameFlags, fsync, openat, renameat_with};

use crate::error::AppError;
use crate::safe_file::open_directory;

#[derive(Clone, Debug)]
pub(crate) struct StorageBoundary {
    root: Arc<File>,
}

impl StorageBoundary {
    pub(crate) fn open(root: &Path) -> Result<Self, AppError> {
        Ok(Self {
            root: Arc::new(open_directory(root)?),
        })
    }

    pub(crate) fn entry_names(&self, directory: &str) -> Result<Vec<OsString>, AppError> {
        let directory = self.directory(directory)?;
        let entries = Dir::new(directory)
            .map_err(|error| boundary_error("read private storage directory", error))?;
        entries
            .filter_map(|entry| match entry {
                Ok(entry) if entry.file_name().to_bytes() == b"." => None,
                Ok(entry) if entry.file_name().to_bytes() == b".." => None,
                Ok(entry) => Some(Ok(
                    OsStr::from_bytes(entry.file_name().to_bytes()).to_owned()
                )),
                Err(error) => Some(Err(boundary_error("read private storage entry", error))),
            })
            .collect()
    }

    pub(crate) fn is_directory(&self, directory: &str, name: &OsStr) -> Result<bool, AppError> {
        let directory = self.directory(directory)?;
        match openat(&directory, name, directory_flags(), Mode::empty()) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => Ok(false),
            Err(error) => Err(boundary_error("classify private storage entry", error)),
        }
    }

    pub(crate) fn quarantine_startup(
        &self,
        source_directory: &str,
        source_name: &OsStr,
    ) -> Result<(), AppError> {
        let source = self.directory(source_directory)?;
        let quarantine = self.directory("quarantine")?;
        for ordinal in 1_u64..=u64::MAX {
            let destination = format!("startup-{ordinal:016x}");
            match renameat_with(
                &source,
                source_name,
                &quarantine,
                destination,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => {
                    fsync(&source)
                        .and_then(|()| fsync(&quarantine))
                        .map_err(|error| boundary_error("sync startup quarantine", error))?;
                    return Ok(());
                }
                Err(rustix::io::Errno::EXIST) => {}
                Err(error) => {
                    return Err(boundary_error("quarantine startup evidence", error));
                }
            }
        }
        Err(AppError::internal(
            "cannot allocate startup quarantine name",
        ))
    }

    fn directory(&self, name: &str) -> Result<rustix::fd::OwnedFd, AppError> {
        openat(&self.root, name, directory_flags(), Mode::empty())
            .map_err(|error| boundary_error("open private storage directory", error))
    }
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn boundary_error(action: &str, error: rustix::io::Errno) -> AppError {
    AppError::internal(format!("cannot {action}: {error}"))
}
