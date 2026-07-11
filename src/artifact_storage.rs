use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, fsync, mkdirat, openat, renameat_with,
    statat,
};
use sha2::{Digest, Sha256};

use crate::artifact_source::{ArtifactSource, DirectorySource, validate_relative_member_path};
use crate::error::AppError;
use crate::revision_manifest::{
    CONTENT_DIRECTORY, MANIFEST_FILE, RevisionIdentity, RevisionManifest, RevisionManifestInput,
    RevisionMemberInput,
};
use crate::safe_file::{SafeRegularFile, open_directory};

#[derive(Clone, Debug)]
pub(crate) struct ArtifactStorage {
    root: Arc<File>,
}

#[derive(Clone, Debug)]
pub(crate) struct StagedRevision {
    operation_id: String,
    revision_id: String,
    entry_path: String,
    entry_media_type: String,
    files: u64,
    logical_bytes: u64,
    payload_digest: String,
    manifest_digest: String,
}

#[derive(Clone, Debug)]
pub(crate) struct FinalizedRevision {
    revision_id: String,
    entry_path: String,
    entry_media_type: String,
    files: u64,
    logical_bytes: u64,
    payload_digest: String,
    manifest_digest: String,
}

#[derive(Clone, Copy)]
pub(crate) struct RecoveryRequest<'a> {
    pub(crate) artifact_id: &'a str,
    pub(crate) revision_id: &'a str,
    pub(crate) entry_path: &'a str,
    pub(crate) entry_media_type: &'a str,
    pub(crate) files: u64,
    pub(crate) logical_bytes: u64,
    pub(crate) published_at: &'a str,
    pub(crate) payload_digest: Option<&'a str>,
    pub(crate) manifest_digest: Option<&'a str>,
}

#[derive(Clone, Copy)]
pub(crate) struct StageRequest<'a> {
    pub(crate) operation_id: &'a str,
    pub(crate) artifact_id: &'a str,
    pub(crate) revision_id: &'a str,
    pub(crate) entry_path: &'a str,
    pub(crate) entry_media_type: &'a str,
    pub(crate) published_at: &'a str,
}

impl StagedRevision {
    pub(crate) fn revision_id(&self) -> &str {
        &self.revision_id
    }

    pub(crate) fn entry_path(&self) -> &str {
        &self.entry_path
    }

    pub(crate) fn entry_media_type(&self) -> &str {
        &self.entry_media_type
    }

    pub(crate) const fn files(&self) -> u64 {
        self.files
    }

    pub(crate) const fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    pub(crate) fn payload_digest(&self) -> &str {
        &self.payload_digest
    }

    pub(crate) fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }
}

impl FinalizedRevision {
    pub(crate) fn revision_id(&self) -> &str {
        &self.revision_id
    }

    pub(crate) fn entry_path(&self) -> &str {
        &self.entry_path
    }

    pub(crate) fn entry_media_type(&self) -> &str {
        &self.entry_media_type
    }

    pub(crate) const fn files(&self) -> u64 {
        self.files
    }

    pub(crate) const fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    pub(crate) fn payload_digest(&self) -> &str {
        &self.payload_digest
    }

    pub(crate) fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }
}

impl ArtifactStorage {
    pub(crate) fn open(root: &std::path::Path) -> Result<Self, AppError> {
        Ok(Self {
            root: Arc::new(open_directory(root)?),
        })
    }

    pub(crate) fn stage_source(
        &self,
        source: &mut ArtifactSource,
        request: StageRequest<'_>,
    ) -> Result<StagedRevision, AppError> {
        match source {
            ArtifactSource::File(source) => self.stage_single_file(source, request),
            ArtifactSource::Directory(source) => self.stage_directory(source, request),
        }
    }

    pub(crate) fn stage_single_file(
        &self,
        source: &mut SafeRegularFile,
        request: StageRequest<'_>,
    ) -> Result<StagedRevision, AppError> {
        validate_storage_identifier(request.operation_id)?;
        validate_storage_identifier(request.artifact_id)?;
        validate_storage_identifier(request.revision_id)?;
        validate_entry_path(request.entry_path)?;
        let staging = self.open_directory("staging")?;
        mkdirat(&staging, request.operation_id, Mode::from_raw_mode(0o700))
            .map_err(|error| storage_error("create Publish staging directory", error))?;
        let operation = openat(
            &staging,
            request.operation_id,
            directory_flags(),
            Mode::empty(),
        )
        .map_err(|error| storage_error("open Publish staging directory", error))?;
        mkdirat(&operation, CONTENT_DIRECTORY, Mode::from_raw_mode(0o700))
            .map_err(|error| storage_error("create Revision content directory", error))?;
        let content = openat(
            &operation,
            CONTENT_DIRECTORY,
            directory_flags(),
            Mode::empty(),
        )
        .map_err(|error| storage_error("open Revision content directory", error))?;
        let mut payload = File::from(
            openat(
                &content,
                request.entry_path,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|error| storage_error("create staged Revision member", error))?,
        );
        let expected_bytes = source.size();
        let (logical_bytes, payload_digest) =
            copy_and_hash(source.file_mut(), &mut payload, expected_bytes)?;
        storage_crash_fault(
            "OBS_TEST_CRASH_PUBLISH_AFTER_COPY_DIGEST",
            "payload copy and digest",
        );
        payload.sync_all().map_err(|error| {
            AppError::internal(format!("cannot sync staged Revision member: {error}"))
        })?;
        storage_crash_fault("OBS_TEST_CRASH_PUBLISH_AFTER_PAYLOAD_SYNC", "payload sync");
        source.verify_unchanged()?;

        let manifest = RevisionManifest::new(RevisionManifestInput {
            artifact_id: request.artifact_id,
            revision_id: request.revision_id,
            entry_path: request.entry_path,
            entry_media_type: request.entry_media_type,
            published_at: request.published_at,
            members: vec![RevisionMemberInput {
                path: request.entry_path.to_owned(),
                size: logical_bytes,
                digest: payload_digest,
            }],
        })?;
        let (payload_digest, manifest_digest) =
            persist_manifest(&operation, &content, &staging, &manifest)?;
        Ok(StagedRevision {
            operation_id: request.operation_id.to_owned(),
            revision_id: request.revision_id.to_owned(),
            entry_path: request.entry_path.to_owned(),
            entry_media_type: request.entry_media_type.to_owned(),
            files: 1,
            logical_bytes,
            payload_digest,
            manifest_digest,
        })
    }

    fn stage_directory(
        &self,
        source: &mut DirectorySource,
        request: StageRequest<'_>,
    ) -> Result<StagedRevision, AppError> {
        validate_storage_identifier(request.operation_id)?;
        validate_storage_identifier(request.artifact_id)?;
        validate_storage_identifier(request.revision_id)?;
        validate_entry_path(request.entry_path)?;
        let staging = self.open_directory("staging")?;
        mkdirat(&staging, request.operation_id, Mode::from_raw_mode(0o700))
            .map_err(|error| storage_error("create Publish staging directory", error))?;
        let operation = openat(
            &staging,
            request.operation_id,
            directory_flags(),
            Mode::empty(),
        )
        .map_err(|error| storage_error("open Publish staging directory", error))?;
        mkdirat(&operation, CONTENT_DIRECTORY, Mode::from_raw_mode(0o700))
            .map_err(|error| storage_error("create Revision content directory", error))?;
        let content = openat(
            &operation,
            CONTENT_DIRECTORY,
            directory_flags(),
            Mode::empty(),
        )
        .map_err(|error| storage_error("open Revision content directory", error))?;
        let mut manifest_members = Vec::new();
        for member in source.members_mut() {
            let mut destination = create_staged_member(&content, member.path())?;
            let expected_size = member.size();
            let (size, digest) = copy_and_hash(member.file_mut(), &mut destination, expected_size)?;
            storage_crash_fault(
                "OBS_TEST_CRASH_PUBLISH_AFTER_COPY_DIGEST",
                "bundle member copy and digest",
            );
            destination.sync_all().map_err(|error| {
                AppError::internal(format!("cannot sync staged Revision member: {error}"))
            })?;
            storage_crash_fault(
                "OBS_TEST_CRASH_PUBLISH_AFTER_PAYLOAD_SYNC",
                "bundle member sync",
            );
            manifest_members.push(RevisionMemberInput {
                path: member.path().to_owned(),
                size,
                digest,
            });
        }
        source.verify_unchanged()?;
        let manifest = RevisionManifest::new(RevisionManifestInput {
            artifact_id: request.artifact_id,
            revision_id: request.revision_id,
            entry_path: request.entry_path,
            entry_media_type: request.entry_media_type,
            published_at: request.published_at,
            members: manifest_members,
        })?;
        let logical_bytes = manifest.logical_bytes();
        let files = manifest.files();
        let (payload_digest, manifest_digest) =
            persist_manifest(&operation, &content, &staging, &manifest)?;
        Ok(StagedRevision {
            operation_id: request.operation_id.to_owned(),
            revision_id: request.revision_id.to_owned(),
            entry_path: request.entry_path.to_owned(),
            entry_media_type: request.entry_media_type.to_owned(),
            files,
            logical_bytes,
            payload_digest,
            manifest_digest,
        })
    }

    pub(crate) fn finalize(&self, staged: StagedRevision) -> Result<FinalizedRevision, AppError> {
        validate_storage_identifier(&staged.operation_id)?;
        validate_storage_identifier(&staged.revision_id)?;
        let staging = self.open_directory("staging")?;
        let revisions = self.open_directory("revisions")?;
        renameat_with(
            &staging,
            staged.operation_id.as_str(),
            &revisions,
            staged.revision_id.as_str(),
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| storage_error("finalize immutable Revision", error))?;
        storage_crash_fault(
            "OBS_TEST_CRASH_PUBLISH_AFTER_STORAGE_RENAME",
            "Revision rename",
        );
        fsync(&staging).map_err(|error| storage_error("sync staging after rename", error))?;
        storage_crash_fault(
            "OBS_TEST_CRASH_PUBLISH_AFTER_RENAME_STAGING_SYNC",
            "staging parent sync after rename",
        );
        fsync(&revisions).map_err(|error| storage_error("sync Revisions after rename", error))?;
        storage_crash_fault(
            "OBS_TEST_CRASH_PUBLISH_AFTER_RENAME_REVISIONS_SYNC",
            "Revisions parent sync after rename",
        );
        Ok(FinalizedRevision {
            revision_id: staged.revision_id,
            entry_path: staged.entry_path,
            entry_media_type: staged.entry_media_type,
            files: staged.files,
            logical_bytes: staged.logical_bytes,
            payload_digest: staged.payload_digest,
            manifest_digest: staged.manifest_digest,
        })
    }

    pub(crate) fn verify_finalized(
        &self,
        expected: RecoveryRequest<'_>,
    ) -> Result<FinalizedRevision, AppError> {
        validate_storage_identifier(expected.artifact_id)?;
        validate_storage_identifier(expected.revision_id)?;
        validate_entry_path(expected.entry_path)?;
        let revisions = self.open_directory("revisions")?;
        let revision = openat(
            &revisions,
            expected.revision_id,
            directory_flags(),
            Mode::empty(),
        )
        .map_err(|error| storage_error("open recoverable Revision", error))?;
        let verified = verify_revision_directory(&revision, expected)?;
        Ok(FinalizedRevision {
            revision_id: verified.revision_id,
            entry_path: verified.entry_path,
            entry_media_type: verified.entry_media_type,
            files: verified.files,
            logical_bytes: verified.logical_bytes,
            payload_digest: verified.payload_digest,
            manifest_digest: verified.manifest_digest,
        })
    }

    pub(crate) fn verify_staged(
        &self,
        operation_id: &str,
        expected: RecoveryRequest<'_>,
    ) -> Result<StagedRevision, AppError> {
        validate_storage_identifier(operation_id)?;
        validate_storage_identifier(expected.artifact_id)?;
        validate_storage_identifier(expected.revision_id)?;
        validate_entry_path(expected.entry_path)?;
        let staging = self.open_directory("staging")?;
        let operation = openat(&staging, operation_id, directory_flags(), Mode::empty())
            .map_err(|error| storage_error("open recoverable Publish staging", error))?;
        let verified = verify_revision_directory(&operation, expected)?;
        Ok(StagedRevision {
            operation_id: operation_id.to_owned(),
            revision_id: verified.revision_id,
            entry_path: verified.entry_path,
            entry_media_type: verified.entry_media_type,
            files: verified.files,
            logical_bytes: verified.logical_bytes,
            payload_digest: verified.payload_digest,
            manifest_digest: verified.manifest_digest,
        })
    }

    pub(crate) fn has_interrupted_bytes(
        &self,
        operation_id: &str,
        revision_id: &str,
    ) -> Result<bool, AppError> {
        validate_storage_identifier(operation_id)?;
        validate_storage_identifier(revision_id)?;
        let staging = self.open_directory("staging")?;
        let revisions = self.open_directory("revisions")?;
        Ok(entry_exists(&staging, operation_id)? || entry_exists(&revisions, revision_id)?)
    }

    pub(crate) fn quarantine_interrupted(
        &self,
        operation_id: &str,
        revision_id: &str,
    ) -> Result<(), AppError> {
        validate_storage_identifier(operation_id)?;
        validate_storage_identifier(revision_id)?;
        let quarantine = self.open_directory("quarantine")?;
        let staging = self.open_directory("staging")?;
        let revisions = self.open_directory("revisions")?;
        quarantine_if_present(
            &staging,
            operation_id,
            &quarantine,
            &format!("publish-{operation_id}-staging"),
        )?;
        quarantine_if_present(
            &revisions,
            revision_id,
            &quarantine,
            &format!("publish-{operation_id}-revision"),
        )?;
        fsync(&staging)
            .and_then(|()| fsync(&revisions))
            .and_then(|()| fsync(&quarantine))
            .map_err(|error| storage_error("sync Publish quarantine directories", error))
    }

    pub(crate) fn read_revision_member(
        &self,
        revision_id: &str,
        entry_path: &str,
        expected_manifest_digest: &str,
    ) -> Result<Vec<u8>, AppError> {
        validate_storage_identifier(revision_id)?;
        validate_entry_path(entry_path)?;
        let revisions = self.open_directory("revisions")?;
        let revision = openat(&revisions, revision_id, directory_flags(), Mode::empty())
            .map_err(|error| storage_error("open immutable Revision", error))?;
        let (manifest, manifest_digest) = read_revision_manifest(&revision)?;
        if manifest_digest != expected_manifest_digest {
            return Err(AppError::internal(
                "immutable Revision manifest digest mismatch",
            ));
        }
        let member = manifest
            .member(entry_path)
            .ok_or_else(|| AppError::not_found("Artifact member does not exist"))?;
        let content = openat(
            &revision,
            CONTENT_DIRECTORY,
            directory_flags(),
            Mode::empty(),
        )
        .map_err(|error| storage_error("open immutable Revision content", error))?;
        let mut file = open_content_member(&content, entry_path)?;
        let metadata = file.metadata().map_err(|error| {
            AppError::internal(format!("cannot inspect Revision member: {error}"))
        })?;
        if !metadata.file_type().is_file() {
            return Err(AppError::internal("Revision member is not a regular file"));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| AppError::internal(format!("cannot read Revision member: {error}")))?;
        let size = u64::try_from(bytes.len())
            .map_err(|_| AppError::internal("Revision member size overflow"))?;
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        if size != member.size() || digest != member.digest() {
            return Err(AppError::internal(
                "immutable Revision member integrity mismatch",
            ));
        }
        Ok(bytes)
    }

    fn open_directory(&self, name: &str) -> Result<rustix::fd::OwnedFd, AppError> {
        openat(&self.root, name, directory_flags(), Mode::empty())
            .map_err(|error| storage_error("open Observatory storage directory", error))
    }
}

struct VerifiedRevision {
    revision_id: String,
    entry_path: String,
    entry_media_type: String,
    files: u64,
    logical_bytes: u64,
    payload_digest: String,
    manifest_digest: String,
}

fn read_revision_manifest(
    revision: &rustix::fd::OwnedFd,
) -> Result<(RevisionManifest, String), AppError> {
    let mut file = File::from(
        openat(
            revision,
            MANIFEST_FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| storage_error("open Revision manifest", error))?,
    );
    let metadata = file.metadata().map_err(|error| {
        AppError::internal(format!("cannot inspect Revision manifest: {error}"))
    })?;
    if !metadata.file_type().is_file() {
        return Err(AppError::internal(
            "Revision manifest is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| AppError::internal(format!("cannot read Revision manifest: {error}")))?;
    let digest = RevisionManifest::digest(&bytes);
    let manifest: RevisionManifest = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::internal(format!("Revision manifest is invalid: {error}")))?;
    if manifest.canonical_bytes()? != bytes {
        return Err(AppError::internal("Revision manifest is not canonical"));
    }
    Ok((manifest, digest))
}

fn verify_revision_directory(
    revision: &rustix::fd::OwnedFd,
    expected: RecoveryRequest<'_>,
) -> Result<VerifiedRevision, AppError> {
    let mut manifest_file = File::from(
        openat(
            revision,
            MANIFEST_FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| storage_error("open recoverable Revision manifest", error))?,
    );
    if !manifest_file
        .metadata()
        .map_err(|error| AppError::internal(format!("cannot inspect Revision manifest: {error}")))?
        .file_type()
        .is_file()
    {
        return Err(AppError::internal(
            "Revision manifest is not a regular file",
        ));
    }
    let mut manifest_bytes = Vec::new();
    manifest_file
        .read_to_end(&mut manifest_bytes)
        .map_err(|error| AppError::internal(format!("cannot read Revision manifest: {error}")))?;
    let manifest_digest = RevisionManifest::digest(&manifest_bytes);
    if expected
        .manifest_digest
        .is_some_and(|digest| digest != manifest_digest)
    {
        return Err(AppError::internal(
            "recoverable Revision manifest digest mismatch",
        ));
    }
    let manifest: RevisionManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        AppError::internal(format!("recoverable Revision manifest is invalid: {error}"))
    })?;
    if manifest.canonical_bytes()? != manifest_bytes
        || !manifest.identity_matches(RevisionIdentity {
            artifact_id: expected.artifact_id,
            revision_id: expected.revision_id,
            entry_path: expected.entry_path,
            entry_media_type: expected.entry_media_type,
            logical_bytes: expected.logical_bytes,
            published_at: expected.published_at,
        })
        || manifest.files() != expected.files
    {
        return Err(AppError::internal(
            "recoverable Revision manifest does not match intent",
        ));
    }
    let content = openat(
        revision,
        CONTENT_DIRECTORY,
        directory_flags(),
        Mode::empty(),
    )
    .map_err(|error| storage_error("open recoverable Revision content", error))?;
    let payload_digest = verify_revision_content(&content, &manifest, expected.payload_digest)?;
    Ok(VerifiedRevision {
        revision_id: expected.revision_id.to_owned(),
        entry_path: expected.entry_path.to_owned(),
        entry_media_type: expected.entry_media_type.to_owned(),
        files: expected.files,
        logical_bytes: expected.logical_bytes,
        payload_digest,
        manifest_digest,
    })
}

fn verify_revision_content(
    content: &rustix::fd::OwnedFd,
    manifest: &RevisionManifest,
    expected_digest: Option<&str>,
) -> Result<String, AppError> {
    let actual_members = content_member_paths(content, "")?;
    let expected_members = manifest
        .members()
        .iter()
        .map(|member| member.path().to_owned())
        .collect::<Vec<_>>();
    if actual_members != expected_members {
        return Err(AppError::internal(
            "recoverable Revision content inventory does not match its manifest",
        ));
    }
    for member in manifest.members() {
        validate_entry_path(member.path())?;
        let mut payload = open_content_member(content, member.path())?;
        let metadata = payload.metadata().map_err(|error| {
            AppError::internal(format!("cannot inspect Revision member: {error}"))
        })?;
        if !metadata.file_type().is_file() || metadata.len() != member.size() {
            return Err(AppError::internal(
                "recoverable Revision member metadata mismatch",
            ));
        }
        if hash_file(&mut payload)? != member.digest() {
            return Err(AppError::internal(
                "recoverable Revision member digest mismatch",
            ));
        }
    }
    let digest = manifest.content_digest()?;
    if expected_digest.is_some_and(|expected| expected != digest) {
        return Err(AppError::internal(
            "recoverable Revision content inventory mismatch",
        ));
    }
    Ok(digest)
}

fn persist_manifest(
    operation: &rustix::fd::OwnedFd,
    content: &rustix::fd::OwnedFd,
    staging: &rustix::fd::OwnedFd,
    manifest: &RevisionManifest,
) -> Result<(String, String), AppError> {
    let content_digest = manifest.content_digest()?;
    let manifest_bytes = manifest.canonical_bytes()?;
    let manifest_digest = RevisionManifest::digest(&manifest_bytes);
    let mut file = File::from(
        openat(
            operation,
            MANIFEST_FILE,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| storage_error("create Revision manifest", error))?,
    );
    file.write_all(&manifest_bytes)
        .map_err(|error| AppError::internal(format!("cannot write Revision manifest: {error}")))?;
    storage_crash_fault(
        "OBS_TEST_CRASH_PUBLISH_AFTER_MANIFEST_WRITE",
        "manifest write",
    );
    file.sync_all()
        .map_err(|error| AppError::internal(format!("cannot sync Revision manifest: {error}")))?;
    storage_crash_fault(
        "OBS_TEST_CRASH_PUBLISH_AFTER_MANIFEST_SYNC",
        "manifest sync",
    );
    sync_content_directories(content)?;
    storage_crash_fault(
        "OBS_TEST_CRASH_PUBLISH_AFTER_CONTENT_SYNC",
        "content directory sync",
    );
    fsync(operation).map_err(|error| storage_error("sync Publish operation directory", error))?;
    storage_crash_fault(
        "OBS_TEST_CRASH_PUBLISH_AFTER_OPERATION_SYNC",
        "operation directory sync",
    );
    fsync(staging).map_err(|error| storage_error("sync staging directory", error))?;
    storage_crash_fault(
        "OBS_TEST_CRASH_PUBLISH_AFTER_STAGING_SYNC",
        "staging directory sync",
    );
    Ok((content_digest, manifest_digest))
}

fn open_content_member(content: &rustix::fd::OwnedFd, path: &str) -> Result<File, AppError> {
    validate_relative_member_path(path)?;
    let mut parts = path.split('/').collect::<Vec<_>>();
    let file_name = parts
        .pop()
        .ok_or_else(|| AppError::internal("Revision member path is empty"))?;
    let mut directory = openat(content, ".", directory_flags(), Mode::empty())
        .map_err(|error| storage_error("open Revision content directory", error))?;
    for part in parts {
        directory = openat(&directory, part, directory_flags(), Mode::empty())
            .map_err(revision_member_error)?;
    }
    Ok(File::from(
        openat(
            &directory,
            file_name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(revision_member_error)?,
    ))
}

fn create_staged_member(content: &rustix::fd::OwnedFd, path: &str) -> Result<File, AppError> {
    validate_relative_member_path(path)?;
    let mut parts = path.split('/').collect::<Vec<_>>();
    let file_name = parts
        .pop()
        .ok_or_else(|| AppError::internal("staged member path is empty"))?;
    let mut directory = openat(content, ".", directory_flags(), Mode::empty())
        .map_err(|error| storage_error("open staged content directory", error))?;
    for part in parts {
        match mkdirat(&directory, part, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(storage_error("create staged member directory", error)),
        }
        directory = openat(&directory, part, directory_flags(), Mode::empty())
            .map_err(|error| storage_error("open staged member directory", error))?;
    }
    Ok(File::from(
        openat(
            &directory,
            file_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| storage_error("create staged Revision member", error))?,
    ))
}

fn content_member_paths(
    directory: &rustix::fd::OwnedFd,
    prefix: &str,
) -> Result<Vec<String>, AppError> {
    let iterator = openat(directory, ".", directory_flags(), Mode::empty())
        .map_err(|error| storage_error("open Revision content inventory", error))?;
    let entries = Dir::new(iterator)
        .map_err(|error| storage_error("read Revision content inventory", error))?;
    let mut names = entries
        .filter_map(|entry| match entry {
            Ok(entry) if matches!(entry.file_name().to_bytes(), b"." | b"..") => None,
            Ok(entry) => Some(Ok(
                OsStr::from_bytes(entry.file_name().to_bytes()).to_owned()
            )),
            Err(error) => Some(Err(storage_error("read Revision content member", error))),
        })
        .collect::<Result<Vec<OsString>, AppError>>()?;
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let mut paths = Vec::new();
    for name in names {
        let name = name
            .to_str()
            .ok_or_else(|| AppError::internal("Revision member name is not UTF-8"))?;
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let stat = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| storage_error("inspect Revision content member", error))?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                let child = openat(directory, name, directory_flags(), Mode::empty())
                    .map_err(|error| storage_error("open Revision content directory", error))?;
                paths.extend(content_member_paths(&child, &path)?);
            }
            FileType::RegularFile => paths.push(path),
            _ => {
                return Err(AppError::internal(
                    "Revision content contains an unsafe member type",
                ));
            }
        }
    }
    Ok(paths)
}

fn sync_content_directories(directory: &rustix::fd::OwnedFd) -> Result<(), AppError> {
    let iterator = openat(directory, ".", directory_flags(), Mode::empty())
        .map_err(|error| storage_error("open staged directory for sync", error))?;
    let entries = Dir::new(iterator)
        .map_err(|error| storage_error("read staged directory for sync", error))?;
    let names = entries
        .filter_map(|entry| match entry {
            Ok(entry) if matches!(entry.file_name().to_bytes(), b"." | b"..") => None,
            Ok(entry) => Some(Ok(
                OsStr::from_bytes(entry.file_name().to_bytes()).to_owned()
            )),
            Err(error) => Some(Err(storage_error("read staged directory entry", error))),
        })
        .collect::<Result<Vec<OsString>, AppError>>()?;
    for name in names {
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| storage_error("inspect staged directory entry", error))?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let child = openat(directory, &name, directory_flags(), Mode::empty())
                .map_err(|error| storage_error("open staged child directory", error))?;
            sync_content_directories(&child)?;
        }
    }
    fsync(directory).map_err(|error| storage_error("sync staged content directory", error))
}

fn entry_exists(directory: &rustix::fd::OwnedFd, name: &str) -> Result<bool, AppError> {
    match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(storage_error("inspect interrupted Publish bytes", error)),
    }
}

fn quarantine_if_present(
    source_directory: &rustix::fd::OwnedFd,
    source_name: &str,
    quarantine: &rustix::fd::OwnedFd,
    destination_name: &str,
) -> Result<(), AppError> {
    match renameat_with(
        source_directory,
        source_name,
        quarantine,
        destination_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(storage_error("quarantine interrupted Publish bytes", error)),
    }
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn hash_file(file: &mut File) -> Result<String, AppError> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| AppError::internal(format!("cannot hash Revision member: {error}")))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn copy_and_hash(
    source: &mut File,
    destination: &mut File,
    expected_bytes: u64,
) -> Result<(u64, String), AppError> {
    let mut digest = Sha256::new();
    let mut logical_bytes = 0_u64;
    let mut remaining = expected_bytes;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    while remaining > 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| AppError::internal("copy length overflow"))?;
        let read = source
            .read(&mut buffer[..limit])
            .map_err(|error| AppError::usage(format!("cannot read Artifact source: {error}")))?;
        if read == 0 {
            return Err(AppError::source_changed());
        }
        destination
            .write_all(&buffer[..read])
            .map_err(|error| AppError::internal(format!("cannot copy Artifact source: {error}")))?;
        digest.update(&buffer[..read]);
        let read = u64::try_from(read).map_err(|_| AppError::internal("read size overflow"))?;
        logical_bytes = logical_bytes
            .checked_add(read)
            .ok_or_else(|| AppError::internal("Artifact size overflow"))?;
        remaining = remaining
            .checked_sub(read)
            .ok_or_else(|| AppError::internal("copy length underflow"))?;
    }
    Ok((logical_bytes, format!("sha256:{:x}", digest.finalize())))
}

fn revision_member_error(error: rustix::io::Errno) -> AppError {
    if error == rustix::io::Errno::NOENT {
        AppError::not_found("Artifact member does not exist")
    } else {
        storage_error("open Revision member", error)
    }
}

#[cfg(feature = "test-faults")]
fn storage_crash_fault(variable: &str, boundary: &str) {
    assert!(
        std::env::var_os(variable).is_none(),
        "injected Publish crash after {boundary}"
    );
}

#[cfg(not(feature = "test-faults"))]
fn storage_crash_fault(_variable: &str, _boundary: &str) {}

fn storage_error(action: &str, error: rustix::io::Errno) -> AppError {
    AppError::internal(format!("cannot {action}: {error}"))
}

fn validate_entry_path(value: &str) -> Result<(), AppError> {
    validate_relative_member_path(value)
        .map_err(|_| AppError::internal("invalid immutable Revision member path"))
}

fn validate_storage_identifier(value: &str) -> Result<(), AppError> {
    if value.len() == 26
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Ok(());
    }
    Err(AppError::internal("invalid opaque storage identifier"))
}
