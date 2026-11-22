//! The primitives of docs/protocol.md section 3.
//! Port of `src/engine/Crypto.cpp`; checked byte for byte against
//! `docs/protocol-vectors.json` (`tests/vectors.rs`).
//!
//! HKDF is written out (RFC 5869) on top of HMAC, as on every other platform.

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use sha2::{Digest, Sha256 as Sha256Hasher};

pub type Bytes = Vec<u8>;

/// Direction: PC -> phone.
pub const DIR_UP: u32 = 1;
/// Direction: phone -> PC.
pub const DIR_DOWN: u32 = 2;

#[derive(Debug, PartialEq, Eq)]
pub enum CryptoError {
    KeySize,
    PeerKey,
    Tag,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CryptoError::KeySize => "key size",
            CryptoError::PeerKey => "peer key",
            CryptoError::Tag => "tag",
        })
    }
}

impl std::error::Error for CryptoError {}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256Hasher::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    // INFALLIBLE: HMAC accepts a key of any length.
    let mut m = <Hmac<Sha256Hasher> as Mac>::new_from_slice(key).expect("hmac key");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// RFC 5869 with SHA-256. `length` of at most 255 * 32 bytes.
pub fn hkdf(ikm: &[u8], salt: &[u8], info: &[u8], length: usize) -> Bytes {
    let prk = hmac_sha256(salt, &[ikm]);
    let mut out = Bytes::with_capacity(length + 32);
    let mut t: Bytes = Vec::new();
    let mut i: u8 = 1;
    while out.len() < length {
        t = hmac_sha256(&prk, &[&t, info, &[i]]).to_vec();
        out.extend_from_slice(&t);
        i = i.wrapping_add(1);
    }
    out.truncate(length);
    out
}

/// u32 big-endian length, then the data.
pub fn lp(data: &[u8]) -> Bytes {
    let mut r = Bytes::with_capacity(4 + data.len());
    r.extend_from_slice(&(data.len() as u32).to_be_bytes());
    r.extend_from_slice(data);
    r
}

/// P-256 key pair: the public key is 65 bytes `04 || X || Y`.
pub struct EcdhKey {
    secret: SecretKey,
    public: [u8; 65],
}

impl EcdhKey {
    /// A fresh random key.
    pub fn generate() -> Self {
        Self::from_secret(SecretKey::random(&mut rand_core::OsRng))
    }

    /// Import an existing key (tests, saved keys).
    pub fn import(public_key: &[u8], d: &[u8]) -> Result<Self, CryptoError> {
        if public_key.len() != 65 || d.len() != 32 {
            return Err(CryptoError::KeySize);
        }
        let key = Self::from_secret(SecretKey::from_slice(d).map_err(|_| CryptoError::KeySize)?);
        // The C++ code trusts the caller's public half; here it must match d.
        if key.public[..] != *public_key {
            return Err(CryptoError::KeySize);
        }
        Ok(key)
    }

    fn from_secret(secret: SecretKey) -> Self {
        let point = secret.public_key().to_encoded_point(false);
        let mut public = [0u8; 65];
        // INFALLIBLE: an uncompressed P-256 point is always 65 bytes.
        public.copy_from_slice(point.as_bytes());
        Self { secret, public }
    }

    pub fn public_key(&self) -> &[u8; 65] {
        &self.public
    }

    /// Z: the X coordinate of the shared point, 32 bytes big-endian.
    pub fn agree(&self, peer_public: &[u8]) -> Result<[u8; 32], CryptoError> {
        if peer_public.len() != 65 || peer_public[0] != 0x04 {
            return Err(CryptoError::PeerKey);
        }
        let peer = PublicKey::from_sec1_bytes(peer_public).map_err(|_| CryptoError::PeerKey)?;
        let shared = diffie_hellman(self.secret.to_nonzero_scalar(), peer.as_affine());
        let mut z = [0u8; 32];
        z.copy_from_slice(shared.raw_secret_bytes());
        Ok(z)
    }
}

pub fn transcript(client_id: &str, device_id: &str, pc_pub: &[u8], phone_pub: &[u8]) -> [u8; 32] {
    let a = lp(b"camstudio-v1");
    let b = lp(client_id.as_bytes());
    let c = lp(device_id.as_bytes());
    let d = lp(pc_pub);
    let e = lp(phone_pub);
    sha256(&[&a, &b, &c, &d, &e])
}

/// The 6-digit comparison code both sides show.
pub fn sas(t: &[u8]) -> String {
    let h = sha256(&[b"camstudio-sas", t]);
    let v = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
    format!("{:06}", v % 1_000_000)
}

pub fn pair_key(z: &[u8], t: &[u8]) -> Bytes {
    hkdf(z, t, b"camstudio pair", 32)
}

pub fn up_key(z: &[u8], k_pair: &[u8], t: &[u8]) -> Bytes {
    hkdf(&[z, k_pair].concat(), t, b"camstudio pc->phone", 32)
}

pub fn down_key(z: &[u8], k_pair: &[u8], t: &[u8]) -> Bytes {
    hkdf(&[z, k_pair].concat(), t, b"camstudio phone->pc", 32)
}

pub fn pc_mac(k_pair: &[u8], t: &[u8]) -> [u8; 32] {
    hmac_sha256(k_pair, &[b"pc", t])
}

pub fn phone_mac(k_pair: &[u8], t: &[u8]) -> [u8; 32] {
    hmac_sha256(k_pair, &[b"phone", t])
}

fn nonce_and_aad(direction: u32, counter: u64) -> ([u8; 12], [u8; 9]) {
    let mut nonce = [0u8; 12];
    let mut aad = [0u8; 9];
    nonce[..4].copy_from_slice(&direction.to_be_bytes());
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    aad[0] = 0x02;
    aad[1..].copy_from_slice(&counter.to_be_bytes());
    (nonce, aad)
}

fn cipher(key: &[u8]) -> Result<Aes256Gcm, CryptoError> {
    Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::KeySize)
}

/// AES-256-GCM with the protocol's nonce (direction || counter) and AAD
/// (0x02 || counter). Returns ciphertext || tag.
pub fn seal(key: &[u8], direction: u32, counter: u64, plain: &[u8]) -> Result<Bytes, CryptoError> {
    let (nonce, aad) = nonce_and_aad(direction, counter);
    let mut buf = plain.to_vec();
    let tag = cipher(key)?
        .encrypt_in_place_detached(&Nonce::from(nonce), &aad, &mut buf)
        .map_err(|_| CryptoError::Tag)?;
    buf.extend_from_slice(&tag);
    Ok(buf)
}

/// Opens ciphertext || tag. `None` when the tag does not verify.
pub fn open(key: &[u8], direction: u32, counter: u64, sealed: &[u8]) -> Option<Bytes> {
    if sealed.len() < 16 {
        return None;
    }
    let (ct, tag) = sealed.split_at(sealed.len() - 16);
    let (nonce, aad) = nonce_and_aad(direction, counter);
    let tag: [u8; 16] = tag.try_into().ok()?;
    let mut buf = ct.to_vec();
    cipher(key)
        .ok()?
        .decrypt_in_place_detached(&Nonce::from(nonce), &aad, &mut buf, &tag.into())
        .ok()?;
    Some(buf)
}

pub fn from_hex(hex: &str) -> Bytes {
    // The C++ version converts each pair with stoul; an odd tail is ignored.
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect()
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}

pub fn base64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// Empty on a bad input, like the C++ version.
pub fn from_base64(s: &str) -> Bytes {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .unwrap_or_default()
}

pub fn constant_time_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_rfc5869_case_1() {
        let ikm = [0x0bu8; 22];
        let salt = from_hex("000102030405060708090a0b0c");
        let info = from_hex("f0f1f2f3f4f5f6f7f8f9");
        assert_eq!(
            to_hex(&hkdf(&ikm, &salt, &info, 42)),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn lp_prefixes_the_length() {
        assert_eq!(lp(b"ab"), vec![0, 0, 0, 2, b'a', b'b']);
    }

    #[test]
    fn open_rejects_short_and_tampered_input() {
        let key = [7u8; 32];
        assert!(open(&key, DIR_UP, 0, &[0u8; 15]).is_none());
        let mut s = seal(&key, DIR_UP, 5, b"hello").unwrap();
        assert_eq!(open(&key, DIR_UP, 5, &s).as_deref(), Some(&b"hello"[..]));
        assert!(open(&key, DIR_DOWN, 5, &s).is_none());
        assert!(open(&key, DIR_UP, 6, &s).is_none());
        s[0] ^= 1;
        assert!(open(&key, DIR_UP, 5, &s).is_none());
    }

    #[test]
    fn fresh_keys_agree() {
        let a = EcdhKey::generate();
        let b = EcdhKey::generate();
        assert_eq!(a.agree(b.public_key()), b.agree(a.public_key()));
    }

    #[test]
    fn rejects_bad_peer_keys() {
        let a = EcdhKey::generate();
        assert_eq!(a.agree(&[4u8; 64]), Err(CryptoError::PeerKey));
        let mut off_curve = *a.public_key();
        off_curve[64] ^= 1;
        assert_eq!(a.agree(&off_curve), Err(CryptoError::PeerKey));
    }
}
