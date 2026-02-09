// PKCE (Proof Key for Code Exchange) for OAuth 2.0.
//
// Generates a verifier (random bytes, base64url) and derives
// a challenge (SHA-256 hash, base64url) per RFC 7636.

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn generate_verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}
