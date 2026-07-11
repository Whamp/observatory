use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

use rustix::fs::{Mode, OFlags, open, openat};

use crate::error::AppError;

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

fn safe_open_error(path: &Path, error: rustix::io::Errno) -> AppError {
    AppError::usage(format!("cannot safely open {}: {error}", path.display()))
}
