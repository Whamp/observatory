use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, statat};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::AppError;
use crate::safe_file::{SafeRegularFile, open_directory, open_regular_file};

const PORTABLE_METADATA: &str = ".obs.json";

pub(crate) enum ArtifactSource {
    File(SafeRegularFile),
    Directory(DirectorySource),
}

pub(crate) struct DirectorySource {
    root: File,
    root_snapshot: SourceSnapshot,
    basename: String,
    members: Vec<SourceMember>,
    files: u64,
    logical_bytes: u64,
    inventory: BTreeMap<String, SourceSnapshot>,
    metadata: Option<PortableMetadata>,
}

pub(crate) struct SourceWarning {
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
    pub(crate) member: String,
}

pub(crate) struct SourceMember {
    path: String,
    file: File,
    snapshot: SourceSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceSnapshot {
    device: u64,
    inode: u64,
    links: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: u64,
    changed_seconds: i64,
    changed_nanoseconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PortableMetadata {
    schema_version: u8,
    entry: Option<String>,
    title: Option<String>,
    description: Option<String>,
}

impl ArtifactSource {
    pub(crate) fn open(path: &Path) -> Result<Self, AppError> {
        match open_directory(path) {
            Ok(root) => {
                let basename = path.file_name().and_then(OsStr::to_str).ok_or_else(|| {
                    AppError::invalid(
                        "invalid_source",
                        "Artifact directory name must be valid UTF-8",
                    )
                })?;
                DirectorySource::open(root, basename).map(Self::Directory)
            }
            Err(_) => open_regular_file(path, "Artifact source").map(Self::File),
        }
    }

    pub(crate) fn entry_path(&self, explicit: Option<&str>) -> Result<String, AppError> {
        match self {
            Self::File(source) => {
                let name = source.file_name().to_str().ok_or_else(|| {
                    AppError::invalid(
                        "invalid_source",
                        "Artifact source filename must be valid UTF-8",
                    )
                })?;
                if explicit.is_some_and(|entry| entry != name) {
                    return Err(AppError::invalid(
                        "invalid_entry",
                        "a single-file Artifact entry must name the selected source file",
                    ));
                }
                validate_relative_member_path(name)?;
                Ok(name.to_owned())
            }
            Self::Directory(source) => source.entry_path(explicit),
        }
    }

    pub(crate) fn files(&self) -> u64 {
        match self {
            Self::File(_) => 1,
            Self::Directory(source) => source.files,
        }
    }

    pub(crate) fn logical_bytes(&self) -> u64 {
        match self {
            Self::File(source) => source.size(),
            Self::Directory(source) => source.logical_bytes,
        }
    }

    pub(crate) fn snapshot_digest(&self) -> String {
        match self {
            Self::File(source) => source.snapshot_digest(),
            Self::Directory(source) => source.snapshot_digest(),
        }
    }

    pub(crate) fn root_snapshot_digest(&self) -> Option<String> {
        match self {
            Self::File(_) => None,
            Self::Directory(source) => Some(source.root_snapshot_digest()),
        }
    }

    pub(crate) fn verify_unchanged(&self) -> Result<(), AppError> {
        match self {
            Self::File(source) => source.verify_unchanged(),
            Self::Directory(source) => source.verify_unchanged(),
        }
    }

    pub(crate) fn portable_title(&self) -> Option<&str> {
        match self {
            Self::File(_) => None,
            Self::Directory(source) => source
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.title.as_deref()),
        }
    }

    pub(crate) fn portable_description(&self) -> Option<&str> {
        match self {
            Self::File(_) => None,
            Self::Directory(source) => source
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.description.as_deref()),
        }
    }

    pub(crate) fn source_basename(&self) -> Result<&str, AppError> {
        match self {
            Self::File(source) => source.file_name().to_str().ok_or_else(|| {
                AppError::invalid("invalid_source", "Artifact source name must be valid UTF-8")
            }),
            Self::Directory(source) => Ok(&source.basename),
        }
    }

    pub(crate) fn warnings(&mut self) -> Result<Vec<SourceWarning>, AppError> {
        let mut warnings = Vec::new();
        match self {
            Self::File(source) => {
                let bytes = source.read_prefix(1_048_576)?;
                if contains_root_relative_reference(&bytes) {
                    warnings.push(SourceWarning {
                        code: "root_relative_reference",
                        message: "root-relative references are unsupported by portable Artifacts",
                        member: source.file_name().to_string_lossy().into_owned(),
                    });
                }
            }
            Self::Directory(source) => {
                for member in &mut source.members {
                    let bytes = member.read_prefix(1_048_576)?;
                    if contains_root_relative_reference(&bytes) {
                        warnings.push(SourceWarning {
                            code: "root_relative_reference",
                            message: "root-relative references are unsupported by portable Artifacts",
                            member: member.path.clone(),
                        });
                    }
                }
            }
        }
        Ok(warnings)
    }

    pub(crate) fn read_entry_prefix(
        &mut self,
        entry_path: &str,
        limit: usize,
    ) -> Result<Vec<u8>, AppError> {
        match self {
            Self::File(source) => source.read_prefix(limit as u64),
            Self::Directory(source) => source.read_member_prefix(entry_path, limit),
        }
    }
}

impl DirectorySource {
    fn open(root: File, basename: &str) -> Result<Self, AppError> {
        let root_metadata = root.metadata().map_err(|error| source_error(&error))?;
        let root_device = root_metadata.dev();
        let root_snapshot = snapshot_from_metadata(&root_metadata, true)?;
        let mut members = Vec::new();
        let mut inventory = BTreeMap::new();
        walk_directory(&root, "", root_device, &mut members, &mut inventory)?;
        let metadata = read_portable_metadata(&mut members)?;
        members.retain(|member| member.path != PORTABLE_METADATA);
        if members.is_empty() {
            return Err(AppError::invalid(
                "invalid_source",
                "Artifact directory contains no publishable regular files",
            ));
        }
        let files = u64::try_from(members.len())
            .map_err(|_| AppError::internal("Artifact member count overflow"))?;
        let logical_bytes = members.iter().try_fold(0_u64, |total, member| {
            total
                .checked_add(member.snapshot.size)
                .ok_or_else(|| AppError::internal("Artifact logical size overflow"))
        })?;
        Ok(Self {
            root,
            root_snapshot,
            basename: basename.to_owned(),
            members,
            files,
            logical_bytes,
            inventory,
            metadata,
        })
    }

    fn entry_path(&self, explicit: Option<&str>) -> Result<String, AppError> {
        let selected = explicit
            .map(str::to_owned)
            .or_else(|| {
                self.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.entry.clone())
            })
            .or_else(|| {
                self.has_member("index.html")
                    .then(|| "index.html".to_owned())
            })
            .ok_or_else(|| {
                AppError::invalid(
                    "invalid_entry",
                    "Artifact directory requires --entry, .obs.json entry, or root index.html",
                )
            })?;
        validate_relative_member_path(&selected)?;
        if !self.has_member(&selected) {
            return Err(AppError::invalid(
                "invalid_entry",
                "Artifact entry does not name a regular file in the selected directory",
            ));
        }
        Ok(selected)
    }

    fn has_member(&self, path: &str) -> bool {
        self.members.iter().any(|member| member.path == path)
    }

    fn read_member_prefix(&mut self, path: &str, limit: usize) -> Result<Vec<u8>, AppError> {
        let member = self
            .members
            .iter_mut()
            .find(|member| member.path == path)
            .ok_or_else(|| AppError::invalid("invalid_entry", "Artifact entry is missing"))?;
        member
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|error| source_error(&error))?;
        let mut bytes = Vec::new();
        member
            .file
            .by_ref()
            .take(limit as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| source_error(&error))?;
        member
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|error| source_error(&error))?;
        Ok(bytes)
    }

    fn snapshot_digest(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"observatory-directory-snapshot-v1\0");
        for (path, snapshot) in &self.inventory {
            digest.update(path.as_bytes());
            digest.update([0]);
            update_snapshot_digest(&mut digest, snapshot);
        }
        format!("sha256:{:x}", digest.finalize())
    }

    fn root_snapshot_digest(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"observatory-directory-root-snapshot-v1\0");
        update_snapshot_digest(&mut digest, &self.root_snapshot);
        format!("sha256:{:x}", digest.finalize())
    }

    pub(crate) fn verify_unchanged(&self) -> Result<(), AppError> {
        let mut members = Vec::new();
        let mut inventory = BTreeMap::new();
        let root_metadata = self.root.metadata().map_err(|error| source_error(&error))?;
        let root_snapshot = snapshot_from_metadata(&root_metadata, true)?;
        if root_snapshot != self.root_snapshot {
            return Err(AppError::source_changed());
        }
        let root_device = root_metadata.dev();
        if walk_directory(&self.root, "", root_device, &mut members, &mut inventory).is_err() {
            return Err(AppError::source_changed());
        }
        if inventory == self.inventory {
            Ok(())
        } else {
            Err(AppError::source_changed())
        }
    }

    pub(crate) fn members_mut(&mut self) -> &mut [SourceMember] {
        &mut self.members
    }
}

impl SourceMember {
    fn read_prefix(&mut self, limit: usize) -> Result<Vec<u8>, AppError> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| source_error(&error))?;
        let mut bytes = Vec::new();
        self.file
            .by_ref()
            .take(limit as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| source_error(&error))?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| source_error(&error))?;
        Ok(bytes)
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn size(&self) -> u64 {
        self.snapshot.size
    }

    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

fn walk_directory(
    directory: &File,
    prefix: &str,
    root_device: u64,
    members: &mut Vec<SourceMember>,
    inventory: &mut BTreeMap<String, SourceSnapshot>,
) -> Result<(), AppError> {
    let iterator_fd = openat(directory, ".", directory_flags(), Mode::empty())
        .map_err(|error| source_boundary_error("open Artifact directory", error))?;
    let entries = Dir::new(iterator_fd)
        .map_err(|error| source_boundary_error("read Artifact directory", error))?;
    let mut names = entries
        .filter_map(|entry| match entry {
            Ok(entry) if matches!(entry.file_name().to_bytes(), b"." | b"..") => None,
            Ok(entry) => Some(Ok(
                OsStr::from_bytes(entry.file_name().to_bytes()).to_owned()
            )),
            Err(error) => Some(Err(source_boundary_error("read Artifact member", error))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    for name in names {
        let name = name.to_str().ok_or_else(|| {
            AppError::invalid(
                "invalid_source",
                "Artifact member names must be valid UTF-8",
            )
        })?;
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        validate_relative_member_path(&path)?;
        let stat = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| source_boundary_error("inspect Artifact member", error))?;
        if stat.st_dev != root_device {
            return Err(AppError::invalid(
                "unsafe_source",
                "Artifact directory cannot cross a filesystem boundary",
            ));
        }
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                inventory.insert(path.clone(), snapshot_from_stat(&stat));
                let child = File::from(
                    openat(directory, name, directory_flags(), Mode::empty())
                        .map_err(|error| source_boundary_error("open Artifact directory", error))?,
                );
                walk_directory(&child, &path, root_device, members, inventory)?;
            }
            FileType::RegularFile => {
                if stat.st_nlink != 1 {
                    return Err(AppError::invalid(
                        "unsafe_source",
                        "Artifact members must not have multiple hard links",
                    ));
                }
                let file = File::from(
                    openat(
                        directory,
                        name,
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|error| source_boundary_error("open Artifact member", error))?,
                );
                let snapshot = snapshot(&file)?;
                inventory.insert(path.clone(), snapshot.clone());
                members.push(SourceMember {
                    path,
                    file,
                    snapshot,
                });
            }
            _ => {
                return Err(AppError::invalid(
                    "unsafe_source",
                    "Artifact directories may contain only regular files and directories",
                ));
            }
        }
    }
    Ok(())
}

fn contains_root_relative_reference(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    [
        "src=\"/", "src='/", "href=\"/", "href='/", "url(/", "url(\"/", "url('/",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn read_portable_metadata(
    members: &mut [SourceMember],
) -> Result<Option<PortableMetadata>, AppError> {
    let Some(member) = members
        .iter_mut()
        .find(|member| member.path == PORTABLE_METADATA)
    else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    member
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| source_error(&error))?;
    member
        .file
        .read_to_end(&mut bytes)
        .map_err(|error| source_error(&error))?;
    member
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| source_error(&error))?;
    let metadata: PortableMetadata = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::invalid(
            "invalid_metadata",
            format!("root .obs.json is invalid: {error}"),
        )
    })?;
    if metadata.schema_version != 1 {
        return Err(AppError::invalid(
            "invalid_metadata",
            "root .obs.json schemaVersion must be 1",
        ));
    }
    if metadata.title.as_deref().is_some_and(str::is_empty) {
        return Err(AppError::invalid(
            "invalid_metadata",
            "portable title must be nonempty when supplied",
        ));
    }
    Ok(Some(metadata))
}

fn update_snapshot_digest(digest: &mut Sha256, snapshot: &SourceSnapshot) {
    for value in [
        snapshot.device,
        snapshot.inode,
        snapshot.links,
        snapshot.size,
        snapshot.modified_seconds.cast_unsigned(),
        snapshot.modified_nanoseconds,
        snapshot.changed_seconds.cast_unsigned(),
        snapshot.changed_nanoseconds,
    ] {
        digest.update(value.to_be_bytes());
    }
}

fn snapshot_from_metadata(
    metadata: &std::fs::Metadata,
    directory: bool,
) -> Result<SourceSnapshot, AppError> {
    if metadata.file_type().is_dir() != directory {
        return Err(AppError::source_changed());
    }
    Ok(SourceSnapshot {
        device: metadata.dev(),
        inode: metadata.ino(),
        links: metadata.nlink(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec().cast_unsigned(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec().cast_unsigned(),
    })
}

fn snapshot_from_stat(stat: &rustix::fs::Stat) -> SourceSnapshot {
    SourceSnapshot {
        device: stat.st_dev,
        inode: stat.st_ino,
        links: stat.st_nlink,
        size: stat.st_size.cast_unsigned(),
        modified_seconds: stat.st_mtime,
        modified_nanoseconds: stat.st_mtime_nsec,
        changed_seconds: stat.st_ctime,
        changed_nanoseconds: stat.st_ctime_nsec,
    }
}

fn snapshot(file: &File) -> Result<SourceSnapshot, AppError> {
    let metadata = file.metadata().map_err(|error| source_error(&error))?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(AppError::invalid(
            "unsafe_source",
            "Artifact member changed type or link count while opening",
        ));
    }
    snapshot_from_metadata(&metadata, false)
}

pub(crate) fn validate_relative_member_path(path: &str) -> Result<(), AppError> {
    let valid = !path.is_empty()
        && !path.starts_with('/')
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.contains(['\\', '\0'])
                && !segment.as_bytes().windows(3).any(|window| {
                    window[0] == b'%'
                        && window[1].is_ascii_hexdigit()
                        && window[2].is_ascii_hexdigit()
                })
        });
    if valid {
        Ok(())
    } else {
        Err(AppError::invalid(
            "invalid_source",
            "Artifact member path cannot be represented safely",
        ))
    }
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn source_error(error: &std::io::Error) -> AppError {
    AppError::usage(format!("cannot read Artifact source: {error}"))
}

fn source_boundary_error(action: &str, error: rustix::io::Errno) -> AppError {
    AppError::usage(format!("cannot {action}: {error}"))
}
