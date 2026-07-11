use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use rusqlite::Connection;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Sha256;

use crate::error::AppError;

type HmacSha256 = Hmac<Sha256>;

pub(crate) fn encode<T: Serialize>(
    connection: &Connection,
    cursor: &T,
) -> Result<String, AppError> {
    let payload = serde_jcs::to_vec(cursor)
        .map_err(|error| AppError::internal(format!("cannot encode cursor: {error}")))?;
    let secret = cursor_secret(connection)?;
    let mut mac = HmacSha256::new_from_slice(&secret)
        .map_err(|_| AppError::internal("cursor secret is invalid"))?;
    mac.update(&payload);
    let signature = mac.finalize().into_bytes();
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

pub(crate) fn decode<T: DeserializeOwned>(
    connection: &Connection,
    token: &str,
) -> Result<T, AppError> {
    let Some((payload, signature)) = token.split_once('.') else {
        return Err(AppError::invalid("invalid_cursor", "cursor is malformed"));
    };
    if signature.contains('.') {
        return Err(AppError::invalid("invalid_cursor", "cursor is malformed"));
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AppError::invalid("invalid_cursor", "cursor is malformed"))?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| AppError::invalid("invalid_cursor", "cursor is malformed"))?;
    let secret = cursor_secret(connection)?;
    let mut mac = HmacSha256::new_from_slice(&secret)
        .map_err(|_| AppError::internal("cursor secret is invalid"))?;
    mac.update(&payload);
    mac.verify_slice(&signature)
        .map_err(|_| AppError::invalid("invalid_cursor", "cursor signature is invalid"))?;
    serde_json::from_slice(&payload)
        .map_err(|_| AppError::invalid("invalid_cursor", "cursor payload is invalid"))
}

fn cursor_secret(connection: &Connection) -> Result<Vec<u8>, AppError> {
    let secret = connection
        .query_row(
            "SELECT value FROM system_state WHERE key='cursor_secret'",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map_err(|error| AppError::internal(format!("cursor catalogue failure: {error}")))?;
    if secret.len() != 32 {
        return Err(AppError::internal("cursor secret has an invalid length"));
    }
    Ok(secret)
}
