use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use rustix::fs::{Mode, OFlags, open, openat};
use sha2::{Digest, Sha256};

use crate::error::AppError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileSnapshot {
    device: u64,
    inode: u64,
    links: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Debug)]
pub(crate) struct SafeRegularFile {
    file: File,
    file_name: OsString,
    snapshot: FileSnapshot,
}

impl SafeRegularFile {
    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) fn file_name(&self) -> &std::ffi::OsStr {
        &self.file_name
    }

    pub(crate) fn read_prefix(&mut self, limit: u64) -> Result<Vec<u8>, AppError> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| AppError::usage(format!("cannot seek Artifact source: {error}")))?;
        let mut bytes = Vec::new();
        self.file
            .by_ref()
            .take(limit)
            .read_to_end(&mut bytes)
            .map_err(|error| AppError::usage(format!("cannot inspect Artifact source: {error}")))?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| AppError::usage(format!("cannot rewind Artifact source: {error}")))?;
        Ok(bytes)
    }

    pub(crate) const fn size(&self) -> u64 {
        self.snapshot.size
    }

    pub(crate) fn snapshot_digest(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"observatory-source-snapshot-v1\0");
        for value in [
            self.snapshot.device,
            self.snapshot.inode,
            self.snapshot.links,
            self.snapshot.size,
            self.snapshot.modified_seconds.cast_unsigned(),
            self.snapshot.modified_nanoseconds.cast_unsigned(),
            self.snapshot.changed_seconds.cast_unsigned(),
            self.snapshot.changed_nanoseconds.cast_unsigned(),
        ] {
            digest.update(value.to_be_bytes());
        }
        format!("sha256:{:x}", digest.finalize())
    }

    pub(crate) fn verify_unchanged(&self) -> Result<(), AppError> {
        let current = snapshot(&self.file)?;
        if current != self.snapshot {
            return Err(AppError::source_changed());
        }
        Ok(())
    }
}

/// Opens one directory without following any symbolic-link component.
pub fn open_directory(path: &Path) -> Result<File, AppError> {
    let (anchor, parts) = split_path(path)?;
    let mut directory = open(
        anchor,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| safe_open_error(path, error))?;
    for part in parts {
        directory = openat(
            &directory,
            part,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| safe_open_error(path, error))?;
    }
    Ok(File::from(directory))
}

pub(crate) fn open_regular_file(
    path: &Path,
    description: &str,
) -> Result<SafeRegularFile, AppError> {
    let (anchor, parts) = split_path(path)?;
    let mut directory = open(
        anchor,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| safe_open_error(path, error))?;
    let (file_name, parents) = parts
        .split_last()
        .ok_or_else(|| AppError::usage(format!("{description} must name one regular file")))?;
    for parent in parents {
        directory = openat(
            &directory,
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| safe_open_error(path, error))?;
    }
    let descriptor = openat(
        &directory,
        file_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| safe_open_error(path, error))?;
    let file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| AppError::usage(format!("cannot inspect {description}: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(AppError::invalid(
            "invalid_source",
            format!("{description} must be one regular file"),
        ));
    }
    let snapshot = snapshot_from_metadata(&metadata);
    if snapshot.links != 1 {
        return Err(AppError::invalid(
            "unsafe_source",
            format!("{description} must not have multiple hard links"),
        ));
    }
    Ok(SafeRegularFile {
        file,
        file_name: file_name.to_owned(),
        snapshot,
    })
}

/// Reads one regular UTF-8 file without following any symbolic-link component.
pub fn read_regular_utf8(path: &Path, description: &str) -> Result<String, AppError> {
    let (anchor, parts) = split_path(path)?;
    let mut directory = open(
        anchor,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| safe_open_error(path, error))?;
    let (file_name, parents) = parts
        .split_last()
        .ok_or_else(|| AppError::usage(format!("{description} must name one regular file")))?;
    for parent in parents {
        directory = openat(
            &directory,
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| safe_open_error(path, error))?;
    }
    let descriptor = openat(
        &directory,
        file_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| safe_open_error(path, error))?;
    let mut file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| AppError::usage(format!("cannot inspect {description}: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(AppError::usage(format!(
            "{description} must be one regular file"
        )));
    }
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|error| AppError::usage(format!("cannot read {description}: {error}")))?;
    Ok(content)
}

fn snapshot(file: &File) -> Result<FileSnapshot, AppError> {
    let metadata = file
        .metadata()
        .map_err(|error| AppError::usage(format!("cannot inspect opened source: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(AppError::source_changed());
    }
    Ok(snapshot_from_metadata(&metadata))
}

fn snapshot_from_metadata(metadata: &std::fs::Metadata) -> FileSnapshot {
    FileSnapshot {
        device: metadata.dev(),
        inode: metadata.ino(),
        links: metadata.nlink(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn split_path(path: &Path) -> Result<(&Path, Vec<OsString>), AppError> {
    let anchor = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_owned()),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(AppError::usage(
                    "file paths containing parent traversal are not accepted",
                ));
            }
        }
    }
    Ok((anchor, parts))
}

fn safe_open_error(_path: &Path, error: rustix::io::Errno) -> AppError {
    AppError::usage(format!("cannot safely open selected path: {error}"))
}
