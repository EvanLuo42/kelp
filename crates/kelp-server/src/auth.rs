use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use kelp_core::Hash;

/// Hashes a password into a PHC string with argon2id.
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 accepts any password with default parameters")
        .to_string()
}

/// Returns `true` if `password` matches the PHC string `hash`.
pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// Returns the hash of a random password, for checking a password against
/// when there is no user, so that doing so takes as long as for a user.
pub fn dummy_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("operating system random source");
        hash_password(&blake3::Hash::from_bytes(bytes).to_hex())
    })
}

/// Generates a random access token and the hash under which it is stored.
pub fn new_token() -> (String, Hash) {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("operating system random source");
    let token = blake3::Hash::from_bytes(bytes).to_hex().to_string();
    let hash = token_hash(&token);
    (token, hash)
}

/// Returns the hash under which `token` is stored, so that the database never
/// holds usable tokens.
pub fn token_hash(token: &str) -> Hash {
    Hash::of(token.as_bytes())
}

const TOTP_STEP_MILLIS: u64 = 30_000;
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Generates a random 160-bit TOTP secret in base32.
pub fn new_totp_secret() -> String {
    let mut bytes = [0u8; 20];
    getrandom::getrandom(&mut bytes).expect("operating system random source");
    base32_encode(&bytes)
}

/// Returns the six-digit TOTP code of `secret` (base32) at `now`, in
/// milliseconds since the Unix epoch, as RFC 6238 defines it with SHA-1 and
/// 30-second steps.
///
/// # Panics
///
/// Panics if `secret` is not base32.
pub fn totp_code(secret: &str, now: u64) -> String {
    let key = base32_decode(secret).expect("a base32 secret");
    format!("{:06}", code_at(&key, now / TOTP_STEP_MILLIS))
}

/// Returns the time step at which `code` is valid for `secret` at `now`,
/// allowing one step of clock skew either way.
pub fn check_totp(secret: &str, code: &str, now: u64) -> Option<u64> {
    let key = base32_decode(secret)?;
    let code: u32 = code
        .trim()
        .parse()
        .ok()
        .filter(|_| code.trim().len() == 6)?;
    let step = now / TOTP_STEP_MILLIS;
    [step.saturating_sub(1), step, step + 1]
        .into_iter()
        .find(|&s| code_at(&key, s) == code)
}

fn code_at(key: &[u8], step: u64) -> u32 {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
    let tag = ring::hmac::sign(&key, &step.to_be_bytes());
    let mac = tag.as_ref();
    let offset = usize::from(mac[mac.len() - 1] & 0x0f);
    let bin = u32::from_be_bytes([
        mac[offset],
        mac[offset + 1],
        mac[offset + 2],
        mac[offset + 3],
    ]);
    (bin & 0x7fff_ffff) % 1_000_000
}

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0);
    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(BASE32[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(BASE32[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0);
    for c in text.trim_end_matches('=').bytes() {
        let value = BASE32.iter().position(|&b| b == c.to_ascii_uppercase())? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}
