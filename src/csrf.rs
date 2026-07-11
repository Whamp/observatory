use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::crypto::random_bytes;
use crate::error::AppError;

const TOKEN_LIFETIME: Duration = Duration::from_mins(10);
const MAX_TOKENS: usize = 4_096;

#[derive(Clone, Debug, Default)]
pub struct CsrfStore {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
}

#[derive(Clone, Debug)]
struct Entry {
    scope: String,
    issued_at: Instant,
    expires_at: Instant,
}

impl CsrfStore {
    pub fn issue(&self, scope: &str) -> Result<String, AppError> {
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| AppError::internal("CSRF token store is unavailable"))?;
        entries.retain(|_, entry| entry.expires_at > now);
        if entries.len() >= MAX_TOKENS
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.issued_at)
                .map(|(token, _)| token.clone())
        {
            entries.remove(&oldest);
        }
        let token = URL_SAFE_NO_PAD.encode(random_bytes::<32>()?);
        entries.insert(
            token.clone(),
            Entry {
                scope: scope.to_owned(),
                issued_at: now,
                expires_at: now + TOKEN_LIFETIME,
            },
        );
        Ok(token)
    }

    pub fn consume(&self, token: &str, scope: &str) -> Result<(), AppError> {
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| AppError::internal("CSRF token store is unavailable"))?;
        let entry = entries.get(token).ok_or_else(csrf_rejected)?;
        if entry.expires_at <= now {
            entries.remove(token);
            return Err(csrf_rejected());
        }
        if entry.scope != scope {
            return Err(csrf_rejected());
        }
        entries.remove(token);
        Ok(())
    }
}

fn csrf_rejected() -> AppError {
    AppError::forbidden(
        "csrf_rejected",
        "browser mutation CSRF capability is missing, expired, replayed, or does not match the action",
    )
}
