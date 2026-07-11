use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::AppError;

pub(crate) const MANIFEST_FILE: &str = "revision-manifest.json";
pub(crate) const CONTENT_DIRECTORY: &str = "content";

#[derive(Clone, Copy)]
pub(crate) struct SingleFileRevision<'a> {
    artifact_id: &'a str,
    revision_id: &'a str,
    entry_path: &'a str,
    entry_media_type: &'a str,
    logical_bytes: u64,
    published_at: &'a str,
    payload_digest: &'a str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RevisionManifest {
    schema_version: u8,
    artifact_id: String,
    revision_id: String,
    entry_path: String,
    entry_media_type: String,
    files: u64,
    logical_bytes: u64,
    published_at: String,
    members: Vec<RevisionMember>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RevisionMember {
    path: String,
    size: u64,
    digest: String,
}

impl<'a> SingleFileRevision<'a> {
    pub(crate) fn new(artifact_id: &'a str, revision_id: &'a str) -> Self {
        SingleFileRevision {
            artifact_id,
            revision_id,
            entry_path: "",
            entry_media_type: "",
            logical_bytes: 0,
            published_at: "",
            payload_digest: "",
        }
    }

    pub(crate) const fn with_entry(mut self, path: &'a str, media_type: &'a str) -> Self {
        self.entry_path = path;
        self.entry_media_type = media_type;
        self
    }

    pub(crate) const fn with_content(
        mut self,
        logical_bytes: u64,
        published_at: &'a str,
        payload_digest: &'a str,
    ) -> Self {
        self.logical_bytes = logical_bytes;
        self.published_at = published_at;
        self.payload_digest = payload_digest;
        self
    }
}

impl RevisionManifest {
    pub(crate) fn single_file(input: &SingleFileRevision<'_>) -> Self {
        Self {
            schema_version: 1,
            artifact_id: input.artifact_id.to_owned(),
            revision_id: input.revision_id.to_owned(),
            entry_path: input.entry_path.to_owned(),
            entry_media_type: input.entry_media_type.to_owned(),
            files: 1,
            logical_bytes: input.logical_bytes,
            published_at: input.published_at.to_owned(),
            members: vec![RevisionMember {
                path: input.entry_path.to_owned(),
                size: input.logical_bytes,
                digest: input.payload_digest.to_owned(),
            }],
        }
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, AppError> {
        serde_jcs::to_vec(self).map_err(|error| {
            AppError::internal(format!("cannot encode Revision manifest: {error}"))
        })
    }

    pub(crate) fn digest(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    pub(crate) fn verify_single_file(&self, input: &SingleFileRevision<'_>) -> bool {
        self.identity_matches(input) && self.entry_matches(input) && self.content_matches(input)
    }

    fn identity_matches(&self, input: &SingleFileRevision<'_>) -> bool {
        self.schema_version == 1
            && self.artifact_id == input.artifact_id
            && self.revision_id == input.revision_id
    }

    fn entry_matches(&self, input: &SingleFileRevision<'_>) -> bool {
        self.entry_path == input.entry_path
            && self.entry_media_type == input.entry_media_type
            && self.files == 1
            && self.members.len() == 1
            && self.members[0].path == input.entry_path
    }

    fn content_matches(&self, input: &SingleFileRevision<'_>) -> bool {
        self.logical_bytes == input.logical_bytes
            && self.published_at == input.published_at
            && self.members[0].size == input.logical_bytes
            && self.members[0].digest == input.payload_digest
    }
}
