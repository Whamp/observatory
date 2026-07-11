use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::AppError;

pub(crate) const MANIFEST_FILE: &str = "revision-manifest.json";
pub(crate) const CONTENT_DIRECTORY: &str = "content";

#[derive(Clone, Copy)]
pub(crate) struct RevisionIdentity<'a> {
    pub(crate) artifact_id: &'a str,
    pub(crate) revision_id: &'a str,
    pub(crate) entry_path: &'a str,
    pub(crate) entry_media_type: &'a str,
    pub(crate) logical_bytes: u64,
    pub(crate) published_at: &'a str,
}

pub(crate) struct RevisionManifestInput<'a> {
    pub(crate) artifact_id: &'a str,
    pub(crate) revision_id: &'a str,
    pub(crate) entry_path: &'a str,
    pub(crate) entry_media_type: &'a str,
    pub(crate) published_at: &'a str,
    pub(crate) members: Vec<RevisionMemberInput>,
}

#[derive(Clone, Debug)]
pub(crate) struct RevisionMemberInput {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) digest: String,
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
pub(crate) struct RevisionMember {
    path: String,
    size: u64,
    digest: String,
}

impl RevisionManifest {
    pub(crate) fn new(input: RevisionManifestInput<'_>) -> Result<Self, AppError> {
        let files = u64::try_from(input.members.len())
            .map_err(|_| AppError::internal("Revision member count overflow"))?;
        let logical_bytes = input.members.iter().try_fold(0_u64, |total, member| {
            total
                .checked_add(member.size)
                .ok_or_else(|| AppError::internal("Revision logical size overflow"))
        })?;
        Ok(Self {
            schema_version: 1,
            artifact_id: input.artifact_id.to_owned(),
            revision_id: input.revision_id.to_owned(),
            entry_path: input.entry_path.to_owned(),
            entry_media_type: input.entry_media_type.to_owned(),
            files,
            logical_bytes,
            published_at: input.published_at.to_owned(),
            members: input
                .members
                .into_iter()
                .map(|member| RevisionMember {
                    path: member.path,
                    size: member.size,
                    digest: member.digest,
                })
                .collect(),
        })
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, AppError> {
        serde_jcs::to_vec(self).map_err(|error| {
            AppError::internal(format!("cannot encode Revision manifest: {error}"))
        })
    }

    pub(crate) fn digest(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    pub(crate) fn content_digest(&self) -> Result<String, AppError> {
        let canonical = serde_jcs::to_vec(&self.members).map_err(|error| {
            AppError::internal(format!("cannot encode Revision member inventory: {error}"))
        })?;
        Ok(Self::digest(&canonical))
    }

    pub(crate) fn identity_matches(&self, expected: RevisionIdentity<'_>) -> bool {
        let ordered_unique = self
            .members
            .windows(2)
            .all(|members| members[0].path < members[1].path);
        let member_bytes = self
            .members
            .iter()
            .try_fold(0_u64, |total, member| total.checked_add(member.size));
        self.schema_version == 1
            && self.artifact_id == expected.artifact_id
            && self.revision_id == expected.revision_id
            && self.entry_path == expected.entry_path
            && self.entry_media_type == expected.entry_media_type
            && self.logical_bytes == expected.logical_bytes
            && self.published_at == expected.published_at
            && u64::try_from(self.members.len()).ok() == Some(self.files)
            && member_bytes == Some(self.logical_bytes)
            && ordered_unique
            && self
                .members
                .iter()
                .any(|member| member.path == expected.entry_path)
    }

    pub(crate) fn member(&self, path: &str) -> Option<&RevisionMember> {
        self.members.iter().find(|member| member.path == path)
    }

    pub(crate) fn members(&self) -> &[RevisionMember] {
        &self.members
    }

    pub(crate) fn files(&self) -> u64 {
        self.files
    }

    pub(crate) fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }
}

impl RevisionMember {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
}

#[cfg(test)]
mod tests {
    use super::{RevisionManifest, RevisionManifestInput, RevisionMemberInput};

    #[test]
    fn format_v1_canonical_bundle_bytes_are_frozen() -> Result<(), Box<dyn std::error::Error>> {
        let manifest = RevisionManifest::new(RevisionManifestInput {
            artifact_id: "00000000000000000000000001",
            revision_id: "00000000000000000000000002",
            entry_path: "index.html",
            entry_media_type: "text/html",
            published_at: "2026-01-02T03:04:05.006Z",
            members: vec![
                RevisionMemberInput {
                    path: "assets/app.js".to_owned(),
                    size: 3,
                    digest:
                        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_owned(),
                },
                RevisionMemberInput {
                    path: "index.html".to_owned(),
                    size: 4,
                    digest:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_owned(),
                },
            ],
        })?;
        let expected = concat!(
            r#"{"artifactId":"00000000000000000000000001","entryMediaType":"text/html","entryPath":"index.html","files":2,"logicalBytes":7,"members":["#,
            r#"{"digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","path":"assets/app.js","size":3},"#,
            r#"{"digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","path":"index.html","size":4}],"#,
            r#""publishedAt":"2026-01-02T03:04:05.006Z","revisionId":"00000000000000000000000002","schemaVersion":1}"#,
        );
        assert_eq!(String::from_utf8(manifest.canonical_bytes()?)?, expected);
        Ok(())
    }
}
