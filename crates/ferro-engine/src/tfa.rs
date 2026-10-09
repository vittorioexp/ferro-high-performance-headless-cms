//! Two-factor authentication: time-based one-time passwords (RFC 6238, SHA-1, 6 digits, 30 s), recovery codes,
//! and the encryption of the shared secret at rest.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::{FerroError, Result};

pub const PERIOD_SECS: u64 = 30;
pub const DIGITS: u32 = 6;
/// Accepted drift: the previous and the next step, for clocks that are a few seconds off.
const WINDOW: i64 = 1;

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let v = ALPHABET.iter().position(|&a| a as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// A fresh 160-bit shared secret, base32-encoded (what authenticator apps expect).
pub fn generate_secret() -> String {
    let mut bytes = [0u8; 20];
    OsRng.fill_bytes(&mut bytes);
    base32_encode(&bytes)
}

/// The code for a time step (HOTP with the step as counter).
pub fn code_at(secret: &[u8], step: u64, digits: u32) -> u32 {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0x0f) as usize;
    let bin = ((h[o] as u32 & 0x7f) << 24) | ((h[o + 1] as u32) << 16) | ((h[o + 2] as u32) << 8) | h[o + 3] as u32;
    bin % 10u32.pow(digits)
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Check a 6-digit code against the secret. Returns the step it matched, which the caller must store: a step that is
/// not greater than `last_step` is refused, so a code (e.g. one that was shoulder-surfed) works only once.
pub fn verify(secret_b32: &str, code: &str, now_unix: u64, last_step: Option<i64>) -> Option<i64> {
    let code = code.trim();
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secret = base32_decode(secret_b32)?;
    let current = (now_unix / PERIOD_SECS) as i64;
    let mut matched = None;
    // no early exit: the time taken does not depend on which step matched
    for delta in -WINDOW..=WINDOW {
        let step = current + delta;
        if step < 0 {
            continue;
        }
        let expected = format!("{:0width$}", code_at(&secret, step as u64, DIGITS), width = DIGITS as usize);
        if constant_time_eq(&expected, code) && last_step.map(|l| step > l).unwrap_or(true) && matched.is_none() {
            matched = Some(step);
        }
    }
    matched
}

fn pct(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
}

/// `otpauth://` URI understood by Google Authenticator, Microsoft Authenticator, 1Password, ...
pub fn otpauth_url(issuer: &str, account: &str, secret: &str) -> String {
    format!(
        "otpauth://totp/{}:{}?secret={secret}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={PERIOD_SECS}",
        pct(issuer),
        pct(account),
        pct(issuer)
    )
}

/// The QR code of `data` as a standalone SVG document.
pub fn qr_svg(data: &str) -> Result<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).map_err(|e| FerroError::Internal(format!("cannot build the QR code: {e}")))?;
    Ok(code.render::<qrcode::render::svg::Color>().min_dimensions(220, 220).quiet_zone(true).build())
}

fn key_from(material: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"ferro-tfa-v1:");
    h.update(material.as_bytes());
    h.finalize().into()
}

/// Encrypt a secret for storage (AES-256-GCM, random nonce): `base64(nonce || ciphertext)`.
pub fn seal(key_material: &str, plain: &str) -> Result<String> {
    let cipher = Aes256Gcm::new(&key_from(key_material).into());
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
        .map_err(|_| FerroError::Internal("cannot encrypt the secret".into()))?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Ok(B64.encode(out))
}

pub fn open(key_material: &str, sealed: &str) -> Result<String> {
    let bad = || FerroError::Internal("cannot decrypt the two-factor secret: was FERRO_SECRET / FERRO_ENCRYPTION_KEY changed?".into());
    let raw = B64.decode(sealed).map_err(|_| bad())?;
    if raw.len() < 13 {
        return Err(bad());
    }
    let cipher = Aes256Gcm::new(&key_from(key_material).into());
    let plain = cipher.decrypt(Nonce::from_slice(&raw[..12]), &raw[12..]).map_err(|_| bad())?;
    String::from_utf8(plain).map_err(|_| bad())
}

/// Ten single-use codes, shown once: `xxxxx-xxxxx` (lowercase base32, unambiguous to type).
pub fn generate_recovery_codes(n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            let mut b = [0u8; 6];
            OsRng.fill_bytes(&mut b);
            let s = base32_encode(&b).to_ascii_lowercase();
            format!("{}-{}", &s[..5], &s[5..10])
        })
        .collect()
}

pub fn looks_like_recovery_code(s: &str) -> bool {
    let s = s.trim();
    s.len() == 11 && s.as_bytes()[5] == b'-' && s.chars().enumerate().all(|(i, c)| i == 5 || c.is_ascii_alphanumeric())
}

pub fn hash_recovery_code(code: &str) -> String {
    ferro_core::hash::sha256_hex(code.trim().to_ascii_lowercase().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn rfc_6238_test_vectors() {
        // Appendix B, SHA-1, 8 digits
        for (t, expected) in [(59u64, 94287082u32), (1111111109, 7081804), (1111111111, 14050471), (1234567890, 89005924), (2000000000, 69279037), (20000000000, 65353130)] {
            assert_eq!(code_at(RFC_SECRET, t / 30, 8), expected, "T={t}");
        }
        // the same secret with 6 digits (last 6 of the 8-digit values)
        assert_eq!(code_at(RFC_SECRET, 59 / 30, 6), 287082);
    }

    #[test]
    fn base32_roundtrip_and_known_value() {
        assert_eq!(base32_encode(RFC_SECRET), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(base32_decode("gezd gnbv GY3TQOJQ GEZDGNBVGY3TQOJQ==").unwrap(), RFC_SECRET);
        assert_eq!(base32_decode("not base32!"), None);
        for len in 0..40 {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(base32_decode(&base32_encode(&data)).unwrap(), data);
        }
        let s = generate_secret();
        assert_eq!(s.len(), 32);
        assert_ne!(s, generate_secret());
    }

    #[test]
    fn verify_accepts_the_window_once() {
        let secret = base32_encode(RFC_SECRET);
        let now = 1_111_111_109u64; // step 37037036
        let step = (now / 30) as i64;
        let code = |s: i64| format!("{:06}", code_at(RFC_SECRET, s as u64, 6));
        assert_eq!(verify(&secret, &code(step), now, None), Some(step));
        assert_eq!(verify(&secret, &code(step - 1), now, None), Some(step - 1), "previous step: slow phone clock");
        assert_eq!(verify(&secret, &code(step + 1), now, None), Some(step + 1), "next step: fast phone clock");
        assert_eq!(verify(&secret, &code(step + 2), now, None), None);
        assert_eq!(verify(&secret, &code(step - 2), now, None), None);
        // replay: once a step was used, the same or an older step is refused
        assert_eq!(verify(&secret, &code(step), now, Some(step)), None);
        assert_eq!(verify(&secret, &code(step - 1), now, Some(step)), None);
        assert_eq!(verify(&secret, &code(step + 1), now, Some(step)), Some(step + 1));
        // malformed input
        for bad in ["", "12345", "1234567", "12345a", "      ", "１２３４５６"] {
            assert_eq!(verify(&secret, bad, now, None), None, "{bad:?}");
        }
        assert_eq!(verify("not base32!", "123456", now, None), None);
        assert_eq!(verify(&secret, &format!(" {} ", code(step)), now, None), Some(step), "surrounding spaces are tolerated");
    }

    #[test]
    fn otpauth_url_is_escaped() {
        let u = otpauth_url("Ferro (prod)", "ann+test@example.com", "ABC234");
        assert_eq!(u, "otpauth://totp/Ferro%20%28prod%29:ann%2Btest%40example.com?secret=ABC234&issuer=Ferro%20%28prod%29&algorithm=SHA1&digits=6&period=30");
        assert!(qr_svg(&u).unwrap().starts_with("<?xml") || qr_svg(&u).unwrap().contains("<svg"));
    }

    #[test]
    fn sealed_secrets_roundtrip_and_are_bound_to_the_key() {
        let s = seal("server-secret", "JBSWY3DPEHPK3PXP").unwrap();
        assert!(!s.contains("JBSWY3DP"));
        assert_eq!(open("server-secret", &s).unwrap(), "JBSWY3DPEHPK3PXP");
        assert_ne!(s, seal("server-secret", "JBSWY3DPEHPK3PXP").unwrap(), "random nonce");
        assert!(open("another-secret", &s).is_err());
        let mut tampered = B64.decode(&s).unwrap();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open("server-secret", &B64.encode(tampered)).is_err());
        assert!(open("server-secret", "AAAA").is_err());
        assert!(open("server-secret", "%%%").is_err());
    }

    #[test]
    fn recovery_codes() {
        let codes = generate_recovery_codes(10);
        assert_eq!(codes.len(), 10);
        let mut unique = codes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 10);
        for c in &codes {
            assert!(looks_like_recovery_code(c), "{c}");
            assert_eq!(hash_recovery_code(c), hash_recovery_code(&format!(" {} ", c.to_uppercase())), "case and spaces do not matter");
        }
        assert!(!looks_like_recovery_code("123456"));
        assert!(!looks_like_recovery_code("abcde-fghi"));
    }
}
