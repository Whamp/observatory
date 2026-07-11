use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::artifact_source::{ArtifactSource, SourceWarning};
use crate::artifact_storage::{
    ArtifactStorage, FinalizedRevision, RecoveryRequest, StageRequest, StagedRevision,
};
use crate::catalogue::Catalogue;
use crate::crypto::random_opaque_id;
use crate::cursor;
use crate::error::{AppError, StoredError};
use crate::project::LedgerQuery;
use crate::route_slug;
use caseless::default_case_fold_str;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use rustix::fs::statvfs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

const DEFAULT_RETENTION_MS: u64 = 2_592_000_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PublishArtifactRequest {
    source: PublishSource,
    project_id: String,
    entry: Option<String>,
    title: Option<String>,
    description: Option<String>,
    slug: Option<String>,
    #[serde(default)]
    retention: PublishRetention,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReplaceArtifactRequest {
    source: PublishSource,
    entry: Option<String>,
    title: Option<String>,
    description: Option<String>,
    slug: Option<String>,
    retention: Option<PublishRetention>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ImportArtifactsRequest {
    project_id: String,
    #[serde(default)]
    defaults: ImportOptions,
    items: Vec<ImportItem>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportOptions {
    project_id: Option<String>,
    entry: Option<String>,
    title: Option<String>,
    description: Option<String>,
    slug: Option<String>,
    retention: Option<PublishRetention>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportItem {
    source: PublishSource,
    label: Option<String>,
    options: Option<ImportOptions>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportResult {
    operation: String,
    overall: ImportOverall,
    partial: bool,
    counts: ImportCounts,
    items: Vec<ImportItemOutcome>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ImportCounts {
    requested: u64,
    succeeded: u64,
    failed: u64,
    skipped: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportItemOutcome {
    index: u64,
    label: String,
    status: ImportItemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ImportItemResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_error: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ImportOverall {
    Complete,
    Partial,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ImportItemStatus {
    Committed,
    Failed,
    UnchangedReplay,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportItemResult {
    artifact_id: String,
    artifact_key: String,
    artifact_record_version: u64,
    artifact_api_url: String,
    revision_id: String,
    revision_api_url: String,
    revision_open_url: String,
    open_url: String,
    detail_url: String,
    files: u64,
    logical_bytes: u64,
    retention: Retention,
    warnings: Vec<PublishWarning>,
    duplicate_candidates: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct ImportOutcome {
    result: ImportResult,
    replayed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublishSource {
    path: String,
    caller_working_directory: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PublishRetention {
    #[serde(default)]
    mode: RetentionMode,
    ttl_ms: Option<u64>,
    pin_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RetentionMode {
    #[default]
    Default,
    Ttl,
    Pinned,
}

impl RetentionMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Ttl => "ttl",
            Self::Pinned => "pinned",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RetentionFilter {
    Default,
    Ttl,
    Pinned,
    #[serde(other)]
    Invalid,
}

impl RetentionFilter {
    fn valid(self) -> Result<RetentionMode, AppError> {
        match self {
            Self::Default => Ok(RetentionMode::Default),
            Self::Ttl => Ok(RetentionMode::Ttl),
            Self::Pinned => Ok(RetentionMode::Pinned),
            Self::Invalid => Err(AppError::invalid(
                "invalid_filter",
                "invalid Artifact retention mode",
            )),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Ttl => "ttl",
            Self::Pinned => "pinned",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublishResult {
    operation: String,
    artifact: Artifact,
    revision: Revision,
    warnings: Vec<PublishWarning>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishWarning {
    code: String,
    message: String,
    member: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Artifact {
    kind: String,
    id: String,
    key: String,
    record_version: u64,
    state: String,
    title: String,
    description: String,
    slug: String,
    project: ProjectReference,
    current_revision_id: String,
    retention: Retention,
    files: u64,
    logical_bytes: u64,
    revision_count: u64,
    published_at: String,
    updated_at: String,
    api_url: String,
    open_url: String,
    detail_url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Revision {
    kind: String,
    id: String,
    artifact_id: String,
    state: String,
    entry_path: String,
    entry_media_type: String,
    files: u64,
    logical_bytes: u64,
    manifest_digest: String,
    published_at: String,
    #[serde(skip)]
    superseded_at: Option<String>,
    api_url: String,
    open_url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProjectReference {
    id: String,
    key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Retention {
    mode: RetentionMode,
    ttl_ms: Option<u64>,
    expires_at: Option<String>,
    pin_reason: Option<String>,
    recovery_until: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishIntent {
    protocol: String,
    operation_id: String,
    phase: PublishPhase,
    artifact: Artifact,
    revision: Revision,
    payload_digest: Option<String>,
    source_snapshot_digest: String,
    #[serde(default)]
    source_root_snapshot_digest: Option<String>,
    capacity_reservation_bytes: u64,
    idempotency_key: String,
    fingerprint: String,
    #[serde(default)]
    publication_method: Option<PublicationMethod>,
    #[serde(default)]
    request_identity: Option<String>,
    #[serde(default)]
    warnings: Vec<PublishWarning>,
    #[serde(default)]
    replacement: Option<ReplacementIntent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplacementIntent {
    previous_revision_id: String,
    expected_record_version: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PublishPhase {
    IntentRecorded,
    Staged,
    Renamed,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PublicationMethod {
    #[default]
    Publish,
    Import,
    Replace,
}

impl PublicationMethod {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::Import => "import",
            Self::Replace => "replace",
        }
    }
}

impl PublishIntent {
    fn publication_method(&self) -> PublicationMethod {
        self.publication_method.unwrap_or_else(|| {
            if self.replacement.is_some() {
                PublicationMethod::Replace
            } else {
                PublicationMethod::Publish
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ListDirection {
    Asc,
    Desc,
}

impl ListDirection {
    fn parse(value: Option<&str>) -> Result<Self, AppError> {
        match value.unwrap_or("desc") {
            "asc" => Ok(Self::Asc),
            "desc" => Ok(Self::Desc),
            _ => Err(AppError::invalid(
                "invalid_direction",
                "Artifact direction must be asc or desc",
            )),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PublishOutcome {
    result: PublishResult,
    replayed: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ListArtifactsQuery {
    project_id: Option<String>,
    state: Option<String>,
    #[serde(rename = "retentionMode")]
    retention_mode: Option<RetentionFilter>,
    query: Option<String>,
    order: Option<String>,
    direction: Option<String>,
    limit: Option<u16>,
    after: Option<String>,
    #[serde(skip)]
    cursor_endpoint: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactList {
    items: Vec<Artifact>,
    page: ArtifactPage,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RevisionList {
    items: Vec<Revision>,
    page: ArtifactPage,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ListRevisionsQuery {
    availability: Option<String>,
    order: Option<String>,
    direction: Option<String>,
    limit: Option<u16>,
    after: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RevisionCursor {
    endpoint: String,
    artifact_id: String,
    availability: String,
    order: String,
    direction: ListDirection,
    last_value: String,
    last_id: String,
    expires_at_ms: i128,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactPage {
    limit: u16,
    next_cursor: Option<String>,
    has_more: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactCursor {
    endpoint: String,
    project_id: Option<String>,
    state: Option<String>,
    retention_mode: Option<RetentionMode>,
    query: String,
    order: String,
    direction: ListDirection,
    last_value: String,
    last_id: String,
    expires_at_ms: i128,
}

struct NewArtifact<'a> {
    artifact_id: &'a str,
    revision_id: &'a str,
    title: &'a str,
    description: &'a str,
    slug: &'a str,
    project: ProjectReference,
    retention: Retention,
    files: u64,
    logical_bytes: u64,
    published_at: &'a str,
}

#[derive(Clone, Copy)]
struct NewRevision<'a> {
    revision_id: &'a str,
    artifact_id: &'a str,
    entry_path: &'a str,
    entry_media_type: &'a str,
    files: u64,
    logical_bytes: u64,
    manifest_digest: &'a str,
    published_at: &'a str,
}

struct ReplacementState {
    artifact: Artifact,
    expected_record_version: u64,
}

struct PreparedPublish {
    source: ArtifactSource,
    operation_id: String,
    artifact_id: String,
    revision_id: String,
    entry_path: String,
    entry_media_type: String,
    published_at: String,
    intent: PublishIntent,
}

#[derive(Clone, Copy)]
struct PublishPreparation<'a> {
    idempotency_key: &'a str,
    fingerprint: &'a str,
    replacement: Option<&'a ReplacementState>,
    publication_method: PublicationMethod,
}

#[derive(Clone, Copy)]
struct CapacityPolicy {
    max_stored_bytes: u64,
    max_live_artifacts: u64,
    filesystem_available_bytes: u64,
    reserve_bytes: u64,
}

#[derive(Clone, Copy)]
enum ReservationKind {
    Publish,
    Replace,
}

#[derive(Clone, Copy)]
struct PublishReservation<'a> {
    idempotency_key: &'a str,
    fingerprint: &'a str,
    project_id: &'a str,
    required_bytes: u64,
    capacity: CapacityPolicy,
    kind: ReservationKind,
}

#[derive(Clone, Copy)]
struct ArtifactCursorBinding<'a> {
    query: &'a ListArtifactsQuery,
    state: Option<&'a str>,
    retention_mode: Option<RetentionMode>,
    folded_query: &'a str,
    order: &'a str,
    direction: ListDirection,
}

struct NormalizedRevisionList {
    availability: String,
    order: String,
    direction: ListDirection,
    limit: u16,
    cursor: Option<RevisionCursor>,
}

struct NormalizedArtifactList {
    cursor_endpoint: String,
    project_id: Option<String>,
    state: Option<String>,
    retention_mode: Option<RetentionMode>,
    query: String,
    order: String,
    direction: ListDirection,
    limit: u16,
    cursor: Option<ArtifactCursor>,
}

#[derive(Clone, Debug)]
pub(crate) struct ServedRevision {
    pub(crate) bytes: Vec<u8>,
    pub(crate) media_type: String,
    pub(crate) artifact_key: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ArtifactService {
    catalogue: Catalogue,
    storage: ArtifactStorage,
    canonical_origin: String,
    max_stored_bytes: u64,
    max_live_artifacts: u64,
    in_flight: Arc<Mutex<HashMap<String, String>>>,
}

impl ArtifactService {
    pub(crate) fn new(
        catalogue: Catalogue,
        canonical_origin: String,
        max_stored_bytes: u64,
        max_live_artifacts: u64,
    ) -> Result<Self, AppError> {
        let storage = ArtifactStorage::open(catalogue.root())?;
        Ok(Self {
            catalogue,
            storage,
            canonical_origin,
            max_stored_bytes,
            max_live_artifacts,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub(crate) fn reconcile_publish_intents(&self) -> Result<(), AppError> {
        let mut connection = self.catalogue.connection()?;
        let intents = {
            let mut statement = connection
                .prepare(
                    "SELECT id,details_json FROM operation_intents
                     WHERE kind='artifact_publish'
                       AND state NOT IN ('completed','cancelled','failed_terminal')
                     ORDER BY id",
                )
                .map_err(database_error)?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(database_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(database_error)?
        };
        for (operation_id, encoded) in intents {
            let intent = decode_recovery_intent(&operation_id, &encoded)?;
            validate_recovery_idempotency(&connection, &intent)?;
            if self.defer_intent_without_bytes(&mut connection, &intent)? {
                continue;
            }
            let expected = RecoveryRequest {
                artifact_id: &intent.artifact.id,
                revision_id: &intent.revision.id,
                entry_path: &intent.revision.entry_path,
                entry_media_type: &intent.revision.entry_media_type,
                files: intent.revision.files,
                logical_bytes: intent.revision.logical_bytes,
                published_at: &intent.revision.published_at,
                payload_digest: intent.payload_digest.as_deref(),
                manifest_digest: (!intent.revision.manifest_digest.is_empty())
                    .then_some(intent.revision.manifest_digest.as_str()),
            };
            let verification = match self.storage.verify_finalized(expected) {
                Ok(finalized) => Ok((finalized, intent.clone())),
                Err(finalized_error) => match self
                    .storage
                    .verify_staged(&intent.operation_id, expected)
                {
                    Ok(staged) => {
                        let recovered_intent = staged_intent(intent.clone(), &staged);
                        update_publish_phase(&connection, &recovered_intent, PublishPhase::Staged)?;
                        self.storage
                            .finalize(staged)
                            .map(|finalized| (finalized, recovered_intent))
                    }
                    Err(staged_error) => Err(AppError::internal(format!(
                        "interrupted Publish bytes are not recoverable: finalized={finalized_error}; staged={staged_error}"
                    ))),
                },
            };
            match verification {
                Ok((finalized, recovered_intent)) => {
                    let completed = completed_intent(recovered_intent, &finalized);
                    update_publish_phase(&connection, &completed, PublishPhase::Renamed)?;
                    let idempotency_key = completed.idempotency_key.clone();
                    commit_publish_visibility(&mut connection, &completed, &idempotency_key)?;
                }
                Err(error) => {
                    self.storage
                        .quarantine_interrupted(&intent.operation_id, &intent.revision.id)?;
                    fail_publish_intent(
                        &connection,
                        &intent.operation_id,
                        &intent.idempotency_key,
                        &error,
                        true,
                    )?;
                    connection
                        .execute(
                            "INSERT INTO audit_events(
                                 kind,details_json,at,actor,cause,resource_type,resource_id
                             ) VALUES (
                                 'artifact_publish_recovery_quarantined',?1,
                                 strftime('%Y-%m-%dT%H:%M:%fZ','now'),'system',
                                 'invalid_interrupted_publish','artifact',?2
                             )",
                            params![
                                serde_json::json!({
                                    "operationId": intent.operation_id,
                                    "revisionId": intent.revision.id,
                                    "errorCode": error.code()
                                })
                                .to_string(),
                                intent.artifact.id,
                            ],
                        )
                        .map_err(database_error)?;
                }
            }
        }
        Ok(())
    }

    fn defer_intent_without_bytes(
        &self,
        connection: &mut Connection,
        intent: &PublishIntent,
    ) -> Result<bool, AppError> {
        if intent.phase != PublishPhase::IntentRecorded
            || self
                .storage
                .has_interrupted_bytes(&intent.operation_id, &intent.revision.id)?
        {
            return Ok(false);
        }
        mark_publish_awaiting_retry(connection, intent)?;
        Ok(true)
    }

    pub(crate) fn publish(
        &self,
        request: &PublishArtifactRequest,
        idempotency_key: &str,
    ) -> Result<PublishOutcome, AppError> {
        self.publish_with_method(request, idempotency_key, PublicationMethod::Publish)
    }

    fn publish_with_method(
        &self,
        request: &PublishArtifactRequest,
        idempotency_key: &str,
        publication_method: PublicationMethod,
    ) -> Result<PublishOutcome, AppError> {
        validate_idempotency_key(idempotency_key)?;
        validate_publish_paths(request)?;
        let normalized_source = normalized_source_selection(&request.source.path)?;
        let fingerprint = publish_fingerprint(request, &normalized_source)?;
        let _guard = self.begin_mutation(idempotency_key, &fingerprint)?;
        let mut connection = self.catalogue.connection()?;
        if let Some(prepared) =
            resumable_publish(&connection, request, idempotency_key, &fingerprint)?
        {
            return self.persist_publish(&mut connection, prepared, idempotency_key);
        }
        if let Some(outcome) = completed_publish(&connection, idempotency_key, &fingerprint, false)?
        {
            return Ok(outcome);
        }
        let prepared = self.prepare_publish(
            &mut connection,
            request,
            PublishPreparation {
                idempotency_key,
                fingerprint: &fingerprint,
                replacement: None,
                publication_method,
            },
        )?;
        self.persist_publish(&mut connection, prepared, idempotency_key)
    }

    pub(crate) fn import(
        &self,
        request: &ImportArtifactsRequest,
        idempotency_key: &str,
    ) -> Result<ImportOutcome, AppError> {
        validate_idempotency_key(idempotency_key)?;
        if request.items.is_empty() {
            return Err(AppError::invalid(
                "invalid_import",
                "Artifact import requires at least one source",
            ));
        }
        let selections = request
            .items
            .iter()
            .map(|item| normalized_import_selection(&item.source))
            .collect::<Vec<_>>();
        let fingerprint = import_fingerprint(request, &selections)?;
        let _guard = self.begin_mutation(idempotency_key, &fingerprint)?;
        if let Some(outcome) =
            completed_import_batch(&self.catalogue.connection()?, idempotency_key, &fingerprint)?
        {
            return Ok(outcome);
        }
        record_import_batch(
            &mut self.catalogue.connection()?,
            idempotency_key,
            &fingerprint,
        )?;

        let mut selection_counts = HashMap::new();
        for selection in selections.iter().flatten() {
            *selection_counts.entry(selection.clone()).or_insert(0_u64) += 1;
        }
        let mut items = Vec::with_capacity(request.items.len());
        for (index, item) in request.items.iter().enumerate() {
            let label = match import_label(item, index) {
                Ok(label) => label,
                Err(error) => {
                    items.push(import_failure(
                        index,
                        import_fallback_label(item, index),
                        &error,
                    )?);
                    continue;
                }
            };
            let duplicate = selections[index]
                .as_ref()
                .is_some_and(|selection| selection_counts.get(selection).copied().unwrap_or(0) > 1);
            if duplicate {
                items.push(import_failure(
                    index,
                    label,
                    &AppError::invalid(
                        "duplicate_selection",
                        "source selection occurs more than once in this import request",
                    ),
                )?);
                continue;
            }
            let publish_request =
                match import_publish_request(request, item, selections[index].as_ref()) {
                    Ok(request) => request,
                    Err(error) => {
                        items.push(import_failure(index, label, &error)?);
                        continue;
                    }
                };
            let item_key = import_item_key(idempotency_key, index);
            let outcome =
                self.publish_with_method(&publish_request, &item_key, PublicationMethod::Import);
            match outcome {
                Ok(outcome) => {
                    let duplicate_candidates = import_duplicate_candidates(
                        &self.catalogue.connection()?,
                        &item_key,
                        &outcome.result().artifact.id,
                    )?;
                    items.push(import_success(
                        index,
                        label,
                        &outcome,
                        duplicate_candidates,
                    )?);
                }
                Err(error) => {
                    if !import_failure_is_trustworthy(&self.catalogue.connection()?, &item_key)? {
                        return Err(error);
                    }
                    items.push(import_failure(index, label, &error)?);
                }
            }
        }
        let result = aggregate_import_items(items)?;
        complete_import_batch(
            &mut self.catalogue.connection()?,
            idempotency_key,
            &fingerprint,
            &result,
        )?;
        Ok(ImportOutcome {
            result,
            replayed: false,
        })
    }

    pub(crate) fn replace(
        &self,
        artifact_id: &str,
        request: &ReplaceArtifactRequest,
        if_match: &str,
        idempotency_key: &str,
    ) -> Result<PublishOutcome, AppError> {
        validate_opaque_id(artifact_id)?;
        validate_idempotency_key(idempotency_key)?;
        validate_source_paths(&request.source)?;
        let normalized_source = normalized_source_selection(&request.source.path)?;
        let fingerprint =
            replacement_fingerprint(artifact_id, request, if_match, &normalized_source)?;
        let _guard = self.begin_mutation(idempotency_key, &fingerprint)?;
        let mut connection = self.catalogue.connection()?;
        if let Some(outcome) = completed_publish(&connection, idempotency_key, &fingerprint, true)?
        {
            return Ok(outcome);
        }
        let current = artifact_by_id(&connection, artifact_id, &self.canonical_origin)?
            .ok_or_else(|| AppError::not_found("Artifact does not exist"))?;
        if current.state != "live" {
            return Err(AppError::gone(
                "artifact_gone",
                "Artifact identity is not live",
            ));
        }
        if current.etag() != if_match {
            return Err(AppError::changed_record());
        }
        let publish_request = replacement_publish_request(request, &current);
        if let Some(prepared) =
            resumable_publish(&connection, &publish_request, idempotency_key, &fingerprint)?
        {
            return self.persist_publish(&mut connection, prepared, idempotency_key);
        }
        let expected_record_version = current.record_version;
        let replacement = ReplacementState {
            artifact: current,
            expected_record_version,
        };
        let prepared = self.prepare_publish(
            &mut connection,
            &publish_request,
            PublishPreparation {
                idempotency_key,
                fingerprint: &fingerprint,
                replacement: Some(&replacement),
                publication_method: PublicationMethod::Replace,
            },
        )?;
        self.persist_publish(&mut connection, prepared, idempotency_key)
    }

    fn prepare_publish(
        &self,
        connection: &mut Connection,
        request: &PublishArtifactRequest,
        preparation: PublishPreparation<'_>,
    ) -> Result<PreparedPublish, AppError> {
        let project = project_reference(connection, &request.project_id)?;
        let normalized_source = normalized_source_selection(&request.source.path)?;
        let mut source = ArtifactSource::open(Path::new(&normalized_source))?;
        let entry_path = source.entry_path(request.entry.as_deref())?;
        let entry_media_type = entry_media_type(&entry_path)?;
        let title = artifact_title(request, &entry_path, &entry_media_type, &mut source)?;
        let description = request
            .description
            .clone()
            .or_else(|| source.portable_description().map(str::to_owned))
            .unwrap_or_default();
        let slug = replacement_slug(request.slug.as_deref(), &title, preparation.replacement)?;
        let published = observed_instant()?;
        let published_at = format_instant(published)?;
        let retention = retention(&request.retention, published)?;
        let warnings = publish_warnings(&mut source)?;
        let filesystem_capacity = filesystem_capacity(self.catalogue.root())?;
        let operation_id = allocate_id(connection, "operation_intents")?;
        let artifact_id = match preparation.replacement {
            Some(replacement) => replacement.artifact.id.clone(),
            None => allocate_id(connection, "artifacts")?,
        };
        let revision_id = allocate_id(connection, "revisions")?;
        let artifact = self.artifact_representation(NewArtifact {
            artifact_id: &artifact_id,
            revision_id: &revision_id,
            title: &title,
            description: &description,
            slug: &slug,
            project,
            retention,
            files: source.files(),
            logical_bytes: source.logical_bytes(),
            published_at: &published_at,
        });
        let artifact = replacement_artifact_representation(artifact, preparation.replacement);
        let revision = self.revision_representation(NewRevision {
            revision_id: &revision_id,
            artifact_id: &artifact_id,
            entry_path: &entry_path,
            entry_media_type: &entry_media_type,
            files: source.files(),
            logical_bytes: source.logical_bytes(),
            manifest_digest: "",
            published_at: &published_at,
        });
        let intent = PublishIntent {
            protocol: "publish_artifact_v2".into(),
            operation_id: operation_id.clone(),
            phase: PublishPhase::IntentRecorded,
            artifact,
            revision,
            payload_digest: None,
            source_snapshot_digest: source.snapshot_digest(),
            source_root_snapshot_digest: source.root_snapshot_digest(),
            capacity_reservation_bytes: source.logical_bytes(),
            idempotency_key: preparation.idempotency_key.to_owned(),
            fingerprint: preparation.fingerprint.to_owned(),
            publication_method: Some(preparation.publication_method),
            request_identity: (preparation.publication_method == PublicationMethod::Import)
                .then(|| import_request_identity(preparation.idempotency_key)),
            warnings,
            replacement: preparation.replacement.map(replacement_intent),
        };
        record_publish_intent(
            connection,
            &intent,
            PublishReservation {
                idempotency_key: preparation.idempotency_key,
                fingerprint: preparation.fingerprint,
                project_id: &request.project_id,
                required_bytes: source.logical_bytes(),
                capacity: CapacityPolicy {
                    max_stored_bytes: self.max_stored_bytes,
                    max_live_artifacts: self.max_live_artifacts,
                    filesystem_available_bytes: filesystem_capacity.0,
                    reserve_bytes: filesystem_capacity.1,
                },
                kind: if preparation.replacement.is_some() {
                    ReservationKind::Replace
                } else {
                    ReservationKind::Publish
                },
            },
        )?;
        publish_fault(
            "OBS_TEST_FAIL_PUBLISH_AFTER_INTENT",
            "durable Publish intent",
        )?;
        publish_test_hold_after_intent()?;
        Ok(PreparedPublish {
            source,
            operation_id,
            artifact_id,
            revision_id,
            entry_path,
            entry_media_type,
            published_at,
            intent,
        })
    }

    fn persist_publish(
        &self,
        connection: &mut Connection,
        mut prepared: PreparedPublish,
        idempotency_key: &str,
    ) -> Result<PublishOutcome, AppError> {
        let staged = match self.storage.stage_source(
            &mut prepared.source,
            StageRequest {
                operation_id: &prepared.operation_id,
                artifact_id: &prepared.artifact_id,
                revision_id: &prepared.revision_id,
                entry_path: &prepared.entry_path,
                entry_media_type: &prepared.entry_media_type,
                published_at: &prepared.published_at,
            },
        ) {
            Ok(staged) => staged,
            Err(error) => {
                return Err(self.record_publish_failure(
                    connection,
                    &prepared,
                    idempotency_key,
                    error,
                ));
            }
        };
        publish_fault(
            "OBS_TEST_FAIL_PUBLISH_AFTER_STAGE_SYNC",
            "durable stage sync",
        )?;
        let staged_intent = staged_intent(prepared.intent.clone(), &staged);
        update_publish_phase(connection, &staged_intent, PublishPhase::Staged)?;
        publish_fault("OBS_TEST_FAIL_PUBLISH_AFTER_STAGED", "durable staged phase")?;
        let finalized = match self.storage.finalize(staged) {
            Ok(finalized) => finalized,
            Err(error) => {
                return Err(self.record_publish_failure(
                    connection,
                    &prepared,
                    idempotency_key,
                    error,
                ));
            }
        };
        if let Err(error) = prepared.source.verify_unchanged() {
            return Err(self.record_publish_failure(connection, &prepared, idempotency_key, error));
        }
        let completed = completed_intent(staged_intent, &finalized);
        publish_fault(
            "OBS_TEST_FAIL_PUBLISH_AFTER_FINALIZE",
            "durable Revision finalize",
        )?;
        update_publish_phase(connection, &completed, PublishPhase::Renamed)?;
        publish_fault(
            "OBS_TEST_FAIL_PUBLISH_AFTER_RENAME",
            "durable Revision rename",
        )?;
        commit_publish_visibility(connection, &completed, idempotency_key)
    }

    fn record_publish_failure(
        &self,
        connection: &Connection,
        prepared: &PreparedPublish,
        idempotency_key: &str,
        error: AppError,
    ) -> AppError {
        let quarantine_error = self
            .storage
            .quarantine_interrupted(&prepared.operation_id, &prepared.revision_id)
            .err();
        let recorded_error = match quarantine_error.as_ref() {
            Some(cleanup_error) => error.with_cleanup_error(cleanup_error),
            None => error,
        };
        match fail_publish_intent(
            connection,
            &prepared.operation_id,
            idempotency_key,
            &recorded_error,
            true,
        ) {
            Ok(()) => recorded_error,
            Err(persistence_error) => {
                let cleanup_error = match quarantine_error {
                    Some(quarantine_error) => AppError::internal(format!(
                        "Publish quarantine and failure persistence both failed: {quarantine_error}; {persistence_error}"
                    )),
                    None => persistence_error,
                };
                recorded_error.with_cleanup_error(&cleanup_error)
            }
        }
    }

    pub(crate) fn show_artifact(&self, id: &str) -> Result<Artifact, AppError> {
        validate_opaque_id(id)?;
        let connection = self.catalogue.connection()?;
        let artifact = artifact_by_id(&connection, id, &self.canonical_origin)?
            .ok_or_else(|| AppError::not_found("Artifact does not exist"))?;
        if artifact.state == "gone" {
            return Err(AppError::gone(
                "artifact_gone",
                "Artifact identity is permanently gone",
            ));
        }
        Ok(artifact)
    }

    pub(crate) fn list_revisions(
        &self,
        artifact_id: &str,
        query: &ListRevisionsQuery,
    ) -> Result<RevisionList, AppError> {
        self.show_artifact(artifact_id)?;
        let connection = self.catalogue.connection()?;
        let normalized = normalize_revision_list(&connection, artifact_id, query)?;
        let availability = normalized.availability.as_str();
        let order = normalized.order.as_str();
        let direction = normalized.direction;
        let limit = normalized.limit;
        let cursor = normalized.cursor;
        let boundary_value = cursor.as_ref().map(|cursor| cursor.last_value.as_str());
        let boundary_id = cursor.as_ref().map(|cursor| cursor.last_id.as_str());
        let value_expression = if order == "superseded" {
            "coalesce(superseded_at,published_at)"
        } else {
            "published_at"
        };
        let comparison = match direction {
            ListDirection::Asc => ">",
            ListDirection::Desc => "<",
        };
        let ordering = match direction {
            ListDirection::Asc => "ASC",
            ListDirection::Desc => "DESC",
        };
        let sql = format!(
            "SELECT id,artifact_id,state,entry_path,entry_media_type,files,logical_bytes,
                    manifest_digest,published_at,superseded_at
             FROM revisions
             WHERE artifact_id=?1
               AND (?2='all' OR state=?2)
               AND (?3 IS NULL OR {value_expression} {comparison} ?3
                    OR ({value_expression}=?3 AND id>?4))
             ORDER BY {value_expression} {ordering},id ASC LIMIT ?5"
        );
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        let rows = statement
            .query_map(
                params![
                    artifact_id,
                    availability,
                    boundary_value,
                    boundary_id,
                    limit + 1
                ],
                |row| revision_from_row(row, &self.canonical_origin),
            )
            .map_err(database_error)?;
        let mut items = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        let has_more = items.len() > usize::from(limit);
        if has_more {
            items.truncate(usize::from(limit));
        }
        let next_cursor = if has_more {
            let last = items
                .last()
                .ok_or_else(|| AppError::internal("Revision page boundary is missing"))?;
            Some(cursor::encode(
                &connection,
                &RevisionCursor {
                    endpoint: format!("artifact-revisions:{artifact_id}"),
                    artifact_id: artifact_id.to_owned(),
                    availability: availability.to_owned(),
                    order: order.to_owned(),
                    direction,
                    last_value: revision_order_value(last, order).to_owned(),
                    last_id: last.id.clone(),
                    expires_at_ms: (OffsetDateTime::now_utc() + Duration::minutes(15))
                        .unix_timestamp_nanos()
                        / 1_000_000,
                },
            )?)
        } else {
            None
        };
        Ok(RevisionList {
            items,
            page: ArtifactPage {
                limit,
                next_cursor,
                has_more,
            },
        })
    }

    pub(crate) fn show_revision(&self, id: &str) -> Result<Revision, AppError> {
        validate_opaque_id(id)?;
        let connection = self.catalogue.connection()?;
        let revision = revision_by_id(&connection, id, &self.canonical_origin)?
            .ok_or_else(|| AppError::not_found("Revision does not exist"))?;
        if revision.state == "gone" {
            return Err(AppError::gone(
                "revision_gone",
                "Revision identity is permanently gone",
            ));
        }
        Ok(revision)
    }

    pub(crate) fn list(&self, query: ListArtifactsQuery) -> Result<ArtifactList, AppError> {
        let normalized = normalize_artifact_list(query, &self.catalogue.connection()?)?;
        let connection = self.catalogue.connection()?;
        let title_order = normalized.order == "title";
        let (comparison, ordering) = match (title_order, normalized.direction) {
            (true, ListDirection::Asc) => (
                "a.title_fold > ?5 OR (a.title_fold = ?5 AND a.id > ?6)",
                "a.title_fold ASC, a.id ASC",
            ),
            (true, ListDirection::Desc) => (
                "a.title_fold < ?5 OR (a.title_fold = ?5 AND a.id > ?6)",
                "a.title_fold DESC, a.id ASC",
            ),
            (false, ListDirection::Asc) => (
                "a.published_at > ?5 OR (a.published_at = ?5 AND a.id > ?6)",
                "a.published_at ASC, a.id ASC",
            ),
            (false, ListDirection::Desc) => (
                "a.published_at < ?5 OR (a.published_at = ?5 AND a.id > ?6)",
                "a.published_at DESC, a.id ASC",
            ),
        };
        let boundary_value = normalized
            .cursor
            .as_ref()
            .map(|cursor| cursor.last_value.clone());
        let boundary_id = normalized
            .cursor
            .as_ref()
            .map(|cursor| cursor.last_id.clone());
        let retention_mode = normalized.retention_mode.map(RetentionMode::as_str);
        let mut statement = connection
            .prepare(&format!(
                "{} WHERE a.state='live'
                   AND (?1 IS NULL OR a.project_id=?1)
                   AND (?2 IS NULL OR a.retention_mode=?2)
                   AND (?3='' OR instr(a.search_text,?3)>0)
                   AND (?5 IS NULL OR {})
                 ORDER BY {} LIMIT ?4",
                artifact_select(),
                comparison,
                ordering
            ))
            .map_err(database_error)?;
        let rows = statement
            .query_map(
                params![
                    normalized.project_id,
                    retention_mode,
                    normalized.query,
                    u64::from(normalized.limit) + 1,
                    boundary_value,
                    boundary_id
                ],
                |row| artifact_from_row(row, &self.canonical_origin),
            )
            .map_err(database_error)?;
        let mut items = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        let has_more = items.len() > usize::from(normalized.limit);
        if has_more {
            items.pop();
        }
        let next_cursor = if has_more {
            let last = items
                .last()
                .ok_or_else(|| AppError::internal("Artifact page boundary is missing"))?;
            Some(cursor::encode(
                &connection,
                &ArtifactCursor {
                    endpoint: normalized.cursor_endpoint.clone(),
                    project_id: normalized.project_id.clone(),
                    state: normalized.state.clone(),
                    retention_mode: normalized.retention_mode,
                    query: normalized.query.clone(),
                    order: normalized.order.clone(),
                    direction: normalized.direction,
                    last_value: if title_order {
                        default_case_fold_str(last.title())
                    } else {
                        last.published_at().to_owned()
                    },
                    last_id: last.id().to_owned(),
                    expires_at_ms: (OffsetDateTime::now_utc() + Duration::minutes(15))
                        .unix_timestamp_nanos()
                        / 1_000_000,
                },
            )?)
        } else {
            None
        };
        Ok(ArtifactList {
            items,
            page: ArtifactPage {
                limit: normalized.limit,
                next_cursor,
                has_more,
            },
        })
    }

    pub(crate) fn serve_artifact(
        &self,
        id: &str,
        requested_member: Option<&str>,
    ) -> Result<ServedRevision, AppError> {
        validate_opaque_id(id)?;
        let connection = self.catalogue.connection()?;
        let record = connection
            .query_row(
                "SELECT a.slug || '~' || a.id, r.id, r.entry_path, r.entry_media_type, r.manifest_digest
                 FROM artifacts a
                 JOIN revisions r ON r.id = a.current_revision_id
                 WHERE a.id=?1 AND a.state='live' AND r.state IN ('current','superseded')",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(database_error)?;
        let Some(record) = record else {
            return Err(artifact_serving_absence(&connection, id)?);
        };
        let member = requested_member.unwrap_or(&record.2);
        let bytes = self
            .storage
            .read_revision_member(&record.1, member, &record.4)?;
        Ok(ServedRevision {
            bytes,
            media_type: member_media_type(member, &record.2, &record.3),
            artifact_key: Some(record.0),
        })
    }

    pub(crate) fn serve_revision(
        &self,
        id: &str,
        requested_member: Option<&str>,
    ) -> Result<ServedRevision, AppError> {
        validate_opaque_id(id)?;
        let connection = self.catalogue.connection()?;
        let record = connection
            .query_row(
                "SELECT entry_path,entry_media_type,manifest_digest
                 FROM revisions WHERE id=?1 AND state IN ('current','superseded')",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(database_error)?;
        let Some(record) = record else {
            return Err(revision_serving_absence(&connection, id)?);
        };
        let member = requested_member.unwrap_or(&record.0);
        let bytes = self.storage.read_revision_member(id, member, &record.2)?;
        Ok(ServedRevision {
            bytes,
            media_type: member_media_type(member, &record.0, &record.1),
            artifact_key: None,
        })
    }

    fn artifact_representation(&self, input: NewArtifact<'_>) -> Artifact {
        let key = format!("{}~{}", input.slug, input.artifact_id);
        Artifact {
            kind: "artifact".into(),
            id: input.artifact_id.to_owned(),
            key: key.clone(),
            record_version: 1,
            state: "live".into(),
            title: input.title.to_owned(),
            description: input.description.to_owned(),
            slug: input.slug.to_owned(),
            detail_url: format!(
                "{}ui/projects/{}/artifacts/{key}/",
                self.canonical_origin, input.project.key
            ),
            project: input.project,
            current_revision_id: input.revision_id.to_owned(),
            retention: input.retention,
            files: input.files,
            logical_bytes: input.logical_bytes,
            revision_count: 1,
            published_at: input.published_at.to_owned(),
            updated_at: input.published_at.to_owned(),
            api_url: format!(
                "{}api/v1/artifacts/{}",
                self.canonical_origin, input.artifact_id
            ),
            open_url: format!("{}artifacts/{key}/", self.canonical_origin),
        }
    }

    fn revision_representation(&self, input: NewRevision<'_>) -> Revision {
        Revision {
            kind: "revision".into(),
            id: input.revision_id.to_owned(),
            artifact_id: input.artifact_id.to_owned(),
            state: "current".into(),
            entry_path: input.entry_path.to_owned(),
            entry_media_type: input.entry_media_type.to_owned(),
            files: input.files,
            logical_bytes: input.logical_bytes,
            manifest_digest: input.manifest_digest.to_owned(),
            published_at: input.published_at.to_owned(),
            superseded_at: None,
            api_url: format!(
                "{}api/v1/revisions/{}",
                self.canonical_origin, input.revision_id
            ),
            open_url: format!("{}revisions/{}/", self.canonical_origin, input.revision_id),
        }
    }

    fn begin_mutation(&self, key: &str, fingerprint: &str) -> Result<MutationGuard, AppError> {
        let mut in_flight = lock_in_flight(&self.in_flight)?;
        if let Some(active) = in_flight.get(key) {
            return Err(if active == fingerprint {
                AppError::retryable_conflict(
                    "idempotency_in_progress",
                    "an identical Artifact Publish is already in progress",
                )
            } else {
                AppError::conflict(
                    "idempotency_conflict",
                    "Idempotency-Key was reused with a different request",
                )
            });
        }
        in_flight.insert(key.to_owned(), fingerprint.to_owned());
        Ok(MutationGuard {
            key: key.to_owned(),
            in_flight: self.in_flight.clone(),
        })
    }
}

impl PublishArtifactRequest {
    pub(crate) fn new(
        source_path: String,
        caller_working_directory: String,
        project_id: String,
    ) -> Self {
        Self {
            source: PublishSource {
                path: source_path,
                caller_working_directory,
            },
            project_id,
            entry: None,
            title: None,
            description: None,
            slug: None,
            retention: PublishRetention {
                mode: RetentionMode::Default,
                ttl_ms: None,
                pin_reason: None,
            },
        }
    }

    pub(crate) fn with_entry(mut self, entry: Option<String>) -> Self {
        self.entry = entry;
        self
    }

    pub(crate) fn with_metadata(
        mut self,
        title: Option<String>,
        description: Option<String>,
        slug: Option<String>,
    ) -> Self {
        self.title = title;
        self.description = description;
        self.slug = slug;
        self
    }

    pub(crate) fn with_retention(
        mut self,
        mode: RetentionMode,
        ttl_ms: Option<u64>,
        pin_reason: Option<String>,
    ) -> Self {
        self.retention = PublishRetention {
            mode,
            ttl_ms,
            pin_reason,
        };
        self
    }
}

impl ReplaceArtifactRequest {
    pub(crate) fn new(source_path: String, caller_working_directory: String) -> Self {
        Self {
            source: PublishSource {
                path: source_path,
                caller_working_directory,
            },
            entry: None,
            title: None,
            description: None,
            slug: None,
            retention: None,
        }
    }

    pub(crate) fn with_entry(mut self, entry: Option<String>) -> Self {
        self.entry = entry;
        self
    }

    pub(crate) fn with_metadata(
        mut self,
        title: Option<String>,
        description: Option<String>,
        slug: Option<String>,
    ) -> Self {
        self.title = title;
        self.description = description;
        self.slug = slug;
        self
    }

    pub(crate) fn with_retention(
        mut self,
        retention: Option<(RetentionMode, Option<u64>, Option<String>)>,
    ) -> Self {
        self.retention = retention.map(|(mode, ttl_ms, pin_reason)| PublishRetention {
            mode,
            ttl_ms,
            pin_reason,
        });
        self
    }
}

impl ListArtifactsQuery {
    pub(crate) fn next_link(
        &self,
        canonical_origin: &str,
        cursor: &str,
    ) -> Result<String, AppError> {
        let mut url =
            url::Url::parse(&format!("{canonical_origin}api/v1/artifacts")).map_err(|error| {
                AppError::internal(format!("Artifact list URL is invalid: {error}"))
            })?;
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in [
                ("projectId", self.project_id.as_deref()),
                ("state", self.state.as_deref()),
                (
                    "retentionMode",
                    self.retention_mode.map(RetentionFilter::as_str),
                ),
                ("query", self.query.as_deref()),
                ("order", self.order.as_deref()),
                ("direction", self.direction.as_deref()),
            ] {
                if let Some(value) = value {
                    pairs.append_pair(name, value);
                }
            }
            if let Some(limit) = self.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
            pairs.append_pair("after", cursor);
        }
        Ok(url.into())
    }

    pub(crate) fn for_project(project_id: String) -> Self {
        Self {
            project_id: Some(project_id),
            state: Some("live".into()),
            retention_mode: None,
            query: None,
            order: Some("recent".into()),
            direction: Some("desc".into()),
            limit: Some(200),
            after: None,
            cursor_endpoint: None,
        }
    }

    pub(crate) fn from_ledger(
        query: &LedgerQuery,
        scoped_project_id: Option<&str>,
    ) -> Option<Self> {
        if query.kind.as_deref() == Some("service") {
            return None;
        }
        let project_id = scoped_project_id
            .map(str::to_owned)
            .or_else(|| query.project_id.clone());
        Some(Self {
            project_id,
            state: Some("live".into()),
            retention_mode: None,
            query: query.query.clone(),
            order: query.order.clone(),
            direction: query.direction.clone(),
            limit: query.limit,
            after: query.after.clone(),
            cursor_endpoint: Some(format!(
                "project-ledger:{}:{}",
                if scoped_project_id.is_some() {
                    "scoped"
                } else {
                    "all"
                },
                query.kind.as_deref().unwrap_or("all")
            )),
        })
    }
}

impl RevisionList {
    pub(crate) fn items(&self) -> &[Revision] {
        &self.items
    }

    pub(crate) fn next_cursor(&self) -> Option<&str> {
        self.page.next_cursor.as_deref()
    }
}

impl ListRevisionsQuery {
    pub(crate) fn next_link(
        &self,
        canonical_origin: &str,
        artifact_id: &str,
        cursor: &str,
    ) -> Result<String, AppError> {
        let mut url = url::Url::parse(&format!(
            "{canonical_origin}api/v1/artifacts/{artifact_id}/revisions"
        ))
        .map_err(|error| AppError::internal(format!("Revision list URL is invalid: {error}")))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in [
                ("availability", self.availability.as_deref()),
                ("order", self.order.as_deref()),
                ("direction", self.direction.as_deref()),
            ] {
                if let Some(value) = value {
                    pairs.append_pair(name, value);
                }
            }
            if let Some(limit) = self.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
            pairs.append_pair("after", cursor);
        }
        Ok(url.into())
    }
}

impl ArtifactList {
    pub(crate) fn empty(limit: u16) -> Self {
        Self {
            items: Vec::new(),
            page: ArtifactPage {
                limit,
                next_cursor: None,
                has_more: false,
            },
        }
    }

    pub(crate) fn items(&self) -> &[Artifact] {
        &self.items
    }

    pub(crate) fn next_cursor(&self) -> Option<&str> {
        self.page.next_cursor.as_deref()
    }
}

impl Revision {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn state(&self) -> &str {
        &self.state
    }

    pub(crate) fn open_url(&self) -> &str {
        &self.open_url
    }

    pub(crate) fn files(&self) -> u64 {
        self.files
    }

    pub(crate) fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    pub(crate) fn published_at(&self) -> &str {
        &self.published_at
    }
}

impl Artifact {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    pub(crate) fn project_key(&self) -> &str {
        &self.project.key
    }

    pub(crate) fn etag(&self) -> String {
        format!("\"rv-{}\"", self.record_version)
    }

    pub(crate) fn open_url(&self) -> &str {
        &self.open_url
    }

    pub(crate) fn current_revision_id(&self) -> &str {
        &self.current_revision_id
    }

    pub(crate) fn detail_url(&self) -> &str {
        &self.detail_url
    }

    pub(crate) const fn files(&self) -> u64 {
        self.files
    }

    pub(crate) const fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    pub(crate) const fn revision_count(&self) -> u64 {
        self.revision_count
    }

    pub(crate) fn published_at(&self) -> &str {
        &self.published_at
    }

    pub(crate) fn retention_label(&self) -> &'static str {
        match self.retention.mode {
            RetentionMode::Default => "EXPIRING · DEFAULT 30 DAYS",
            RetentionMode::Ttl => "EXPIRING · EXPLICIT TTL",
            RetentionMode::Pinned => "PINNED",
        }
    }

    pub(crate) fn retention_deadline(&self) -> Option<&str> {
        self.retention.expires_at.as_deref()
    }

    pub(crate) fn pin_reason(&self) -> Option<&str> {
        self.retention.pin_reason.as_deref()
    }
}

impl ImportOutcome {
    pub(crate) const fn result(&self) -> &ImportResult {
        &self.result
    }

    pub(crate) const fn replayed(&self) -> bool {
        self.replayed
    }
}

impl PublishOutcome {
    pub(crate) const fn result(&self) -> &PublishResult {
        &self.result
    }

    pub(crate) fn is_replacement(&self) -> bool {
        self.result.operation == "replace"
    }

    pub(crate) const fn replayed(&self) -> bool {
        self.replayed
    }

    pub(crate) fn etag(&self) -> String {
        self.result.artifact.etag()
    }

    pub(crate) fn location(&self) -> &str {
        &self.result.artifact.api_url
    }
}

struct MutationGuard {
    key: String,
    in_flight: Arc<Mutex<HashMap<String, String>>>,
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = self.in_flight.lock() {
            in_flight.remove(&self.key);
        }
    }
}

fn artifact_select() -> &'static str {
    "SELECT
       a.id,a.project_id,p.slug || '~' || p.id,a.record_version,a.state,
       a.title,a.description,a.slug,a.current_revision_id,a.retention_mode,
       a.ttl_ms,a.expires_at,a.pin_reason,a.recovery_until,a.files,
       a.logical_bytes,a.revision_count,a.published_at,a.updated_at
     FROM artifacts a JOIN projects p ON p.id=a.project_id"
}

fn artifact_by_id(
    connection: &Connection,
    id: &str,
    canonical_origin: &str,
) -> Result<Option<Artifact>, AppError> {
    connection
        .query_row(
            &format!("{} WHERE a.id=?1", artifact_select()),
            [id],
            |row| artifact_from_row(row, canonical_origin),
        )
        .optional()
        .map_err(database_error)
}

fn artifact_from_row(
    row: &rusqlite::Row<'_>,
    canonical_origin: &str,
) -> rusqlite::Result<Artifact> {
    let id = row.get::<_, String>(0)?;
    let project_id = row.get::<_, String>(1)?;
    let project_key = row.get::<_, String>(2)?;
    let slug = row.get::<_, String>(7)?;
    let key = format!("{slug}~{id}");
    let retention_mode = match row.get::<_, String>(9)?.as_str() {
        "ttl" => RetentionMode::Ttl,
        "pinned" => RetentionMode::Pinned,
        _ => RetentionMode::Default,
    };
    Ok(Artifact {
        kind: "artifact".into(),
        api_url: format!("{canonical_origin}api/v1/artifacts/{id}"),
        open_url: format!("{canonical_origin}artifacts/{key}/"),
        detail_url: format!("{canonical_origin}ui/projects/{project_key}/artifacts/{key}/"),
        id,
        key,
        record_version: row.get(3)?,
        state: row.get(4)?,
        title: row.get(5)?,
        description: row.get(6)?,
        slug,
        project: ProjectReference {
            id: project_id,
            key: project_key,
        },
        current_revision_id: row.get(8)?,
        retention: Retention {
            mode: retention_mode,
            ttl_ms: row.get(10)?,
            expires_at: row.get(11)?,
            pin_reason: row.get(12)?,
            recovery_until: row.get(13)?,
        },
        files: row.get(14)?,
        logical_bytes: row.get(15)?,
        revision_count: row.get(16)?,
        published_at: row.get(17)?,
        updated_at: row.get(18)?,
    })
}

fn artifact_serving_absence(connection: &Connection, id: &str) -> Result<AppError, AppError> {
    let state = connection
        .query_row("SELECT state FROM artifacts WHERE id=?1", [id], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .map_err(database_error)?;
    Ok(match state.as_deref() {
        Some("recoverable" | "gone") => {
            AppError::gone("artifact_gone", "Artifact is no longer available")
        }
        Some(_) => AppError::internal("Artifact current Revision is unavailable"),
        None => AppError::not_found("Artifact does not exist"),
    })
}

fn revision_serving_absence(connection: &Connection, id: &str) -> Result<AppError, AppError> {
    let state = connection
        .query_row("SELECT state FROM revisions WHERE id=?1", [id], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .map_err(database_error)?;
    Ok(match state.as_deref() {
        Some("gone") => AppError::gone("revision_gone", "Revision is no longer available"),
        Some("unavailable") => AppError::internal("Revision bytes are unavailable"),
        Some(_) => AppError::internal("Revision serving state is inconsistent"),
        None => AppError::not_found("Revision does not exist"),
    })
}

fn revision_by_id(
    connection: &Connection,
    id: &str,
    canonical_origin: &str,
) -> Result<Option<Revision>, AppError> {
    connection
        .query_row(
            "SELECT id,artifact_id,state,entry_path,entry_media_type,files,logical_bytes,manifest_digest,published_at,superseded_at
             FROM revisions WHERE id=?1",
            [id],
            |row| revision_from_row(row, canonical_origin),
        )
        .optional()
        .map_err(database_error)
}

fn revision_from_row(
    row: &rusqlite::Row<'_>,
    canonical_origin: &str,
) -> rusqlite::Result<Revision> {
    let id = row.get::<_, String>(0)?;
    Ok(Revision {
        kind: "revision".into(),
        api_url: format!("{canonical_origin}api/v1/revisions/{id}"),
        open_url: format!("{canonical_origin}revisions/{id}/"),
        id,
        artifact_id: row.get(1)?,
        state: row.get(2)?,
        entry_path: row.get(3)?,
        entry_media_type: row.get(4)?,
        files: row.get(5)?,
        logical_bytes: row.get(6)?,
        manifest_digest: row.get(7)?,
        published_at: row.get(8)?,
        superseded_at: row.get(9)?,
    })
}

fn decode_recovery_intent(operation_id: &str, encoded: &str) -> Result<PublishIntent, AppError> {
    let intent: PublishIntent = serde_json::from_str(encoded).map_err(|error| {
        AppError::internal(format!("interrupted Publish intent is invalid: {error}"))
    })?;
    if !matches!(
        intent.protocol.as_str(),
        "publish_single_file_v1" | "publish_artifact_v2"
    ) || intent.operation_id != operation_id
        || !valid_sha256_digest(&intent.source_snapshot_digest)
    {
        return Err(AppError::internal(
            "interrupted Publish intent identity is invalid",
        ));
    }
    Ok(intent)
}

fn valid_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn validate_recovery_idempotency(
    connection: &Connection,
    intent: &PublishIntent,
) -> Result<(), AppError> {
    let stored = connection
        .query_row(
            "SELECT fingerprint,state FROM idempotency_requests WHERE key=?1",
            [&intent.idempotency_key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(database_error)?;
    match stored {
        Some((fingerprint, state))
            if fingerprint == intent.fingerprint && state == "in_progress" =>
        {
            Ok(())
        }
        _ => Err(AppError::internal(
            "interrupted Publish idempotency record does not match its intent",
        )),
    }
}

fn mark_publish_awaiting_retry(
    connection: &mut Connection,
    intent: &PublishIntent,
) -> Result<(), AppError> {
    let transaction = connection.transaction().map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE operation_intents SET state='awaiting_retry'
             WHERE id=?1 AND state!='awaiting_retry'",
            [&intent.operation_id],
        )
        .map_err(database_error)?;
    if changed == 1 {
        transaction
            .execute(
                "INSERT INTO audit_events(
                   kind,details_json,at,actor,cause,resource_type,resource_id
                 ) VALUES (
                   'artifact_publish_recovery_awaiting_retry',?1,
                   strftime('%Y-%m-%dT%H:%M:%fZ','now'),'system',
                   'intent_recorded_without_bytes','artifact',?2
                 )",
                params![
                    serde_json::json!({
                        "operationId": intent.operation_id,
                        "revisionId": intent.revision.id
                    })
                    .to_string(),
                    intent.artifact.id,
                ],
            )
            .map_err(database_error)?;
    }
    transaction.commit().map_err(database_error)
}

fn resumable_publish(
    connection: &Connection,
    request: &PublishArtifactRequest,
    idempotency_key: &str,
    fingerprint: &str,
) -> Result<Option<PreparedPublish>, AppError> {
    let mut statement = connection
        .prepare(
            "SELECT details_json FROM operation_intents
             WHERE kind='artifact_publish' AND state='awaiting_retry' ORDER BY id",
        )
        .map_err(database_error)?;
    let encoded = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(database_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(database_error)?;
    drop(statement);
    let intent = encoded
        .into_iter()
        .map(|value| {
            serde_json::from_str::<PublishIntent>(&value).map_err(|error| {
                AppError::internal(format!("resumable Publish intent is invalid: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|intent| intent.idempotency_key == idempotency_key);
    let Some(intent) = intent else {
        return Ok(None);
    };
    if intent.fingerprint != fingerprint {
        return Err(AppError::conflict(
            "idempotency_conflict",
            "Idempotency-Key was reused with a different request",
        ));
    }
    let normalized_source = normalized_source_selection(&request.source.path)?;
    let source = ArtifactSource::open(Path::new(&normalized_source))?;
    if source.snapshot_digest() != intent.source_snapshot_digest
        || intent
            .source_root_snapshot_digest
            .as_ref()
            .is_some_and(|expected| source.root_snapshot_digest().as_ref() != Some(expected))
    {
        let error = AppError::source_changed();
        fail_publish_intent(
            connection,
            &intent.operation_id,
            &intent.idempotency_key,
            &error,
            false,
        )?;
        return Err(error);
    }
    let entry_path = source.entry_path(request.entry.as_deref())?;
    if entry_path != intent.revision.entry_path
        || source.logical_bytes() != intent.revision.logical_bytes
        || source.files() != intent.revision.files
        || entry_media_type(&entry_path)? != intent.revision.entry_media_type
    {
        let error = AppError::source_changed();
        fail_publish_intent(
            connection,
            &intent.operation_id,
            &intent.idempotency_key,
            &error,
            false,
        )?;
        return Err(error);
    }
    Ok(Some(PreparedPublish {
        source,
        operation_id: intent.operation_id.clone(),
        artifact_id: intent.artifact.id.clone(),
        revision_id: intent.revision.id.clone(),
        entry_path: intent.revision.entry_path.clone(),
        entry_media_type: intent.revision.entry_media_type.clone(),
        published_at: intent.revision.published_at.clone(),
        intent,
    }))
}

fn completed_publish(
    connection: &Connection,
    key: &str,
    fingerprint: &str,
    in_progress_is_none: bool,
) -> Result<Option<PublishOutcome>, AppError> {
    let stored = connection
        .query_row(
            "SELECT fingerprint,state,response_json FROM idempotency_requests WHERE key=?1",
            [key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(database_error)?;
    let Some((stored_fingerprint, state, response_json)) = stored else {
        return Ok(None);
    };
    if stored_fingerprint != fingerprint {
        return Err(AppError::conflict(
            "idempotency_conflict",
            "Idempotency-Key was reused with a different request",
        ));
    }
    if state == "failed_terminal" {
        let stored: StoredError = serde_json::from_str(
            response_json
                .as_deref()
                .ok_or_else(|| AppError::internal("failed Publish has no stored error"))?,
        )
        .map_err(|error| AppError::internal(format!("stored Publish error is invalid: {error}")))?;
        return Err(AppError::from_stored(stored));
    }
    if state != "completed" {
        if in_progress_is_none {
            return Ok(None);
        }
        return Err(AppError::retryable_conflict(
            "idempotency_in_progress",
            "an identical Artifact Publish is already in progress",
        ));
    }
    let result = serde_json::from_str(
        response_json
            .as_deref()
            .ok_or_else(|| AppError::internal("completed Publish has no stored result"))?,
    )
    .map_err(|error| AppError::internal(format!("stored Publish result is invalid: {error}")))?;
    Ok(Some(PublishOutcome {
        result,
        replayed: true,
    }))
}

#[cfg(feature = "test-faults")]
fn publish_test_hold_after_intent() -> Result<(), AppError> {
    let Some(value) = std::env::var_os("OBS_TEST_HOLD_PUBLISH_AFTER_INTENT_MS") else {
        return Ok(());
    };
    let milliseconds = value
        .to_str()
        .ok_or_else(|| AppError::internal("invalid Publish test hold"))?
        .parse::<u64>()
        .map_err(|error| AppError::internal(format!("invalid Publish test hold: {error}")))?;
    std::thread::sleep(std::time::Duration::from_millis(milliseconds));
    Ok(())
}

#[cfg(not(feature = "test-faults"))]
fn publish_test_hold_after_intent() -> Result<(), AppError> {
    Ok(())
}

#[cfg(feature = "test-faults")]
fn publish_fault(variable: &str, phase: &str) -> Result<(), AppError> {
    if std::env::var_os(variable).is_some() {
        Err(AppError::internal(format!(
            "injected Publish failure after {phase}"
        )))
    } else {
        Ok(())
    }
}

#[cfg(not(feature = "test-faults"))]
fn publish_fault(_variable: &str, _phase: &str) -> Result<(), AppError> {
    Ok(())
}

fn normalized_import_selection(source: &PublishSource) -> Option<String> {
    let selected = Path::new(&source.path);
    let absolute = if selected.is_absolute() {
        selected.to_path_buf()
    } else {
        Path::new(&source.caller_working_directory).join(selected)
    };
    normalized_source_selection(&absolute.to_string_lossy()).ok()
}

fn import_publish_request(
    request: &ImportArtifactsRequest,
    item: &ImportItem,
    normalized_selection: Option<&String>,
) -> Result<PublishArtifactRequest, AppError> {
    let source_path = normalized_selection
        .cloned()
        .ok_or_else(|| AppError::invalid("invalid_source", "source selection is invalid"))?;
    let options = item.options.as_ref().unwrap_or(&request.defaults);
    let default = &request.defaults;
    Ok(PublishArtifactRequest {
        source: PublishSource {
            path: source_path,
            caller_working_directory: item.source.caller_working_directory.clone(),
        },
        project_id: options
            .project_id
            .clone()
            .or_else(|| default.project_id.clone())
            .unwrap_or_else(|| request.project_id.clone()),
        entry: options.entry.clone().or_else(|| default.entry.clone()),
        title: options.title.clone().or_else(|| default.title.clone()),
        description: options
            .description
            .clone()
            .or_else(|| default.description.clone()),
        slug: options.slug.clone().or_else(|| default.slug.clone()),
        retention: options
            .retention
            .clone()
            .or_else(|| default.retention.clone())
            .unwrap_or_default(),
    })
}

fn import_fallback_label(item: &ImportItem, index: usize) -> String {
    Path::new(&item.source.path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 200
                && !name.chars().any(char::is_control)
                && !name.contains(['/', '\\'])
        })
        .map_or_else(|| format!("item-{index}"), str::to_owned)
}

fn import_label(item: &ImportItem, index: usize) -> Result<String, AppError> {
    let label = item
        .label
        .clone()
        .unwrap_or_else(|| import_fallback_label(item, index));
    if label.trim().is_empty()
        || label.len() > 200
        || label.chars().any(char::is_control)
        || label.contains('/')
        || label.contains('\\')
    {
        return Err(AppError::invalid(
            "invalid_import_label",
            "import label must be a nonempty safe label of at most 200 characters",
        ));
    }
    Ok(label)
}

fn aggregate_import_items(items: Vec<ImportItemOutcome>) -> Result<ImportResult, AppError> {
    let succeeded = items
        .iter()
        .filter(|item| {
            matches!(
                item.status,
                ImportItemStatus::Committed | ImportItemStatus::UnchangedReplay
            )
        })
        .count();
    let failed = items.len().saturating_sub(succeeded);
    let overall = if failed == 0 {
        ImportOverall::Complete
    } else if succeeded == 0 {
        ImportOverall::Failed
    } else {
        ImportOverall::Partial
    };
    Ok(ImportResult {
        operation: "import".into(),
        overall,
        partial: overall == ImportOverall::Partial,
        counts: ImportCounts {
            requested: u64::try_from(items.len())
                .map_err(|_| AppError::internal("import item count overflow"))?,
            succeeded: u64::try_from(succeeded)
                .map_err(|_| AppError::internal("import success count overflow"))?,
            failed: u64::try_from(failed)
                .map_err(|_| AppError::internal("import failure count overflow"))?,
            skipped: 0,
        },
        items,
    })
}

fn import_success(
    index: usize,
    label: String,
    outcome: &PublishOutcome,
    duplicate_candidates: Vec<String>,
) -> Result<ImportItemOutcome, AppError> {
    let published = outcome.result();
    Ok(ImportItemOutcome {
        index: u64::try_from(index).map_err(|_| AppError::internal("import index overflow"))?,
        label,
        status: if outcome.replayed() {
            ImportItemStatus::UnchangedReplay
        } else {
            ImportItemStatus::Committed
        },
        result: Some(ImportItemResult {
            artifact_id: published.artifact.id.clone(),
            artifact_key: published.artifact.key.clone(),
            artifact_record_version: published.artifact.record_version,
            artifact_api_url: published.artifact.api_url.clone(),
            revision_id: published.revision.id.clone(),
            revision_api_url: published.revision.api_url.clone(),
            revision_open_url: published.revision.open_url.clone(),
            open_url: published.artifact.open_url.clone(),
            detail_url: published.artifact.detail_url.clone(),
            files: published.artifact.files,
            logical_bytes: published.artifact.logical_bytes,
            retention: published.artifact.retention.clone(),
            warnings: published.warnings.clone(),
            duplicate_candidates,
        }),
        error: None,
        cleanup_error: None,
    })
}

fn import_failure(
    index: usize,
    label: String,
    error: &AppError,
) -> Result<ImportItemOutcome, AppError> {
    let failure = error.envelope().get("error").cloned();
    Ok(ImportItemOutcome {
        index: u64::try_from(index).map_err(|_| AppError::internal("import index overflow"))?,
        label,
        status: ImportItemStatus::Failed,
        result: None,
        error: failure,
        cleanup_error: error.cleanup_error(),
    })
}

fn import_item_key(request_key: &str, index: usize) -> String {
    let mut digest = Sha256::new();
    digest.update(b"observatory-import-v1");
    digest.update([0]);
    digest.update(request_key.as_bytes());
    digest.update([0]);
    digest.update(index.to_string().as_bytes());
    format!("{:x}", digest.finalize())
}

fn import_failure_is_trustworthy(
    connection: &Connection,
    item_key: &str,
) -> Result<bool, AppError> {
    let state = connection
        .query_row(
            "SELECT state FROM idempotency_requests WHERE key=?1",
            [item_key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(database_error)?;
    Ok(state
        .as_deref()
        .is_none_or(|state| state == "failed_terminal"))
}

fn import_request_identity(item_key: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(item_key.as_bytes()))
}

fn import_duplicate_candidates(
    connection: &Connection,
    item_key: &str,
    artifact_id: &str,
) -> Result<Vec<String>, AppError> {
    let content_fingerprint = connection
        .query_row(
            "SELECT json_extract(details_json,'$.payloadDigest')
             FROM operation_intents
             WHERE kind='artifact_publish' AND state='completed'
               AND json_extract(details_json,'$.idempotencyKey')=?1
               AND json_extract(details_json,'$.artifact.id')=?2",
            params![item_key, artifact_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(database_error)?;
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT a.id
             FROM operation_intents AS operation
             JOIN artifacts AS a
               ON a.id=json_extract(operation.details_json,'$.artifact.id')
             WHERE operation.kind='artifact_publish' AND operation.state='completed'
               AND a.state='live' AND a.id<>?1
               AND json_extract(operation.details_json,'$.payloadDigest')=?2
             ORDER BY a.published_at,a.id",
        )
        .map_err(database_error)?;
    statement
        .query_map(params![artifact_id, content_fingerprint], |row| row.get(0))
        .map_err(database_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(database_error)
}

fn import_fingerprint(
    request: &ImportArtifactsRequest,
    selections: &[Option<String>],
) -> Result<String, AppError> {
    let canonical = serde_jcs::to_vec(&serde_json::json!({
        "apiVersion": 1,
        "method": "POST",
        "route": "/api/v1/artifact-imports",
        "body": request,
        "normalizedSelections": selections,
    }))
    .map_err(|error| AppError::internal(format!("cannot fingerprint import: {error}")))?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn completed_import_batch(
    connection: &Connection,
    batch_key: &str,
    fingerprint: &str,
) -> Result<Option<ImportOutcome>, AppError> {
    let stored = connection
        .query_row(
            "SELECT fingerprint,state,response_json FROM idempotency_requests WHERE key=?1",
            [batch_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(database_error)?;
    let Some((stored_fingerprint, state, response)) = stored else {
        return Ok(None);
    };
    if stored_fingerprint != fingerprint {
        return Err(AppError::conflict(
            "idempotency_conflict",
            "Idempotency-Key was reused with a different import request",
        ));
    }
    if state != "completed" {
        return Ok(None);
    }
    let mut result: ImportResult = serde_json::from_str(
        response
            .as_deref()
            .ok_or_else(|| AppError::internal("completed import has no stored result"))?,
    )
    .map_err(|error| AppError::internal(format!("stored import result is invalid: {error}")))?;
    for item in &mut result.items {
        if item.status == ImportItemStatus::Committed {
            item.status = ImportItemStatus::UnchangedReplay;
        }
    }
    Ok(Some(ImportOutcome {
        result,
        replayed: true,
    }))
}

fn record_import_batch(
    connection: &mut Connection,
    batch_key: &str,
    fingerprint: &str,
) -> Result<(), AppError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO idempotency_requests(key,fingerprint,state) VALUES (?1,?2,'in_progress')",
            params![batch_key, fingerprint],
        )
        .map_err(database_error)?;
    transaction.commit().map_err(database_error)
}

fn complete_import_batch(
    connection: &mut Connection,
    batch_key: &str,
    fingerprint: &str,
    result: &ImportResult,
) -> Result<(), AppError> {
    let encoded = serde_json::to_string(result)
        .map_err(|error| AppError::internal(format!("cannot store import result: {error}")))?;
    let changed = connection
        .execute(
            "UPDATE idempotency_requests SET state='completed',status_code=200,response_json=?3,completed_at=?4
             WHERE key=?1 AND fingerprint=?2 AND state='in_progress'",
            params![batch_key, fingerprint, encoded, format_instant(observed_instant()?)?],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(AppError::conflict(
            "idempotency_conflict",
            "import batch idempotency state changed",
        ));
    }
    Ok(())
}

fn filesystem_capacity(root: &Path) -> Result<(u64, u64), AppError> {
    let capacity = statvfs(root)
        .map_err(|error| AppError::internal(format!("cannot inspect storage capacity: {error}")))?;
    let available_bytes = capacity.f_bavail.saturating_mul(capacity.f_frsize);
    let total_bytes = capacity.f_blocks.saturating_mul(capacity.f_frsize);
    let five_percent_ceiling = total_bytes / 20 + u64::from(total_bytes % 20 != 0);
    Ok((available_bytes, 1_073_741_824_u64.max(five_percent_ceiling)))
}

fn check_capacity(
    transaction: &Transaction<'_>,
    required_bytes: u64,
    policy: CapacityPolicy,
    kind: ReservationKind,
) -> Result<(), AppError> {
    let live_artifacts: u64 = transaction
        .query_row(
            "SELECT count(*) FROM artifacts WHERE state='live'",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    let pending_artifacts: u64 = transaction
        .query_row(
            "SELECT count(*) FROM operation_intents
             WHERE kind='artifact_publish'
               AND state IN ('intent_recorded','awaiting_retry','staged','renamed')
               AND json_extract(details_json,'$.replacement') IS NULL",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    let stored_bytes: u64 = transaction
        .query_row(
            "SELECT coalesce(sum(logical_bytes),0) FROM revisions",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    let reserved_bytes: u64 = transaction
        .query_row(
            "SELECT coalesce(sum(CAST(coalesce(
                 json_extract(details_json,'$.capacityReservationBytes'),
                 json_extract(details_json,'$.revision.logicalBytes')
             ) AS INTEGER)),0)
             FROM operation_intents
             WHERE kind='artifact_publish'
               AND state IN ('intent_recorded','awaiting_retry','staged','renamed','failed_terminal')",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    let unmaterialized_bytes: u64 = transaction
        .query_row(
            "SELECT coalesce(sum(CAST(coalesce(
                 json_extract(details_json,'$.capacityReservationBytes'),
                 json_extract(details_json,'$.revision.logicalBytes')
             ) AS INTEGER)),0)
             FROM operation_intents
             WHERE kind='artifact_publish' AND state IN ('intent_recorded','awaiting_retry')",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    let accounted_stored_bytes = stored_bytes.saturating_add(reserved_bytes);
    let projected_stored_bytes = accounted_stored_bytes.saturating_add(required_bytes);
    let projected_live_artifacts = live_artifacts
        .saturating_add(pending_artifacts)
        .saturating_add(u64::from(matches!(kind, ReservationKind::Publish)));
    let stored_blocked =
        policy.max_stored_bytes != 0 && projected_stored_bytes > policy.max_stored_bytes;
    let live_blocked =
        policy.max_live_artifacts != 0 && projected_live_artifacts > policy.max_live_artifacts;
    let required_filesystem_bytes = policy
        .reserve_bytes
        .saturating_add(unmaterialized_bytes)
        .saturating_add(required_bytes);
    let reserve_blocked = policy.filesystem_available_bytes < required_filesystem_bytes;
    if stored_blocked || live_blocked || reserve_blocked {
        return Err(AppError::capacity(serde_json::json!({
            "requiredBytes": required_bytes,
            "accountedStoredBytes": accounted_stored_bytes,
            "maxStoredBytes": policy.max_stored_bytes,
            "liveArtifacts": live_artifacts,
            "maxLiveArtifacts": policy.max_live_artifacts,
            "filesystemAvailableBytes": policy.filesystem_available_bytes,
            "reserveBytes": policy.reserve_bytes,
            "reclaimableBytes": 0,
            "blockingConstraint": if stored_blocked {
                "max_stored_bytes"
            } else if live_blocked {
                "max_live_artifacts"
            } else {
                "free_space_reserve"
            }
        })));
    }
    Ok(())
}

fn record_publish_intent(
    connection: &mut Connection,
    intent: &PublishIntent,
    reservation: PublishReservation<'_>,
) -> Result<(), AppError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    require_live_project(&transaction, reservation.project_id)?;
    check_capacity(
        &transaction,
        reservation.required_bytes,
        reservation.capacity,
        reservation.kind,
    )?;
    transaction
        .execute(
            "INSERT INTO idempotency_requests(key,fingerprint,state) VALUES (?1,?2,'in_progress')",
            params![reservation.idempotency_key, reservation.fingerprint],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO operation_intents(id,kind,state,details_json,project_id)
             VALUES (?1,'artifact_publish','intent_recorded',?2,?3)",
            params![
                intent.operation_id,
                serde_json::to_string(intent).map_err(|error| AppError::internal(format!(
                    "cannot record Publish intent: {error}"
                )))?,
                reservation.project_id,
            ],
        )
        .map_err(database_error)?;
    transaction.commit().map_err(database_error)
}

fn update_publish_phase(
    connection: &Connection,
    intent: &PublishIntent,
    phase: PublishPhase,
) -> Result<(), AppError> {
    let state = match phase {
        PublishPhase::IntentRecorded => "intent_recorded",
        PublishPhase::Staged => "staged",
        PublishPhase::Renamed => "renamed",
    };
    let changed = connection
        .execute(
            "UPDATE operation_intents SET state=?2, details_json=?3 WHERE id=?1 AND kind='artifact_publish' AND state NOT IN ('completed','failed_terminal')",
            params![
                intent.operation_id,
                state,
                serde_json::to_string(intent).map_err(|error| AppError::internal(format!("cannot update Publish intent: {error}")))?,
            ],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(AppError::internal(
            "Publish intent changed during storage finalization",
        ));
    }
    Ok(())
}

fn commit_publish_visibility(
    connection: &mut Connection,
    intent: &PublishIntent,
    idempotency_key: &str,
) -> Result<PublishOutcome, AppError> {
    let result = PublishResult {
        operation: if intent.replacement.is_some() {
            "replace".into()
        } else {
            "publish".into()
        },
        artifact: intent.artifact.clone(),
        revision: intent.revision.clone(),
        warnings: intent.warnings.clone(),
    };
    let response_json = serde_json::to_string(&result)
        .map_err(|error| AppError::internal(format!("cannot store Publish result: {error}")))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    require_live_project(&transaction, &intent.artifact.project.id)?;
    if let Some(replacement) = intent.replacement.as_ref() {
        commit_replacement(&transaction, intent, replacement)?;
    } else {
        insert_artifact(&transaction, &intent.artifact)?;
        insert_revision(&transaction, &intent.revision)?;
        transaction
            .execute(
                "UPDATE artifacts SET current_revision_id=?2 WHERE id=?1 AND current_revision_id IS NULL",
                params![intent.artifact.id, intent.revision.id],
            )
            .map_err(database_error)?;
    }
    insert_publish_audit(&transaction, intent)?;
    transaction
        .execute(
            "UPDATE operation_intents SET state='completed',details_json=?2 WHERE id=?1 AND state='renamed'",
            params![
                intent.operation_id,
                serde_json::to_string(intent).map_err(|error| AppError::internal(format!("cannot complete Publish intent: {error}")))?,
            ],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "UPDATE idempotency_requests SET state='completed',status_code=?2,response_json=?3,etag=?4,completed_at=?5 WHERE key=?1 AND state='in_progress'",
            params![
                idempotency_key,
                if intent.replacement.is_some() { 200 } else { 201 },
                response_json,
                intent.artifact.etag(),
                intent.artifact.published_at
            ],
        )
        .map_err(database_error)?;
    transaction.commit().map_err(database_error)?;
    Ok(PublishOutcome {
        result,
        replayed: false,
    })
}

fn commit_replacement(
    transaction: &Transaction<'_>,
    intent: &PublishIntent,
    replacement: &ReplacementIntent,
) -> Result<(), AppError> {
    let artifact = &intent.artifact;
    insert_revision(transaction, &intent.revision)?;
    let changed = transaction
        .execute(
            "UPDATE artifacts SET
               record_version=?2,title=?3,description=?4,slug=?5,title_fold=?6,search_text=?7,
               current_revision_id=?8,retention_mode=?9,ttl_ms=?10,expires_at=?11,pin_reason=?12,
               recovery_until=NULL,files=?13,logical_bytes=?14,revision_count=?15,
               published_at=?16,updated_at=?17
             WHERE id=?1 AND state='live' AND record_version=?18 AND current_revision_id=?19",
            params![
                artifact.id,
                artifact.record_version,
                artifact.title,
                artifact.description,
                artifact.slug,
                default_case_fold_str(&artifact.title),
                default_case_fold_str(&format!(
                    "{}\n{}\n{}",
                    artifact.title, artifact.description, artifact.slug
                )),
                intent.revision.id,
                retention_mode(&artifact.retention),
                artifact.retention.ttl_ms,
                artifact.retention.expires_at,
                artifact.retention.pin_reason,
                artifact.files,
                artifact.logical_bytes,
                artifact.revision_count,
                artifact.published_at,
                artifact.updated_at,
                replacement.expected_record_version,
                replacement.previous_revision_id,
            ],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(AppError::changed_record());
    }
    let superseded = transaction
        .execute(
            "UPDATE revisions SET state='superseded',superseded_at=?3
             WHERE id=?1 AND artifact_id=?2 AND state='current'",
            params![
                replacement.previous_revision_id,
                artifact.id,
                artifact.updated_at
            ],
        )
        .map_err(database_error)?;
    if superseded != 1 {
        return Err(AppError::changed_record());
    }
    Ok(())
}

fn insert_publish_audit(
    transaction: &Transaction<'_>,
    intent: &PublishIntent,
) -> Result<(), AppError> {
    let replacing = intent.replacement.is_some();
    let content_fingerprint = intent
        .payload_digest
        .as_deref()
        .ok_or_else(|| AppError::internal("completed Publish has no content fingerprint"))?;
    transaction
        .execute(
            "INSERT INTO audit_events(kind,details_json,at,actor,cause,resource_type,resource_id)
             VALUES (?1,?2,?3,'operator',?4,'artifact',?5)",
            params![
                if replacing {
                    "artifact_replaced"
                } else {
                    "artifact_published"
                },
                serde_json::json!({
                    "artifactId": intent.artifact.id,
                    "revisionId": intent.revision.id,
                    "previousRevisionId": intent
                        .replacement
                        .as_ref()
                        .map(|replacement| replacement.previous_revision_id.as_str()),
                    "logicalBytes": intent.artifact.logical_bytes,
                    "method": intent.publication_method().as_str(),
                    "contentFingerprint": content_fingerprint,
                    "requestIdentity": intent.request_identity
                })
                .to_string(),
                intent.artifact.published_at,
                intent.publication_method().as_str(),
                intent.artifact.id,
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn insert_artifact(transaction: &Transaction<'_>, artifact: &Artifact) -> Result<(), AppError> {
    transaction
        .execute(
            "INSERT INTO artifacts(
               id,project_id,record_version,state,title,description,slug,title_fold,search_text,
               current_revision_id,retention_mode,ttl_ms,expires_at,pin_reason,recovery_until,
               files,logical_bytes,revision_count,published_at,updated_at
             ) VALUES (?1,?2,?3,'live',?4,?5,?6,?7,?8,NULL,?9,?10,?11,?12,NULL,?13,?14,?15,?16,?17)",
            params![
                artifact.id,
                artifact.project.id,
                artifact.record_version,
                artifact.title,
                artifact.description,
                artifact.slug,
                default_case_fold_str(&artifact.title),
                default_case_fold_str(&format!("{}\n{}\n{}", artifact.title, artifact.description, artifact.slug)),
                retention_mode(&artifact.retention),
                artifact.retention.ttl_ms,
                artifact.retention.expires_at,
                artifact.retention.pin_reason,
                artifact.files,
                artifact.logical_bytes,
                artifact.revision_count,
                artifact.published_at,
                artifact.updated_at,
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn insert_revision(transaction: &Transaction<'_>, revision: &Revision) -> Result<(), AppError> {
    transaction
        .execute(
            "INSERT INTO revisions(id,artifact_id,state,entry_path,entry_media_type,files,logical_bytes,manifest_digest,published_at)
             VALUES (?1,?2,'current',?3,?4,?5,?6,?7,?8)",
            params![
                revision.id,
                revision.artifact_id,
                revision.entry_path,
                revision.entry_media_type,
                revision.files,
                revision.logical_bytes,
                revision.manifest_digest,
                revision.published_at,
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn fail_publish_intent(
    connection: &Connection,
    operation_id: &str,
    idempotency_key: &str,
    error: &AppError,
    retain_capacity_reservation: bool,
) -> Result<(), AppError> {
    let stored_error = serde_json::to_string(&error.stored()).map_err(|encode_error| {
        AppError::internal(format!("cannot store Publish error: {encode_error}"))
    })?;
    connection
        .execute_batch("BEGIN IMMEDIATE;")
        .map_err(database_error)?;
    let result = connection
        .execute(
            "UPDATE operation_intents
             SET state='failed_terminal',
                 details_json=json_set(
                     details_json,
                     '$.capacityReservationBytes',
                     CASE WHEN ?2 THEN coalesce(
                         json_extract(details_json,'$.capacityReservationBytes'),
                         json_extract(details_json,'$.revision.logicalBytes')
                     ) ELSE 0 END
                 )
             WHERE id=?1 AND state NOT IN ('completed','failed_terminal')",
            params![operation_id, retain_capacity_reservation],
        )
        .and_then(|_| {
            connection.execute(
                "UPDATE idempotency_requests SET state='failed_terminal',status_code=?2,response_json=?3,completed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE key=?1 AND state='in_progress'",
                params![idempotency_key, error.api_status(), stored_error],
            )
        });
    match result {
        Ok(_) => connection.execute_batch("COMMIT;").map_err(database_error),
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK;");
            Err(database_error(error))
        }
    }
}

fn staged_intent(mut intent: PublishIntent, staged: &StagedRevision) -> PublishIntent {
    intent.phase = PublishPhase::Staged;
    staged.revision_id().clone_into(&mut intent.revision.id);
    staged
        .entry_path()
        .clone_into(&mut intent.revision.entry_path);
    staged
        .entry_media_type()
        .clone_into(&mut intent.revision.entry_media_type);
    intent.revision.files = staged.files();
    intent.revision.logical_bytes = staged.logical_bytes();
    staged
        .manifest_digest()
        .clone_into(&mut intent.revision.manifest_digest);
    intent.artifact.logical_bytes = staged.logical_bytes();
    intent.payload_digest = Some(staged.payload_digest().to_owned());
    intent
}

fn completed_intent(mut intent: PublishIntent, finalized: &FinalizedRevision) -> PublishIntent {
    intent.phase = PublishPhase::Renamed;
    finalized.revision_id().clone_into(&mut intent.revision.id);
    finalized
        .entry_path()
        .clone_into(&mut intent.revision.entry_path);
    finalized
        .entry_media_type()
        .clone_into(&mut intent.revision.entry_media_type);
    intent.revision.files = finalized.files();
    intent.revision.logical_bytes = finalized.logical_bytes();
    finalized
        .manifest_digest()
        .clone_into(&mut intent.revision.manifest_digest);
    intent.artifact.files = finalized.files();
    intent.artifact.logical_bytes = finalized.logical_bytes();
    intent.payload_digest = Some(finalized.payload_digest().to_owned());
    intent
}

fn project_reference(connection: &Connection, id: &str) -> Result<ProjectReference, AppError> {
    validate_opaque_id(id)?;
    connection
        .query_row(
            "SELECT slug || '~' || id FROM projects WHERE id=?1 AND state='live'",
            [id],
            |row| {
                Ok(ProjectReference {
                    id: id.to_owned(),
                    key: row.get(0)?,
                })
            },
        )
        .optional()
        .map_err(database_error)?
        .ok_or_else(|| AppError::not_found("Project does not exist"))
}

fn require_live_project(transaction: &Transaction<'_>, id: &str) -> Result<(), AppError> {
    let found: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1 AND state='live')",
            [id],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if !found {
        return Err(AppError::not_found("Project does not exist"));
    }
    Ok(())
}

fn normalize_artifact_list(
    query: ListArtifactsQuery,
    connection: &Connection,
) -> Result<NormalizedArtifactList, AppError> {
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(AppError::invalid(
            "invalid_limit",
            "Artifact list limit must be 1..200",
        ));
    }
    let state = Some(query.state.clone().unwrap_or_else(|| "live".into()));
    let order = query.order.clone().unwrap_or_else(|| "recent".into());
    let direction = ListDirection::parse(query.direction.as_deref())?;
    let retention_mode = query
        .retention_mode
        .map(RetentionFilter::valid)
        .transpose()?;
    let folded_query = default_case_fold_str(query.query.as_deref().unwrap_or(""));
    validate_artifact_filters(&query, state.as_deref(), &order, &folded_query)?;
    let cursor = decode_artifact_cursor(
        connection,
        ArtifactCursorBinding {
            query: &query,
            state: state.as_deref(),
            retention_mode,
            folded_query: &folded_query,
            order: &order,
            direction,
        },
    )?;
    Ok(NormalizedArtifactList {
        cursor_endpoint: query
            .cursor_endpoint
            .unwrap_or_else(|| "artifacts".to_owned()),
        project_id: query.project_id,
        state,
        retention_mode,
        query: folded_query,
        order,
        direction,
        limit,
        cursor,
    })
}

fn validate_artifact_filters(
    query: &ListArtifactsQuery,
    state: Option<&str>,
    order: &str,
    folded_query: &str,
) -> Result<(), AppError> {
    if !matches!(state, Some("live" | "all")) {
        return Err(AppError::invalid(
            "invalid_filter",
            "invalid Artifact state",
        ));
    }
    if !matches!(order, "recent" | "title" | "attention") {
        return Err(AppError::invalid("invalid_order", "invalid Artifact order"));
    }
    if let Some(project_id) = query.project_id.as_deref() {
        validate_opaque_id(project_id)?;
    }
    if folded_query.chars().count() > 500 {
        return Err(AppError::invalid(
            "invalid_filter",
            "Artifact query exceeds 500 characters",
        ));
    }
    Ok(())
}

fn decode_artifact_cursor(
    connection: &Connection,
    binding: ArtifactCursorBinding<'_>,
) -> Result<Option<ArtifactCursor>, AppError> {
    let cursor = binding
        .query
        .after
        .as_deref()
        .map(|token| cursor::decode::<ArtifactCursor>(connection, token))
        .transpose()?;
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let now_ms = OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    if cursor.expires_at_ms <= now_ms {
        return Err(AppError::conflict("cursor_expired", "cursor has expired"));
    }
    let expected_endpoint = binding
        .query
        .cursor_endpoint
        .as_deref()
        .unwrap_or("artifacts");
    let matches = cursor.endpoint == expected_endpoint
        && cursor.project_id == binding.query.project_id
        && cursor.state.as_deref() == binding.state
        && cursor.retention_mode == binding.retention_mode
        && cursor.query == binding.folded_query
        && cursor.order == binding.order
        && cursor.direction == binding.direction
        && validate_opaque_id(&cursor.last_id).is_ok();
    if !matches {
        return Err(AppError::invalid(
            "invalid_cursor",
            "cursor does not match Artifact filters or order",
        ));
    }
    Ok(Some(cursor))
}

fn normalize_revision_list(
    connection: &Connection,
    artifact_id: &str,
    query: &ListRevisionsQuery,
) -> Result<NormalizedRevisionList, AppError> {
    let availability = query.availability.as_deref().unwrap_or("all");
    if !matches!(
        availability,
        "all" | "current" | "superseded" | "unavailable" | "gone"
    ) {
        return Err(AppError::invalid(
            "invalid_filter",
            "invalid Revision availability",
        ));
    }
    let order = query.order.as_deref().unwrap_or("published");
    if !matches!(order, "published" | "superseded") {
        return Err(AppError::invalid("invalid_order", "invalid Revision order"));
    }
    let direction = ListDirection::parse(query.direction.as_deref())?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(AppError::invalid(
            "invalid_limit",
            "limit must be between 1 and 200",
        ));
    }
    let cursor = decode_revision_cursor(
        connection,
        artifact_id,
        query,
        availability,
        order,
        direction,
    )?;
    Ok(NormalizedRevisionList {
        availability: availability.to_owned(),
        order: order.to_owned(),
        direction,
        limit,
        cursor,
    })
}

fn decode_revision_cursor(
    connection: &Connection,
    artifact_id: &str,
    query: &ListRevisionsQuery,
    availability: &str,
    order: &str,
    direction: ListDirection,
) -> Result<Option<RevisionCursor>, AppError> {
    let cursor = query
        .after
        .as_deref()
        .map(|token| cursor::decode::<RevisionCursor>(connection, token))
        .transpose()?;
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let now_ms = OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    if cursor.expires_at_ms <= now_ms {
        return Err(AppError::conflict("cursor_expired", "cursor has expired"));
    }
    if cursor.endpoint != format!("artifact-revisions:{artifact_id}")
        || cursor.artifact_id != artifact_id
        || cursor.availability != availability
        || cursor.order != order
        || cursor.direction != direction
        || validate_opaque_id(&cursor.last_id).is_err()
    {
        return Err(AppError::invalid(
            "invalid_cursor",
            "cursor does not match Revision filters or order",
        ));
    }
    Ok(Some(cursor))
}

fn revision_order_value<'a>(revision: &'a Revision, order: &str) -> &'a str {
    if order == "superseded" {
        revision
            .superseded_at
            .as_deref()
            .unwrap_or(&revision.published_at)
    } else {
        &revision.published_at
    }
}

fn normalized_source_selection(value: &str) -> Result<String, AppError> {
    let path = Path::new(value);
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(AppError::invalid(
                        "invalid_source",
                        "Artifact source path cannot traverse above the filesystem root",
                    ));
                }
            }
            Component::Prefix(_) => {
                return Err(AppError::invalid(
                    "invalid_source",
                    "Artifact source path uses an unsupported prefix",
                ));
            }
        }
    }
    normalized
        .into_os_string()
        .into_string()
        .map_err(|_| AppError::invalid("invalid_source", "Artifact source path is not UTF-8"))
}

fn validate_publish_paths(request: &PublishArtifactRequest) -> Result<(), AppError> {
    validate_source_paths(&request.source)
}

fn validate_source_paths(source: &PublishSource) -> Result<(), AppError> {
    if !Path::new(&source.path).is_absolute()
        || !Path::new(&source.caller_working_directory).is_absolute()
    {
        return Err(AppError::invalid(
            "invalid_source",
            "source path and callerWorkingDirectory must be absolute",
        ));
    }
    Ok(())
}

fn member_media_type(member_path: &str, entry_path: &str, entry_media_type: &str) -> String {
    if member_path == entry_path {
        entry_media_type.to_owned()
    } else {
        mime_guess::from_path(member_path)
            .first_raw()
            .unwrap_or("application/octet-stream")
            .to_owned()
    }
}

fn entry_media_type(entry_path: &str) -> Result<String, AppError> {
    let media_type = mime_guess::from_path(entry_path)
        .first_raw()
        .ok_or_else(|| AppError::unsupported_media("Artifact entry media type is unknown"))?;
    let excluded_text = matches!(
        media_type,
        "text/css" | "text/javascript" | "application/javascript" | "application/ecmascript"
    );
    let accepted = !excluded_text
        && (media_type.starts_with("text/")
            || media_type.starts_with("image/")
            || media_type.starts_with("audio/")
            || media_type.starts_with("video/")
            || media_type == "application/pdf"
            || media_type == "application/json"
            || media_type.ends_with("+json"));
    if !accepted {
        return Err(AppError::unsupported_media(format!(
            "{media_type} is not a supported Artifact entry media type"
        )));
    }
    Ok(media_type.to_owned())
}

fn artifact_title(
    request: &PublishArtifactRequest,
    entry_path: &str,
    media_type: &str,
    source: &mut ArtifactSource,
) -> Result<String, AppError> {
    if let Some(title) = request.title.as_deref().or_else(|| source.portable_title()) {
        return validated_title(title);
    }
    if media_type == "text/html" {
        let prefix = source.read_entry_prefix(entry_path, 1_048_576)?;
        if let Some(title) = html_title(&prefix) {
            return validated_title(&title);
        }
    }
    validated_title(source.source_basename().unwrap_or(entry_path))
}

fn html_title(bytes: &[u8]) -> Option<String> {
    let document = String::from_utf8_lossy(bytes);
    let folded = document.to_ascii_lowercase();
    let start = folded.find("<title")?;
    let content_start = folded[start..].find('>')? + start + 1;
    let content_end = folded[content_start..].find("</title>")? + content_start;
    Some(document[content_start..content_end].trim().to_owned())
}

fn validated_title(title: &str) -> Result<String, AppError> {
    let title = title.trim();
    if title.is_empty() || title.chars().any(char::is_control) {
        return Err(AppError::invalid(
            "invalid_artifact_title",
            "Artifact title must be nonempty and contain no controls",
        ));
    }
    Ok(title.to_owned())
}

fn artifact_slug(supplied: Option<&str>, title: &str) -> Result<String, AppError> {
    let normalized = route_slug::normalize(supplied.unwrap_or(title));
    if normalized.is_empty() && supplied.is_some() {
        return Err(AppError::invalid(
            "invalid_artifact_slug",
            "supplied Artifact slug normalizes to empty",
        ));
    }
    Ok(if normalized.is_empty() {
        "artifact".to_owned()
    } else {
        normalized
    })
}

fn retention(request: &PublishRetention, published: OffsetDateTime) -> Result<Retention, AppError> {
    match request.mode {
        RetentionMode::Default => {
            if request.ttl_ms.is_some() || request.pin_reason.is_some() {
                return Err(AppError::invalid(
                    "invalid_retention",
                    "default retention accepts neither ttlMs nor pinReason",
                ));
            }
            Ok(Retention {
                mode: RetentionMode::Default,
                ttl_ms: Some(DEFAULT_RETENTION_MS),
                expires_at: Some(format_instant(
                    published
                        + Duration::milliseconds(
                            i64::try_from(DEFAULT_RETENTION_MS)
                                .map_err(|_| AppError::internal("default retention overflow"))?,
                        ),
                )?),
                pin_reason: None,
                recovery_until: None,
            })
        }
        RetentionMode::Ttl => {
            let ttl_ms = request.ttl_ms.filter(|ttl| *ttl > 0).ok_or_else(|| {
                AppError::invalid("invalid_retention", "ttl retention requires positive ttlMs")
            })?;
            if request.pin_reason.is_some() {
                return Err(AppError::invalid(
                    "invalid_retention",
                    "ttl retention does not accept pinReason",
                ));
            }
            let duration = i64::try_from(ttl_ms)
                .map_err(|_| AppError::invalid("invalid_retention", "ttlMs is too large"))?;
            Ok(Retention {
                mode: RetentionMode::Ttl,
                ttl_ms: Some(ttl_ms),
                expires_at: Some(format_instant(
                    published + Duration::milliseconds(duration),
                )?),
                pin_reason: None,
                recovery_until: None,
            })
        }
        RetentionMode::Pinned => {
            if request.ttl_ms.is_some() {
                return Err(AppError::invalid(
                    "invalid_retention",
                    "pinned retention does not accept ttlMs",
                ));
            }
            Ok(Retention {
                mode: RetentionMode::Pinned,
                ttl_ms: None,
                expires_at: None,
                pin_reason: request.pin_reason.clone(),
                recovery_until: None,
            })
        }
    }
}

fn retention_mode(retention: &Retention) -> &'static str {
    match retention.mode {
        RetentionMode::Default => "default",
        RetentionMode::Ttl => "ttl",
        RetentionMode::Pinned => "pinned",
    }
}

fn replacement_slug(
    supplied: Option<&str>,
    title: &str,
    replacement: Option<&ReplacementState>,
) -> Result<String, AppError> {
    match (replacement, supplied) {
        (Some(replacement), None) => Ok(replacement.artifact.slug.clone()),
        (_, supplied) => artifact_slug(supplied, title),
    }
}

fn publish_warnings(source: &mut ArtifactSource) -> Result<Vec<PublishWarning>, AppError> {
    source
        .warnings()
        .map(|warnings| warnings.into_iter().map(publish_warning).collect())
}

fn publish_warning(warning: SourceWarning) -> PublishWarning {
    PublishWarning {
        code: warning.code.to_owned(),
        message: warning.message.to_owned(),
        member: warning.member,
    }
}

fn replacement_artifact_representation(
    mut artifact: Artifact,
    replacement: Option<&ReplacementState>,
) -> Artifact {
    if let Some(replacement) = replacement {
        artifact.record_version = replacement.artifact.record_version.saturating_add(1);
        artifact.revision_count = replacement.artifact.revision_count.saturating_add(1);
    }
    artifact
}

fn replacement_intent(replacement: &ReplacementState) -> ReplacementIntent {
    ReplacementIntent {
        previous_revision_id: replacement.artifact.current_revision_id.clone(),
        expected_record_version: replacement.expected_record_version,
    }
}

fn replacement_publish_request(
    request: &ReplaceArtifactRequest,
    current: &Artifact,
) -> PublishArtifactRequest {
    let retention = request
        .retention
        .clone()
        .unwrap_or_else(|| match current.retention.mode {
            RetentionMode::Default => PublishRetention::default(),
            RetentionMode::Ttl => PublishRetention {
                mode: RetentionMode::Ttl,
                ttl_ms: current.retention.ttl_ms,
                pin_reason: None,
            },
            RetentionMode::Pinned => PublishRetention {
                mode: RetentionMode::Pinned,
                ttl_ms: None,
                pin_reason: current.retention.pin_reason.clone(),
            },
        });
    PublishArtifactRequest {
        source: request.source.clone(),
        project_id: current.project.id.clone(),
        entry: request.entry.clone(),
        title: request.title.clone(),
        description: request.description.clone(),
        slug: request.slug.clone(),
        retention,
    }
}

fn replacement_fingerprint(
    artifact_id: &str,
    request: &ReplaceArtifactRequest,
    if_match: &str,
    normalized_source: &str,
) -> Result<String, AppError> {
    #[derive(Serialize)]
    struct Fingerprint<'a> {
        api_version: u8,
        method: &'static str,
        route: String,
        body: &'a ReplaceArtifactRequest,
        artifact_id: &'a str,
        if_match: &'a str,
        normalized_source: &'a str,
    }
    let canonical = serde_jcs::to_vec(&Fingerprint {
        api_version: 1,
        method: "POST",
        route: format!("/api/v1/artifacts/{artifact_id}/replace"),
        body: request,
        artifact_id,
        if_match,
        normalized_source,
    })
    .map_err(|error| AppError::internal(format!("cannot fingerprint replacement: {error}")))?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn publish_fingerprint(
    request: &PublishArtifactRequest,
    normalized_source: &str,
) -> Result<String, AppError> {
    #[derive(Serialize)]
    struct Fingerprint<'a> {
        api_version: u8,
        method: &'static str,
        route: &'static str,
        body: &'a PublishArtifactRequest,
        normalized_source: &'a str,
    }
    let canonical = serde_jcs::to_vec(&Fingerprint {
        api_version: 1,
        method: "POST",
        route: "/api/v1/artifacts",
        body: request,
        normalized_source,
    })
    .map_err(|error| AppError::internal(format!("cannot fingerprint Publish: {error}")))?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn allocate_id(connection: &Connection, table: &str) -> Result<String, AppError> {
    for _ in 0..16 {
        let id = random_opaque_id()?;
        let exists: bool = connection
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?1)"),
                [&id],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        if !exists {
            return Ok(id);
        }
    }
    Err(AppError::internal("cannot allocate unique opaque identity"))
}

fn validate_opaque_id(id: &str) -> Result<(), AppError> {
    const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";
    if id.len() == 26 && id.as_bytes()[0] <= b'7' && id.bytes().all(|byte| ALPHABET.contains(&byte))
    {
        return Ok(());
    }
    Err(AppError::invalid(
        "invalid_artifact_id",
        "Artifact or Revision ID is malformed",
    ))
}

fn validate_idempotency_key(key: &str) -> Result<(), AppError> {
    let valid = (8..=200).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\\' | b'\''));
    if valid {
        return Ok(());
    }
    Err(AppError::invalid(
        "invalid_idempotency_key",
        "Idempotency-Key must be 8..200 visible ASCII characters without quotes or backslashes",
    ))
}

fn lock_in_flight(
    in_flight: &Mutex<HashMap<String, String>>,
) -> Result<MutexGuard<'_, HashMap<String, String>>, AppError> {
    in_flight
        .lock()
        .map_err(|_| AppError::internal("Artifact mutation guard is poisoned"))
}

fn observed_instant() -> Result<OffsetDateTime, AppError> {
    let instant = OffsetDateTime::now_utc();
    instant
        .replace_nanosecond((instant.nanosecond() / 1_000_000) * 1_000_000)
        .map_err(|error| AppError::internal(format!("cannot normalize timestamp: {error}")))
}

fn format_instant(instant: OffsetDateTime) -> Result<String, AppError> {
    instant
        .format(&Rfc3339)
        .map_err(|error| AppError::internal(format!("cannot format timestamp: {error}")))
}

#[allow(clippy::needless_pass_by_value)]
fn database_error(error: rusqlite::Error) -> AppError {
    match error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked,
                ..
            },
            _,
        ) => AppError::contention("Artifact catalogue is busy"),
        error => AppError::internal(format!("Artifact catalogue failure: {error}")),
    }
}
