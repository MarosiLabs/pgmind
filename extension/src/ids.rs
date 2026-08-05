//! UUIDv7 minting (RFC-004 A1): time-ordered identity, minted by the
//! extension on every supported Postgres major — never `gen_random_uuid()`,
//! never the client. Randomness comes from the server's own CSPRNG
//! (`pg_strong_random`), so no extra dependency and no userspace RNG state.

use pgrx::pg_sys;
use std::time::{SystemTime, UNIX_EPOCH};

/// RFC 9562 UUIDv7: 48-bit unix milliseconds ‖ ver=7 ‖ 12 rand ‖ var=10 ‖ 62 rand.
pub fn mint() -> pgrx::Uuid {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
    let ok =
        unsafe { pg_sys::pg_strong_random(bytes[6..].as_mut_ptr() as *mut core::ffi::c_void, 10) };
    if !ok {
        // pg_strong_random failing means the server's entropy source is broken;
        // erroring out is the only honest move (identity must never be guessable-degraded silently).
        pgrx::error!("pgmind: pg_strong_random failed while minting a block ID");
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x70; // version 7
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10
    pgrx::Uuid::from_bytes(bytes)
}
