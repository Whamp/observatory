use rustix::rand::{GetRandomFlags, getrandom};

use crate::error::AppError;

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

pub fn random_opaque_id() -> Result<String, AppError> {
    let mut value = u128::from_be_bytes(random_bytes::<16>()?);
    let mut encoded = [b'0'; 26];
    for character in encoded.iter_mut().rev() {
        let index = usize::try_from(value & 31)
            .map_err(|_| AppError::internal("opaque ID encoding failed"))?;
        *character = CROCKFORD[index];
        value >>= 5;
    }
    String::from_utf8(encoded.to_vec())
        .map_err(|error| AppError::internal(format!("opaque ID encoding failed: {error}")))
}

pub fn random_bytes<const LENGTH: usize>() -> Result<[u8; LENGTH], AppError> {
    let mut bytes = [0_u8; LENGTH];
    getrandom(&mut bytes, GetRandomFlags::empty()).map_err(|error| {
        AppError::internal(format!("secure randomness is unavailable: {error}"))
    })?;
    Ok(bytes)
}
