use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;

use rustix::fs::{
    AtFlags, Mode, OFlags, RenameFlags, fsync, mkdirat, openat, renameat_with, statat,
};
use sha2::{Digest, Sha256};

use crate::error::AppError;
use crate::revision_manifest::{
    CONTENT_DIRECTORY, MANIFEST_FILE, RevisionManifest, SingleFileRevision,
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
    logical_bytes: u64,
    payload_digest: String,
    manifest_digest: String,
}

#[derive(Clone, Debug)]
pub(crate) struct FinalizedRevision {
    revision_id: String,
    entry_path: String,
    entry_media_type: String,
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

        let manifest_input = SingleFileRevision::new(request.artifact_id, request.revision_id)
            .with_entry(request.entry_path, request.entry_media_type)
            .with_content(logical_bytes, request.published_at, &payload_digest);
        let manifest = RevisionManifest::single_file(&manifest_input);
        let manifest_bytes = manifest.canonical_bytes()?;
        let manifest_digest = RevisionManifest::digest(&manifest_bytes);
        let mut manifest_file = File::from(
            openat(
                &operation,
                MANIFEST_FILE,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|error| storage_error("create Revision manifest", error))?,
        );
        manifest_file.write_all(&manifest_bytes).map_err(|error| {
            AppError::internal(format!("cannot write Revision manifest: {error}"))
        })?;
        storage_crash_fault(
            "OBS_TEST_CRASH_PUBLISH_AFTER_MANIFEST_WRITE",
            "manifest write",
        );
        manifest_file.sync_all().map_err(|error| {
            AppError::internal(format!("cannot sync Revision manifest: {error}"))
        })?;
        storage_crash_fault(
            "OBS_TEST_CRASH_PUBLISH_AFTER_MANIFEST_SYNC",
            "manifest sync",
        );
        fsync(&content).map_err(|error| storage_error("sync Revision content directory", error))?;
        storage_crash_fault(
            "OBS_TEST_CRASH_PUBLISH_AFTER_CONTENT_SYNC",
            "content directory sync",
        );
        fsync(&operation)
            .map_err(|error| storage_error("sync Publish operation directory", error))?;
        storage_crash_fault(
            "OBS_TEST_CRASH_PUBLISH_AFTER_OPERATION_SYNC",
            "operation directory sync",
        );
        fsync(&staging).map_err(|error| storage_error("sync staging directory", error))?;
        storage_crash_fault(
            "OBS_TEST_CRASH_PUBLISH_AFTER_STAGING_SYNC",
            "staging directory sync",
        );
        Ok(StagedRevision {
            operation_id: request.operation_id.to_owned(),
            revision_id: request.revision_id.to_owned(),
            entry_path: request.entry_path.to_owned(),
            entry_media_type: request.entry_media_type.to_owned(),
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
    ) -> Result<Vec<u8>, AppError> {
        validate_storage_identifier(revision_id)?;
        validate_entry_path(entry_path)?;
        let revisions = self.open_directory("revisions")?;
        let revision = openat(&revisions, revision_id, directory_flags(), Mode::empty())
            .map_err(|error| storage_error("open immutable Revision", error))?;
        let content = openat(
            &revision,
            CONTENT_DIRECTORY,
            directory_flags(),
            Mode::empty(),
        )
        .map_err(|error| storage_error("open immutable Revision content", error))?;
        let mut file = File::from(
            openat(
                &content,
                entry_path,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| storage_error("open immutable Revision member", error))?,
        );
        let metadata = file.metadata().map_err(|error| {
            AppError::internal(format!("cannot inspect Revision member: {error}"))
        })?;
        if !metadata.file_type().is_file() {
            return Err(AppError::internal("Revision member is not a regular file"));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| AppError::internal(format!("cannot read Revision member: {error}")))?;
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
    logical_bytes: u64,
    payload_digest: String,
    manifest_digest: String,
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
    let content = openat(
        revision,
        CONTENT_DIRECTORY,
        directory_flags(),
        Mode::empty(),
    )
    .map_err(|error| storage_error("open recoverable Revision content", error))?;
    let mut payload = File::from(
        openat(
            &content,
            expected.entry_path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| storage_error("open recoverable Revision member", error))?,
    );
    if !payload
        .metadata()
        .map_err(|error| AppError::internal(format!("cannot inspect Revision member: {error}")))?
        .file_type()
        .is_file()
    {
        return Err(AppError::internal("Revision member is not a regular file"));
    }
    let mut payload_bytes = Vec::new();
    payload
        .read_to_end(&mut payload_bytes)
        .map_err(|error| AppError::internal(format!("cannot read Revision member: {error}")))?;
    let payload_digest = format!("sha256:{:x}", Sha256::digest(&payload_bytes));
    if u64::try_from(payload_bytes.len()).ok() != Some(expected.logical_bytes)
        || expected
            .payload_digest
            .is_some_and(|digest| digest != payload_digest)
    {
        return Err(AppError::internal(
            "recoverable Revision payload does not match intent",
        ));
    }
    let manifest: RevisionManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        AppError::internal(format!("recoverable Revision manifest is invalid: {error}"))
    })?;
    if manifest.canonical_bytes()? != manifest_bytes
        || !manifest.verify_single_file(
            &SingleFileRevision::new(expected.artifact_id, expected.revision_id)
                .with_entry(expected.entry_path, expected.entry_media_type)
                .with_content(
                    expected.logical_bytes,
                    expected.published_at,
                    &payload_digest,
                ),
        )
    {
        return Err(AppError::internal(
            "recoverable Revision manifest does not match intent",
        ));
    }
    Ok(VerifiedRevision {
        revision_id: expected.revision_id.to_owned(),
        entry_path: expected.entry_path.to_owned(),
        entry_media_type: expected.entry_media_type.to_owned(),
        logical_bytes: expected.logical_bytes,
        payload_digest,
        manifest_digest,
    })
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

fn storage_crash_fault(variable: &str, boundary: &str) {
    assert!(
        std::env::var_os(variable).is_none(),
        "injected Publish crash after {boundary}"
    );
}

fn storage_error(action: &str, error: rustix::io::Errno) -> AppError {
    AppError::internal(format!("cannot {action}: {error}"))
}

pub(crate) fn safe_entry_name(source: &SafeRegularFile) -> Result<&str, AppError> {
    let name = source.file_name().to_str().ok_or_else(|| {
        AppError::invalid(
            "invalid_source",
            "Artifact source filename must be valid UTF-8",
        )
    })?;
    if entry_path_is_safe(name) {
        Ok(name)
    } else {
        Err(AppError::invalid(
            "invalid_source",
            "Artifact source filename cannot be represented by a safe Artifact route",
        ))
    }
}

fn validate_entry_path(value: &str) -> Result<(), AppError> {
    if entry_path_is_safe(value) {
        Ok(())
    } else {
        Err(AppError::internal("invalid immutable Revision entry path"))
    }
}

fn entry_path_is_safe(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', '\0'])
        && !value.as_bytes().windows(3).any(|window| {
            window[0] == b'%' && window[1].is_ascii_hexdigit() && window[2].is_ascii_hexdigit()
        })
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
