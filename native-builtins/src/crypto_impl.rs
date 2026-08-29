// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real cryptographic primitive implementations.
//!
//! Phase 19.2 — AES (ECB/CBC/GCM), SHA-2 (256/384/512), HMAC, HKDF, SecureRandom.
//!
//! C18 — AES and AES-GCM are now constant-time, backed by the RustCrypto
//! `aes` and `aes-gcm` crates already in `Cargo.toml`. The previous
//! in-tree FIPS-197 core (256-byte S-box tables + bit-loop GHASH) was a
//! textbook cache-timing oracle (Bernstein 2005) and has been deleted.
//! SHA-2 / HMAC / HKDF / RSA / ECDSA / Ed25519 / X.509 / PKCS#12
//! parsers remain in-tree as before.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, ObjectRef, Value};

// ============================================================================
// CryptoError
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum CryptoError {
    InvalidKeyLength(usize),
    InvalidBlockSize,
    InvalidPadding,
    AuthenticationFailed,
    InvalidNonceLength,
    UnsupportedAlgorithm(String),
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::InvalidKeyLength(len) => write!(f, "invalid key length: {}", len),
            CryptoError::InvalidBlockSize => write!(f, "invalid block size"),
            CryptoError::InvalidPadding => write!(f, "invalid padding"),
            CryptoError::AuthenticationFailed => write!(f, "authentication failed"),
            CryptoError::InvalidNonceLength => write!(f, "invalid nonce length"),
            CryptoError::UnsupportedAlgorithm(a) => write!(f, "unsupported algorithm: {}", a),
        }
    }
}

// ============================================================================
// AES — Constant-Time RustCrypto Backend (C18)
// ============================================================================
//
// The legacy hand-rolled AES core (256-byte SBOX/InvSBOX, ShiftRows,
// MixColumns, gf_mul) was a textbook cache-timing oracle (Bernstein
// 2005). The hand-rolled GHASH `gf128_mul` had the same data-dependent
// branch hazard on the AES-GCM auth tag. All of that has been replaced
// with the RustCrypto `aes` / `aes-gcm` crates (already in Cargo.toml).
// The public call surface (`Aes::key_expansion`/`encrypt_block`/
// `decrypt_block`, `AesKey { nr, .. }`) is preserved so the in-tree
// `AesEcb`/`AesCbc`/`AesGcm` wrappers and `jca/cipher.rs` keep compiling
// without changes.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};

/// AES expanded-key state, parameterised over the three NIST key sizes.
/// Each variant owns a fully-initialised RustCrypto cipher; cloning is
/// cheap (a few hundred bytes of pre-expanded round keys).
#[derive(Clone, Debug)]
pub enum AesCipher {
    Aes128(aes::Aes128),
    Aes192(aes::Aes192),
    Aes256(aes::Aes256),
}

/// AES key handle. `nr` (number of rounds: 10/12/14) is preserved for
/// API back-compat with the legacy struct — `crypto_impl`'s own tests
/// inspect it.
#[derive(Clone, Debug)]
pub struct AesKey {
    pub cipher: AesCipher,
    pub nr: usize,
}

pub struct Aes;

impl Aes {
    /// Construct an AES key handle. Rejects any key length other than
    /// 128/192/256 bits — matches the JCA contract for SecretKeySpec.
    pub fn key_expansion(key: &[u8]) -> Result<AesKey, CryptoError> {
        let (cipher, nr) = match key.len() {
            16 => (
                AesCipher::Aes128(aes::Aes128::new(GenericArray::from_slice(key))),
                10,
            ),
            24 => (
                AesCipher::Aes192(aes::Aes192::new(GenericArray::from_slice(key))),
                12,
            ),
            32 => (
                AesCipher::Aes256(aes::Aes256::new(GenericArray::from_slice(key))),
                14,
            ),
            other => return Err(CryptoError::InvalidKeyLength(other)),
        };
        Ok(AesKey { cipher, nr })
    }

    /// Encrypt a single 16-byte block, constant-time.
    pub fn encrypt_block(key: &AesKey, input: &[u8; 16]) -> [u8; 16] {
        let mut block = GenericArray::clone_from_slice(input);
        match &key.cipher {
            AesCipher::Aes128(c) => c.encrypt_block(&mut block),
            AesCipher::Aes192(c) => c.encrypt_block(&mut block),
            AesCipher::Aes256(c) => c.encrypt_block(&mut block),
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(block.as_slice());
        out
    }

    /// Decrypt a single 16-byte block, constant-time.
    pub fn decrypt_block(key: &AesKey, input: &[u8; 16]) -> [u8; 16] {
        let mut block = GenericArray::clone_from_slice(input);
        match &key.cipher {
            AesCipher::Aes128(c) => c.decrypt_block(&mut block),
            AesCipher::Aes192(c) => c.decrypt_block(&mut block),
            AesCipher::Aes256(c) => c.decrypt_block(&mut block),
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(block.as_slice());
        out
    }
}

// ============================================================================
// PKCS7 Padding
// ============================================================================

fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let pad_len = block_size - (data.len() % block_size);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(pad_len as u8).take(pad_len));
    out
}

fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if data.is_empty() {
        return Err(CryptoError::InvalidPadding);
    }
    let pad_len = *data.last().unwrap() as usize;
    if pad_len == 0 || pad_len > 16 || pad_len > data.len() {
        return Err(CryptoError::InvalidPadding);
    }
    for &b in &data[data.len() - pad_len..] {
        if b != pad_len as u8 {
            return Err(CryptoError::InvalidPadding);
        }
    }
    Ok(data[..data.len() - pad_len].to_vec())
}

// ============================================================================
// AES-ECB
// ============================================================================

pub struct AesEcb;

impl AesEcb {
    pub fn encrypt(key: &AesKey, plaintext: &[u8]) -> Vec<u8> {
        let padded = pkcs7_pad(plaintext, 16);
        let mut out = Vec::with_capacity(padded.len());
        for chunk in padded.chunks(16) {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            out.extend_from_slice(&Aes::encrypt_block(key, &block));
        }
        out
    }

    pub fn decrypt(key: &AesKey, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
            return Err(CryptoError::InvalidBlockSize);
        }
        let mut out = Vec::with_capacity(ciphertext.len());
        for chunk in ciphertext.chunks(16) {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            out.extend_from_slice(&Aes::decrypt_block(key, &block));
        }
        pkcs7_unpad(&out)
    }
}

// ============================================================================
// AES-CBC
// ============================================================================

pub struct AesCbc;

impl AesCbc {
    pub fn encrypt(key: &AesKey, iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
        let padded = pkcs7_pad(plaintext, 16);
        let mut out = Vec::with_capacity(padded.len());
        let mut prev = *iv;
        for chunk in padded.chunks(16) {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            // XOR with previous ciphertext block (or IV)
            for i in 0..16 {
                block[i] ^= prev[i];
            }
            let encrypted = Aes::encrypt_block(key, &block);
            out.extend_from_slice(&encrypted);
            prev = encrypted;
        }
        out
    }

    pub fn decrypt(key: &AesKey, iv: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
            return Err(CryptoError::InvalidBlockSize);
        }
        let mut out = Vec::with_capacity(ciphertext.len());
        let mut prev = *iv;
        for chunk in ciphertext.chunks(16) {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            let decrypted = Aes::decrypt_block(key, &block);
            let mut plain_block = [0u8; 16];
            for i in 0..16 {
                plain_block[i] = decrypted[i] ^ prev[i];
            }
            out.extend_from_slice(&plain_block);
            prev = block;
        }
        pkcs7_unpad(&out)
    }
}

// ============================================================================
// AES-GCM
// ============================================================================

pub struct AesGcmOutput {
    pub ciphertext: Vec<u8>,
    pub tag: [u8; 16],
}

pub struct AesGcm;

impl AesGcm {
    /// Encrypt with AES-GCM. `nonce` MUST be the canonical 96-bit IV.
    /// Returns `(ciphertext, tag)` split into the legacy `AesGcmOutput`
    /// shape; callers that want the JDK wire format concatenate
    /// `ciphertext || tag` themselves (`jca/cipher.rs` does so).
    pub fn encrypt(key: &AesKey, nonce: &[u8; 12], plaintext: &[u8], aad: &[u8]) -> AesGcmOutput {
        use aes::cipher::consts::U12;
        use aes_gcm::{aead::AeadInPlace, Aes128Gcm, Aes256Gcm, AesGcm as AesGcmAead};
        // aes-gcm 0.10 ships type aliases for AES-128/256 only; spell
        // out the AES-192 variant explicitly.
        type Aes192Gcm = AesGcmAead<aes::Aes192, U12>;

        let nonce_arr = aes_gcm::Nonce::<U12>::from_slice(nonce);
        let mut buf = plaintext.to_vec();

        let tag_arr = match &key.cipher {
            AesCipher::Aes128(c) => {
                let gcm = Aes128Gcm::from(c.clone());
                gcm.encrypt_in_place_detached(nonce_arr, aad, &mut buf)
                    .expect("AES-128-GCM detached encrypt: buffer never grows")
            }
            AesCipher::Aes192(c) => {
                let gcm: Aes192Gcm = c.clone().into();
                gcm.encrypt_in_place_detached(nonce_arr, aad, &mut buf)
                    .expect("AES-192-GCM detached encrypt: buffer never grows")
            }
            AesCipher::Aes256(c) => {
                let gcm = Aes256Gcm::from(c.clone());
                gcm.encrypt_in_place_detached(nonce_arr, aad, &mut buf)
                    .expect("AES-256-GCM detached encrypt: buffer never grows")
            }
        };

        let mut tag = [0u8; 16];
        tag.copy_from_slice(tag_arr.as_slice());
        AesGcmOutput {
            ciphertext: buf,
            tag,
        }
    }

    /// Verify-then-decrypt with AES-GCM. Returns
    /// `Err(AuthenticationFailed)` if the tag or ciphertext is
    /// tampered. The constant-time tag check lives inside
    /// `aes_gcm`'s `decrypt_in_place_detached`.
    pub fn decrypt(
        key: &AesKey,
        nonce: &[u8; 12],
        ciphertext: &[u8],
        aad: &[u8],
        tag: &[u8; 16],
    ) -> Result<Vec<u8>, CryptoError> {
        use aes::cipher::consts::U12;
        use aes_gcm::{aead::AeadInPlace, Aes128Gcm, Aes256Gcm, AesGcm as AesGcmAead};
        type Aes192Gcm = AesGcmAead<aes::Aes192, U12>;

        let nonce_arr = aes_gcm::Nonce::<U12>::from_slice(nonce);
        let tag_arr = aes_gcm::Tag::<aes::cipher::consts::U16>::from_slice(tag);
        let mut buf = ciphertext.to_vec();

        let res = match &key.cipher {
            AesCipher::Aes128(c) => Aes128Gcm::from(c.clone())
                .decrypt_in_place_detached(nonce_arr, aad, &mut buf, tag_arr),
            AesCipher::Aes192(c) => {
                let gcm: Aes192Gcm = c.clone().into();
                gcm.decrypt_in_place_detached(nonce_arr, aad, &mut buf, tag_arr)
            }
            AesCipher::Aes256(c) => Aes256Gcm::from(c.clone())
                .decrypt_in_place_detached(nonce_arr, aad, &mut buf, tag_arr),
        };
        res.map_err(|_| CryptoError::AuthenticationFailed)?;
        Ok(buf)
    }
}

// ============================================================================
// SHA-256
// ============================================================================

const SHA256_H: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    buffer: Vec<u8>,
    total_len: u64,
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            h: SHA256_H,
            buffer: Vec::with_capacity(64),
            total_len: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.total_len += data.len() as u64;
        self.buffer.extend_from_slice(data);

        while self.buffer.len() >= 64 {
            let block: Vec<u8> = self.buffer.drain(..64).collect();
            self.compress(&block);
        }
    }

    pub fn finalize(&mut self) -> [u8; 32] {
        let bit_len = self.total_len * 8;
        // Padding
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0x00);
        }
        self.buffer.extend_from_slice(&bit_len.to_be_bytes());

        // Process remaining blocks
        let buf = std::mem::take(&mut self.buffer);
        for chunk in buf.chunks(64) {
            self.compress(chunk);
        }

        let mut out = [0u8; 32];
        for (i, &val) in self.h.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&val.to_be_bytes());
        }
        out
    }

    /// One-shot convenience.
    pub fn digest(data: &[u8]) -> [u8; 32] {
        let mut hasher = Self::new();
        hasher.update(data);
        hasher.finalize()
    }

    fn compress(&mut self, block: &[u8]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
        self.h[5] = self.h[5].wrapping_add(f);
        self.h[6] = self.h[6].wrapping_add(g);
        self.h[7] = self.h[7].wrapping_add(h);
    }
}

// ============================================================================
// SHA-512 / SHA-384
// ============================================================================

const SHA512_H: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const SHA384_H: [u64; 8] = [
    0xcbbb9d5dc1059ed8,
    0x629a292a367cd507,
    0x9159015a3070dd17,
    0x152fecd8f70e5939,
    0x67332667ffc00b31,
    0x8eb44a8768581511,
    0xdb0c2e0d64f98fa7,
    0x47b5481dbefa4fa4,
];

const SHA512_K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

/// Internal SHA-512 engine used by both SHA-512 and SHA-384.
#[derive(Clone)]
struct Sha512Engine {
    h: [u64; 8],
    buffer: Vec<u8>,
    total_len: u128,
}

impl Sha512Engine {
    fn new(init: [u64; 8]) -> Self {
        Sha512Engine {
            h: init,
            buffer: Vec::with_capacity(128),
            total_len: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.total_len += data.len() as u128;
        self.buffer.extend_from_slice(data);

        while self.buffer.len() >= 128 {
            let block: Vec<u8> = self.buffer.drain(..128).collect();
            self.compress(&block);
        }
    }

    fn finalize(&mut self) -> [u64; 8] {
        let bit_len = self.total_len * 8;
        self.buffer.push(0x80);
        while self.buffer.len() % 128 != 112 {
            self.buffer.push(0x00);
        }
        self.buffer
            .extend_from_slice(&(bit_len as u128).to_be_bytes());

        let buf = std::mem::take(&mut self.buffer);
        for chunk in buf.chunks(128) {
            self.compress(chunk);
        }
        self.h
    }

    fn compress(&mut self, block: &[u8]) {
        let mut w = [0u64; 80];
        for i in 0..16 {
            w[i] = u64::from_be_bytes([
                block[8 * i],
                block[8 * i + 1],
                block[8 * i + 2],
                block[8 * i + 3],
                block[8 * i + 4],
                block[8 * i + 5],
                block[8 * i + 6],
                block[8 * i + 7],
            ]);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;

        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA512_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
        self.h[5] = self.h[5].wrapping_add(f);
        self.h[6] = self.h[6].wrapping_add(g);
        self.h[7] = self.h[7].wrapping_add(h);
    }
}

pub struct Sha512;

impl Sha512 {
    pub fn digest(data: &[u8]) -> [u8; 64] {
        let mut engine = Sha512Engine::new(SHA512_H);
        engine.update(data);
        let h = engine.finalize();
        let mut out = [0u8; 64];
        for (i, &val) in h.iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&val.to_be_bytes());
        }
        out
    }
}

pub struct Sha384;

impl Sha384 {
    pub fn digest(data: &[u8]) -> [u8; 48] {
        let mut engine = Sha512Engine::new(SHA384_H);
        engine.update(data);
        let h = engine.finalize();
        let mut out = [0u8; 48];
        for i in 0..6 {
            out[i * 8..(i + 1) * 8].copy_from_slice(&h[i].to_be_bytes());
        }
        out
    }
}

// ============================================================================
// HashFunction enum
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HashFunction {
    Sha256,
    Sha384,
    Sha512,
}

impl HashFunction {
    fn block_size(&self) -> usize {
        match self {
            HashFunction::Sha256 => 64,
            HashFunction::Sha384 | HashFunction::Sha512 => 128,
        }
    }

    fn output_size(&self) -> usize {
        match self {
            HashFunction::Sha256 => 32,
            HashFunction::Sha384 => 48,
            HashFunction::Sha512 => 64,
        }
    }

    fn hash(&self, data: &[u8]) -> Vec<u8> {
        match self {
            HashFunction::Sha256 => Sha256::digest(data).to_vec(),
            HashFunction::Sha384 => Sha384::digest(data).to_vec(),
            HashFunction::Sha512 => Sha512::digest(data).to_vec(),
        }
    }
}

// ============================================================================
// HMAC
// ============================================================================

#[derive(Clone)]
pub struct Hmac {
    hash_fn: HashFunction,
    i_key_pad: Vec<u8>,
    o_key_pad: Vec<u8>,
    inner_data: Vec<u8>,
}

impl Hmac {
    pub fn new(key: &[u8], hash_fn: HashFunction) -> Self {
        let block_size = hash_fn.block_size();
        let mut k = if key.len() > block_size {
            hash_fn.hash(key)
        } else {
            key.to_vec()
        };
        // Pad key to block_size
        k.resize(block_size, 0);

        let mut i_key_pad = vec![0u8; block_size];
        let mut o_key_pad = vec![0u8; block_size];
        for i in 0..block_size {
            i_key_pad[i] = k[i] ^ 0x36;
            o_key_pad[i] = k[i] ^ 0x5c;
        }

        Hmac {
            hash_fn,
            i_key_pad,
            o_key_pad,
            inner_data: Vec::new(),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.inner_data.extend_from_slice(data);
    }

    pub fn finalize(&self) -> Vec<u8> {
        // inner = hash(i_key_pad || data)
        let mut inner_input = self.i_key_pad.clone();
        inner_input.extend_from_slice(&self.inner_data);
        let inner_hash = self.hash_fn.hash(&inner_input);

        // outer = hash(o_key_pad || inner)
        let mut outer_input = self.o_key_pad.clone();
        outer_input.extend_from_slice(&inner_hash);
        self.hash_fn.hash(&outer_input)
    }

    /// One-shot convenience.
    pub fn mac(key: &[u8], data: &[u8], hash_fn: HashFunction) -> Vec<u8> {
        let mut h = Hmac::new(key, hash_fn);
        h.update(data);
        h.finalize()
    }
}

// ============================================================================
// HKDF
// ============================================================================

pub struct Hkdf;

impl Hkdf {
    /// Extract: PRK = HMAC-Hash(salt, IKM)
    pub fn extract(hash_fn: HashFunction, salt: &[u8], ikm: &[u8]) -> Vec<u8> {
        let salt = if salt.is_empty() {
            vec![0u8; hash_fn.output_size()]
        } else {
            salt.to_vec()
        };
        Hmac::mac(&salt, ikm, hash_fn)
    }

    /// Expand: OKM = T(1) || T(2) || ... truncated to length
    pub fn expand(hash_fn: HashFunction, prk: &[u8], info: &[u8], length: usize) -> Vec<u8> {
        let hash_len = hash_fn.output_size();
        let n = (length + hash_len - 1) / hash_len;
        let mut okm = Vec::with_capacity(n * hash_len);
        let mut t = Vec::new();

        for i in 1..=n {
            let mut input = t.clone();
            input.extend_from_slice(info);
            input.push(i as u8);
            t = Hmac::mac(prk, &input, hash_fn);
            okm.extend_from_slice(&t);
        }
        okm.truncate(length);
        okm
    }

    /// Extract-then-expand in one call.
    pub fn derive(
        hash_fn: HashFunction,
        salt: &[u8],
        ikm: &[u8],
        info: &[u8],
        length: usize,
    ) -> Vec<u8> {
        let prk = Self::extract(hash_fn, salt, ikm);
        Self::expand(hash_fn, &prk, info, length)
    }
}

/// TEST ONLY — do not use for production key material; fixed IKM/salt.
///
/// C17 (2026-05-24 review): this function returns the *same* derived bytes
/// for every call with the same `(alg_idx, key_bytes)` pair, across every
/// VM run and every process, because the salt/IKM/info inputs are fixed
/// string literals (`"cratonvm-kdf-salt"`, `"cratonvm-kdf-ikm"`,
/// `"cratonvm-kdf"`). It exists only so the synthetic KDF surface can
/// return non-zero bytes during round-trip experiments — anyone using it
/// as key material would get a single hard-coded "secret".
///
/// The production call sites (`crypto.rs::KDF.getInstance(...)`) now
/// reject HKDF/PBKDF2 algorithm names with `NoSuchAlgorithmException`, so
/// this path is unreachable from Java code. The function is kept as a
/// `#[doc(hidden)]` test helper for the in-crate HKDF unit tests; it must
/// not be promoted back into a production path without first replacing
/// the fixed inputs with real `AlgorithmParameterSpec`-derived bytes.
#[doc(hidden)]
pub fn derive_key_bytes(alg_idx: i32, key_bytes: usize) -> Vec<u8> {
    let hash_fn = match alg_idx {
        1 | 4 => HashFunction::Sha384,
        2 | 5 => HashFunction::Sha512,
        _ => HashFunction::Sha256, // 0, 3, or fallback
    };
    // Fixed salt/IKM/info — see TEST ONLY warning above. The bytes are
    // deterministic per (alg_idx, key_bytes); never use as real key material.
    let salt = b"cratonvm-kdf-salt";
    let ikm = b"cratonvm-kdf-ikm";
    let info = b"cratonvm-kdf";
    Hkdf::derive(hash_fn, salt, ikm, info, key_bytes)
}

// ============================================================================
// SecureRandom
// ============================================================================

pub struct SecureRandom {
    /// Fallback seed for platforms where OS entropy is unavailable.
    seed: u64,
    counter: u64,
    /// When true, prefer OS entropy over seed-based generation.
    use_os_entropy: bool,
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

/// Fill `buf` with bytes from the OS cryptographic entropy source.
/// Returns `true` on success, `false` if OS entropy is unavailable.
pub fn os_random_bytes(buf: &mut [u8]) -> bool {
    #[cfg(target_os = "windows")]
    {
        // BCryptGenRandom via std::sys — use getrandom-style approach
        // On Windows, RtlGenRandom (SystemFunction036) is the simplest path.
        #[link(name = "advapi32")]
        extern "system" {
            #[link_name = "SystemFunction036"]
            fn RtlGenRandom(buf: *mut u8, len: u32) -> u8;
        }
        if buf.len() <= u32::MAX as usize {
            let ok = unsafe { RtlGenRandom(buf.as_mut_ptr(), buf.len() as u32) };
            return ok != 0;
        }
        // For buffers larger than u32::MAX, fill in chunks
        for chunk in buf.chunks_mut(u32::MAX as usize) {
            let ok = unsafe { RtlGenRandom(chunk.as_mut_ptr(), chunk.len() as u32) };
            if ok == 0 {
                return false;
            }
        }
        true
    }
    #[cfg(not(target_os = "windows"))]
    {
        // On Unix-like systems, read from /dev/urandom
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            return f.read_exact(buf).is_ok();
        }
        false
    }
}

impl SecureRandom {
    pub fn new() -> Self {
        // Seed from OS entropy, fall back to system time
        let mut seed_bytes = [0u8; 8];
        let seed = if os_random_bytes(&mut seed_bytes) {
            u64::from_le_bytes(seed_bytes)
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        };
        SecureRandom {
            seed,
            counter: 0,
            use_os_entropy: true,
        }
    }

    pub fn new_with_seed(seed: u64) -> Self {
        SecureRandom {
            seed,
            counter: 0,
            use_os_entropy: false,
        }
    }

    pub fn next_bytes(&mut self, buf: &mut [u8]) {
        if self.use_os_entropy {
            // Cryptographic path: use OS entropy (BCryptGenRandom / /dev/urandom).
            // Retry once on failure before falling back.
            if os_random_bytes(buf) {
                return;
            }
            // Second attempt — some transient failures recover on retry.
            if os_random_bytes(buf) {
                return;
            }
            // OS entropy completely unavailable — log and use time-mixed seed.
            // This is strictly better than splitmix64 alone because the seed
            // incorporates nanosecond timing, PID, and thread ID.
            tracing::warn!("OS entropy unavailable for SecureRandom — using mixed fallback");
            let time_seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let pid_mix = std::process::id() as u64;
            let thread_mix = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                std::thread::current().id().hash(&mut h);
                h.finish()
            };
            let mut state = time_seed ^ pid_mix ^ thread_mix ^ self.seed;
            let mut pos = 0;
            while pos < buf.len() {
                state = state.wrapping_add(0x9e3779b97f4a7c15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
                z = z ^ (z >> 31);
                let bytes = z.to_le_bytes();
                let remaining = buf.len() - pos;
                let to_copy = remaining.min(8);
                buf[pos..pos + to_copy].copy_from_slice(&bytes[..to_copy]);
                pos += to_copy;
            }
            return;
        }
        // Explicitly-seeded path (deterministic, for testing only).
        let mut pos = 0;
        while pos < buf.len() {
            let mut state = self.seed ^ self.counter;
            let val = splitmix64(&mut state);
            self.counter += 1;
            let bytes = val.to_le_bytes();
            let remaining = buf.len() - pos;
            let to_copy = remaining.min(8);
            buf[pos..pos + to_copy].copy_from_slice(&bytes[..to_copy]);
            pos += to_copy;
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        self.next_bytes(&mut buf);
        u64::from_le_bytes(buf)
    }

    pub fn next_u32(&mut self) -> u32 {
        let mut buf = [0u8; 4];
        self.next_bytes(&mut buf);
        u32::from_le_bytes(buf)
    }
}

// ============================================================================
// Native method stubs
// ============================================================================

// nb-crypto-impl SecureRandom — design summary.
//
// `nextBytes` / `generateSeed` draw EVERY byte directly from the OS CSPRNG
// (`secure_random_fill` → `os_random_bytes`), with a ChaCha20 software fallback
// only when the OS source is unavailable. There is intentionally NO per-instance
// DRBG state: output does not depend on the receiver, on any seed, or on the
// object's identity hash. See, in this file:
//   * VULN(secrand)            — why splitmix64-derived output was broken (≤64
//                                bits entropy, invertible) and how the OS-CSPRNG
//                                rewrite fixes it (on `secure_random_fill`).
//   * VULN(secrand-collision)  — why the old `identity_hash_code`-keyed DRBG
//                                side-table aliased colliding instances, and how
//                                removing it (output is now stateless) fixes it.
//
// Reproducibility: `java.security.SecureRandom` does NOT promise that an
// unseeded instance is reproducible, and `setSeed` only *supplements* (never
// weakens) the source — so honouring a caller seed against an already-fully-
// seeded OS CSPRNG is a no-op. (`java.util.Random`'s bit-reproducible `setSeed`
// is a different class, handled in `securerandom.rs`.)

// nb-crypto-impl VULN(secrand-collision) [FIXED]: there used to be a per-instance
// DRBG state side-table here — `HashMap<i32, (seed, counter)>` keyed on
// `NativeContext::identity_hash_code(this)`. Because `identity_hash_code` is a
// 32-bit value that CAN collide across distinct live objects, two different
// `SecureRandom` instances with a colliding identity hash SHARED (aliased) the
// same DRBG `(seed, counter)` — so their streams were correlated, and one
// instance's draws advanced the other's state. Combined with the (now removed)
// invertible splitmix64 stream this widened the predictability break.
//
// FIX: the output path (`secure_random_fill`) now pulls every byte straight from
// the OS CSPRNG and no longer consults any per-instance state, so the side-table
// is gone entirely. No security-sensitive randomness keys on `identity_hash_code`
// any more, which removes the collision/aliasing hazard at its root.

/// Draw a fresh OS-entropy `u64`.  Falls back to the time/pid/thread mix only if
/// the OS source is unavailable (mirrors `SecureRandom::new`), so we never use a
/// constant.  Now consumed solely by `secure_random_fill`'s ChaCha20 fallback to
/// derive a one-shot key/nonce when the OS CSPRNG is down.
fn secure_random_entropy_seed() -> u64 {
    let mut seed_bytes = [0u8; 8];
    if os_random_bytes(&mut seed_bytes) {
        u64::from_le_bytes(seed_bytes)
    } else {
        let time_seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let pid_mix = std::process::id() as u64;
        let thread_mix = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::thread::current().id().hash(&mut h);
            h.finish()
        };
        time_seed ^ pid_mix ^ thread_mix
    }
}

/// Fill `buf` with cryptographically-strong random bytes.  This is the single
/// output path used by both `nextBytes` and `generateSeed`.
///
/// nb-crypto-impl VULN(secrand): the PRIOR implementation derived every output
/// byte as `splitmix64(seed ^ counter)` over a per-instance state seeded with a
/// SINGLE 64-bit OS draw.  splitmix64 is an invertible bijection, so the whole
/// stream carried at most 64 bits of entropy: an attacker who observed ~8
/// consecutive output bytes could invert the mixer to recover `seed ^ counter`,
/// then reproduce every past and future byte from that `SecureRandom`.  That is
/// a catastrophic break of `java.security.SecureRandom`'s contract.
///
/// FIX: draw EVERY output byte directly from the OS CSPRNG
/// (`os_random_bytes` → `RtlGenRandom` / `/dev/urandom`).  `java.security.
/// SecureRandom` does NOT promise reproducibility for an unseeded instance, so
/// this is spec-compliant.  The `key` argument is retained for signature
/// stability but is no longer consulted — output no longer depends on any
/// per-instance, identity-hash-keyed DRBG state (see VULN(secrand-collision)),
/// which also closes the identity-hash aliasing hazard.
///
/// On the rare event that the OS source is unavailable, fall back to a fresh
/// ChaCha20 keystream re-keyed from `secure_random_entropy_seed` (which itself
/// mixes nanosecond timing, PID and thread id when the OS source is down).  The
/// fallback is far stronger than the broken splitmix64 stream — ChaCha20 is a
/// CSPRNG, not an invertible 64-bit mixer — and the primary path is always the
/// OS CSPRNG.
pub(crate) fn secure_random_fill(_key: i32, buf: &mut [u8]) {
    // Primary path: straight from the OS CSPRNG. Retry once on a transient
    // failure before degrading to the software fallback.
    if os_random_bytes(buf) {
        return;
    }
    if os_random_bytes(buf) {
        return;
    }
    // OS entropy unavailable — derive a one-shot ChaCha20 key/nonce from a fresh
    // entropy mix and run the keystream. This block is re-keyed on every call
    // (no persistent, predictable state survives between draws).
    tracing::warn!("OS entropy unavailable for SecureRandom — using ChaCha20 fallback");
    let mut key = [0u8; 32];
    for chunk in key.chunks_mut(8) {
        let n = chunk.len();
        chunk.copy_from_slice(&secure_random_entropy_seed().to_le_bytes()[..n]);
    }
    let mut nonce = [0u8; 12];
    for chunk in nonce.chunks_mut(4) {
        let n = chunk.len();
        let word = secure_random_entropy_seed() as u32;
        chunk.copy_from_slice(&word.to_le_bytes()[..n]);
    }
    chacha20_keystream_fill(&key, &nonce, buf);
}

/// RFC 8439 ChaCha20, XOR-ing the keystream into `buf` starting from block
/// `initial_counter`.
///
/// This is the crate's ONE ChaCha20 core. It was previously
/// `chacha20_keystream_fill`, which wrote its keystream instead of XOR-ing it
/// and hard-coded the block counter to 0 — the two properties that stood
/// between an already-correct RFC 8439 implementation and a usable stream
/// cipher. Generalising in place rather than adding a second entry point is
/// deliberate: `chacha20_keystream_fill` is now a wrapper over this function,
/// so the RFC 7539 known-answer test below covers both callers, and there is
/// no second ChaCha20 to drift.
///
/// The counter is the caller's because JCA's `ChaCha20ParameterSpec(nonce,
/// counter)` lets the caller choose it. Verified against HotSpot 25's own
/// SunJCE `Cipher.getInstance("ChaCha20")` — see
/// `chacha20_xor_counter_one_matches_hotspot` — which also confirms the
/// counter is a plain block index: the same key/nonce at counter 0 produces,
/// in its second 64-byte block, exactly what counter 1 produces in its first.
///
/// Counter wrap is `wrapping_add`, matching the pre-existing behaviour. RFC
/// 8439 §2.3 caps a single (key, nonce) message at 256 GiB, which this
/// function does not enforce; no caller in this tree comes near it, and the
/// only present caller is `secure_random_fill`'s one-shot fallback.
pub(crate) fn chacha20_xor(key: &[u8; 32], nonce: &[u8; 12], initial_counter: u32, buf: &mut [u8]) {
    #[inline]
    fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
        s[a] = s[a].wrapping_add(s[b]);
        s[d] = (s[d] ^ s[a]).rotate_left(16);
        s[c] = s[c].wrapping_add(s[d]);
        s[b] = (s[b] ^ s[c]).rotate_left(12);
        s[a] = s[a].wrapping_add(s[b]);
        s[d] = (s[d] ^ s[a]).rotate_left(8);
        s[c] = s[c].wrapping_add(s[d]);
        s[b] = (s[b] ^ s[c]).rotate_left(7);
    }
    let mut state0 = [0u32; 16];
    // Constants "expand 32-byte k".
    state0[0] = 0x6170_7865;
    state0[1] = 0x3320_646e;
    state0[2] = 0x7962_2d32;
    state0[3] = 0x6b20_6574;
    for i in 0..8 {
        state0[4 + i] =
            u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    // state[12] is the block counter; state[13..16] are the 96-bit nonce.
    for i in 0..3 {
        state0[13 + i] = u32::from_le_bytes([
            nonce[4 * i],
            nonce[4 * i + 1],
            nonce[4 * i + 2],
            nonce[4 * i + 3],
        ]);
    }
    let mut counter: u32 = initial_counter;
    let mut pos = 0;
    while pos < buf.len() {
        let mut working = state0;
        working[12] = counter;
        let mut x = working;
        for _ in 0..10 {
            // Column rounds.
            quarter_round(&mut x, 0, 4, 8, 12);
            quarter_round(&mut x, 1, 5, 9, 13);
            quarter_round(&mut x, 2, 6, 10, 14);
            quarter_round(&mut x, 3, 7, 11, 15);
            // Diagonal rounds.
            quarter_round(&mut x, 0, 5, 10, 15);
            quarter_round(&mut x, 1, 6, 11, 12);
            quarter_round(&mut x, 2, 7, 8, 13);
            quarter_round(&mut x, 3, 4, 9, 14);
        }
        let mut block = [0u8; 64];
        for i in 0..16 {
            let word = x[i].wrapping_add(working[i]);
            block[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
        }
        let to_copy = (buf.len() - pos).min(64);
        for i in 0..to_copy {
            buf[pos + i] ^= block[i];
        }
        pos += to_copy;
        counter = counter.wrapping_add(1);
    }
}

/// Raw ChaCha20 keystream from block 0, written over whatever `buf` held.
///
/// Used ONLY as the software fallback inside `secure_random_fill` when the OS
/// CSPRNG is unavailable. Kept as a named wrapper rather than folded into its
/// one call site so that the RFC 7539 known-answer test keeps testing the
/// shape the fallback actually uses: zero the buffer first, so XOR-ing the
/// keystream in is the same thing as writing it. Zeroing is not incidental —
/// `secure_random_fill` hands us a caller's buffer whose prior contents are
/// arbitrary, and entropy that depends on them is entropy nobody audited.
fn chacha20_keystream_fill(key: &[u8; 32], nonce: &[u8; 12], buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
    chacha20_xor(key, nonce, 0, buf);
}

fn native_secure_random_next_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // java/security/SecureRandom.nextBytes([B)V
    // args: [this, byte[]]
    let _this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr);
    // Output comes straight from the OS CSPRNG. We deliberately do NOT key on the
    // receiver's `identity_hash_code` any more (that 32-bit value can collide,
    // aliasing two instances' DRBG state — VULN(secrand-collision)); the OS
    // CSPRNG is per-draw fresh and needs no per-instance state.
    let mut buf = vec![0u8; len];
    secure_random_fill(0, &mut buf);
    for (i, &b) in buf.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    Ok(None)
}

fn native_secure_random_generate_seed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // java/security/SecureRandom.generateSeed(I)[B
    // args: [this, numBytes]
    let _this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let num_bytes = match args.get(1) {
        Some(Value::Int(n)) => *n as usize,
        _ => return Ok(Some(Value::Object(None))),
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, num_bytes);
    // generateSeed draws directly from the OS CSPRNG (the canonical source of
    // fresh seed material). Not keyed on the receiver's identity hash — see
    // VULN(secrand-collision).
    let mut buf = vec![0u8; num_bytes];
    secure_random_fill(0, &mut buf);
    for (i, &b) in buf.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

// nb-crypto-impl VULN(secrand-collision) [FIXED]: the `nextBytes` /
// `generateSeed` natives below NO LONGER mutate any per-instance,
// identity-hash-keyed DRBG state (that state was the collision/aliasing hazard
// and has been removed). Output is drawn straight from the OS CSPRNG, which is
// already maximally and freshly seeded.
//
// The `setSeed(J)V`, `setSeed([B)V` and `<init>([B)V` natives that used to live
// here were DELETED (L8 residual pass, 2026-08-12), bodies and registrations
// alike. Each was a shape-checking no-op justified by "a caller seed cannot
// weaken an already-fully-seeded OS CSPRNG". That argument is sound for entropy
// and WRONG for replay: `securerandom.rs`'s `setSeed` bodies check
// `secure_random_is_sha1prng` and route SHA1PRNG through real reseeding,
// because `SecureRandom.getInstance("SHA1PRNG")` seeded twice alike yields
// identical bytes on HotSpot — the one replay guarantee the JDK gives a
// `SecureRandom`, and the property H2's `TestAll` depends on. The no-ops here
// won by registration order in synthetic mode (see `register_crypto_impl_natives`)
// and undid that wholesale. The seeded constructor went with them because its
// body stamped neither `algorithm` nor `provider`, so `new SecureRandom(seed)`
// answered `null` from `getAlgorithm()` and `getProvider()` in synthetic mode —
// L8's own headline defect, surviving in one mode because a later registrar
// overwrote the fix. `securerandom.rs` now serves all three triples in every
// mode. (`java.util.Random`'s bit-reproducible `setSeed` is a different class,
// also handled in `securerandom.rs`.)

// nb-crypto-impl VULN(3): The single-shot MessageDigest/Cipher/Mac stubs that
// formerly lived here have been DELETED outright. They were dead code (registered
// nowhere — only stored in an `_unused_single_shot_stubs` tuple to silence
// dead-code warnings) and were actively dangerous:
//   * `native_cipher_do_final` ALWAYS performed raw AES-ECB regardless of the
//     requested transformation, silently downgrading GCM/CBC to ECB (no auth tag,
//     no IV), and FELL BACK TO AN ALL-ZERO 16-byte KEY whenever Cipher.init()
//     had not stored key material — an attacker-predictable null encryption.
//   * `native_mac_do_final` returned a fixed HMAC-SHA256 over a zero key and an
//     empty message, i.e. a constant MAC independent of the data.
//   * the digest stub hashed only the single doFinal argument, ignoring any
//     accumulated update() state and the requested algorithm (always SHA-256).
// The real accumulate-and-finalize implementations live in phases_early.rs /
// phases_late.rs / lib.rs and are the ones actually registered, so removing
// these leaves no functional gap — only the landmine.

pub(crate) fn register_crypto_impl_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // SecureRandom — `nextBytes` / `generateSeed` draw every byte directly from
    // the OS CSPRNG (BCryptGenRandom / getrandom; ChaCha20 software fallback if
    // the OS source is down). There is no per-instance DRBG state any more:
    // output is independent of the receiver's identity hash, so the prior
    // splitmix64-predictability and identity-hash-collision aliasing hazards are
    // both closed (see VULN(secrand) / VULN(secrand-collision) in this file).
    //
    // Registration order: both this registrar and
    // `securerandom::register_random_and_securerandom_natives` are called from
    // `register_synthetic_overrides`, this one SECOND, so these bodies win —
    // `register()` is last-registration-wins. That scope is the whole story:
    // `register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]`,
    // the feature is in no crate's default set, and `vm_init` reaches it only
    // when `config.use_synthetic_jdk` is also true. So none of this exists in a
    // default CLI build, and `--real-jdk` / `--jdk-only` are served by
    // `securerandom.rs` — see docs/architecture/natives-over-real-jdk-classes.md §2.
    // Only `nextBytes` / `generateSeed` are registered here: both draw from the
    // OS CSPRNG in either file, so the shadowing is behaviour-neutral. The
    // `setSeed` and seeded-ctor rows were REMOVED (L8 residual pass, 2026-08-12)
    // because their no-op bodies silently undid two fixes in `securerandom.rs`:
    // SHA1PRNG reseeding, which HotSpot makes reproducible and which is the one
    // replay guarantee the JDK gives a `SecureRandom`; and the `algorithm` /
    // `provider` stamping that `getProvider()` returning null was fixed by.
    r.register(
        "java/security/SecureRandom",
        "nextBytes",
        "([B)V",
        native_secure_random_next_bytes,
    );
    r.register(
        "java/security/SecureRandom",
        "generateSeed",
        "(I)[B",
        native_secure_random_generate_seed,
    );
    r.set_category(__prev_cat);
}

// ============================================================================
// G58 — Real RSA / ECDSA (P-256) implementations
// ============================================================================

// ---------------------------------------------------------------------------
// BigUint — minimal arbitrary-precision unsigned integer (u32 limbs, LE)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct BigUint {
    /// Little-endian u32 limbs.
    pub limbs: Vec<u32>,
}

/// Knuth Algorithm D step D3 — estimate one quotient digit.
///
/// Given the top two limbs of the current partial remainder
/// (`u_high:u_mid`), the limb below them (`u_low`), and the divisor's top two
/// limbs (`v_top:v_top2`), return `q_hat`: the estimate of the next quotient
/// digit, guaranteed by D1's normalisation (`v_top >= 2^31`) to be either
/// exact or 1 too large.
///
/// # Why this is a separate function
///
/// It is the only arithmetic in the file that can exceed 64 bits, and it is
/// almost impossible to steer from the outside: reaching the `u_high == v_top`
/// branch through `div_rem` means arranging for a *partial remainder* several
/// digits into the division to have a particular top limb. That is why the
/// overflow below reached production as a once-in-a-few-runs failure of an RSA
/// test and survived a 400,000-pair random sweep of `div_rem` without
/// reproducing once. Split out, the branch is three arguments away.
///
/// # The overflow this fixes
///
/// The previous form was
///
/// ```text
/// while q_hat >= base || q_hat * v_top2 > base * r_hat + u_low {
///     q_hat -= 1; r_hat += v_top;
///     if r_hat >= base { break; }
/// }
/// ```
///
/// which is Knuth's loop with the `r_hat < b` guard moved from *in front of
/// the test* to *after the body*. That is equivalent on every iteration but
/// the first — and on the first, `r_hat` is only bounded by `base` on the
/// `u_high != v_top` branch. On the other branch `r_hat` starts at
/// `u_mid + v_top`, which reaches ~2^33, and `base * r_hat` is then 2^32 ×
/// 2^33. Debug builds panicked with "attempt to multiply with overflow";
/// **release builds wrapped**, so the comparison answered nonsense, the
/// correction was skipped, and the division returned a wrong quotient with no
/// symptom at all.
///
/// Restoring Knuth's guard fixes it; computing in `u128` as well means the
/// bound no longer has to be re-derived by whoever edits this next.
fn estimate_quotient_digit(u_high: u64, u_mid: u64, u_low: u64, v_top: u64, v_top2: u64) -> u64 {
    const BASE: u64 = 1u64 << 32;
    debug_assert!(v_top >= 1 << 31, "D1 must normalise the divisor first");
    let dividend = (u_high << 32) | u_mid;
    let mut q_hat = if u_high >= v_top {
        BASE - 1
    } else {
        dividend / v_top
    };
    let mut r_hat = dividend - q_hat * v_top;
    // Knuth's guard, in Knuth's position: `BASE * r_hat` is never formed for
    // an `r_hat` that does not fit beside it.
    while r_hat < BASE
        && u128::from(q_hat) * u128::from(v_top2)
            > u128::from(BASE) * u128::from(r_hat) + u128::from(u_low)
    {
        q_hat -= 1;
        r_hat += v_top;
    }
    q_hat
}

impl BigUint {
    pub const ZERO: BigUint = BigUint { limbs: Vec::new() };

    pub fn zero() -> Self {
        BigUint { limbs: Vec::new() }
    }

    pub fn one() -> Self {
        BigUint { limbs: vec![1] }
    }

    pub fn from_u64(v: u64) -> Self {
        if v == 0 {
            return Self::zero();
        }
        let lo = v as u32;
        let hi = (v >> 32) as u32;
        let mut limbs = vec![lo];
        if hi != 0 {
            limbs.push(hi);
        }
        BigUint { limbs }
    }

    pub fn from_bytes_be(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self::zero();
        }
        let mut limbs = Vec::with_capacity((bytes.len() + 3) / 4);
        let mut i = bytes.len();
        while i > 0 {
            let start = if i >= 4 { i - 4 } else { 0 };
            let mut val = 0u32;
            for &b in &bytes[start..i] {
                val = (val << 8) | b as u32;
            }
            limbs.push(val);
            i = start;
        }
        let mut b = BigUint { limbs };
        b.normalize();
        b
    }

    pub fn to_bytes_be(&self) -> Vec<u8> {
        if self.is_zero() {
            return vec![0];
        }
        let mut bytes = Vec::new();
        for &limb in self.limbs.iter().rev() {
            bytes.extend_from_slice(&limb.to_be_bytes());
        }
        // strip leading zeros
        while bytes.len() > 1 && bytes[0] == 0 {
            bytes.remove(0);
        }
        bytes
    }

    /// Return bytes zero-padded to exactly `len` bytes (big-endian).
    pub fn to_bytes_be_padded(&self, len: usize) -> Vec<u8> {
        let raw = self.to_bytes_be();
        if raw.len() >= len {
            return raw[raw.len() - len..].to_vec();
        }
        let mut out = vec![0u8; len - raw.len()];
        out.extend_from_slice(&raw);
        out
    }

    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty() || self.limbs.iter().all(|&l| l == 0)
    }

    pub fn is_one(&self) -> bool {
        self.limbs.len() == 1 && self.limbs[0] == 1
    }

    pub fn is_even(&self) -> bool {
        self.limbs.is_empty() || (self.limbs[0] & 1) == 0
    }

    pub fn bit_length(&self) -> usize {
        if self.is_zero() {
            return 0;
        }
        let top = self.limbs.len() - 1;
        (top * 32) + (32 - self.limbs[top].leading_zeros() as usize)
    }

    pub fn bit(&self, idx: usize) -> bool {
        let limb_idx = idx / 32;
        if limb_idx >= self.limbs.len() {
            return false;
        }
        (self.limbs[limb_idx] >> (idx % 32)) & 1 == 1
    }

    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    pub fn add(&self, other: &BigUint) -> BigUint {
        let max_len = self.limbs.len().max(other.limbs.len());
        let mut result = Vec::with_capacity(max_len + 1);
        let mut carry = 0u64;
        for i in 0..max_len {
            let a = *self.limbs.get(i).unwrap_or(&0) as u64;
            let b = *other.limbs.get(i).unwrap_or(&0) as u64;
            let sum = a + b + carry;
            result.push(sum as u32);
            carry = sum >> 32;
        }
        if carry > 0 {
            result.push(carry as u32);
        }
        let mut r = BigUint { limbs: result };
        r.normalize();
        r
    }

    /// self - other. Panics if other > self.
    pub fn sub(&self, other: &BigUint) -> BigUint {
        let mut result = Vec::with_capacity(self.limbs.len());
        let mut borrow = 0i64;
        for i in 0..self.limbs.len() {
            let a = self.limbs[i] as i64;
            let b = *other.limbs.get(i).unwrap_or(&0) as i64;
            let diff = a - b - borrow;
            if diff < 0 {
                result.push((diff + (1i64 << 32)) as u32);
                borrow = 1;
            } else {
                result.push(diff as u32);
                borrow = 0;
            }
        }
        let mut r = BigUint { limbs: result };
        r.normalize();
        r
    }

    pub fn mul(&self, other: &BigUint) -> BigUint {
        if self.is_zero() || other.is_zero() {
            return Self::zero();
        }
        let mut result = vec![0u32; self.limbs.len() + other.limbs.len()];
        for i in 0..self.limbs.len() {
            let mut carry = 0u64;
            for j in 0..other.limbs.len() {
                let prod =
                    self.limbs[i] as u64 * other.limbs[j] as u64 + result[i + j] as u64 + carry;
                result[i + j] = prod as u32;
                carry = prod >> 32;
            }
            result[i + other.limbs.len()] += carry as u32;
        }
        let mut r = BigUint { limbs: result };
        r.normalize();
        r
    }

    pub fn mul_u32(&self, v: u32) -> BigUint {
        if v == 0 || self.is_zero() {
            return Self::zero();
        }
        let mut result = Vec::with_capacity(self.limbs.len() + 1);
        let mut carry = 0u64;
        for &limb in &self.limbs {
            let prod = limb as u64 * v as u64 + carry;
            result.push(prod as u32);
            carry = prod >> 32;
        }
        if carry > 0 {
            result.push(carry as u32);
        }
        let mut r = BigUint { limbs: result };
        r.normalize();
        r
    }

    /// Returns (quotient, remainder).
    pub fn div_rem(&self, divisor: &BigUint) -> (BigUint, BigUint) {
        if divisor.is_zero() {
            return (Self::zero(), Self::zero());
        }
        if self.cmp(divisor) == std::cmp::Ordering::Less {
            return (Self::zero(), self.clone());
        }
        if divisor.limbs.len() == 1 {
            return self.div_rem_u32(divisor.limbs[0]);
        }
        // Knuth Algorithm D (simplified)
        self.div_rem_long(divisor)
    }

    fn div_rem_u32(&self, d: u32) -> (BigUint, BigUint) {
        let d = d as u64;
        let mut quotient = vec![0u32; self.limbs.len()];
        let mut rem = 0u64;
        for i in (0..self.limbs.len()).rev() {
            let cur = (rem << 32) | self.limbs[i] as u64;
            quotient[i] = (cur / d) as u32;
            rem = cur % d;
        }
        let mut q = BigUint { limbs: quotient };
        q.normalize();
        (q, BigUint::from_u64(rem))
    }

    fn div_rem_long(&self, divisor: &BigUint) -> (BigUint, BigUint) {
        // Knuth Algorithm D (TAOCP 4.3.1) — schoolbook long division at limb
        // granularity. ~32× faster than the bit-by-bit fallback that used to
        // live here; for 2048-bit RSA modpow this turns ~minutes into seconds.
        //
        // The previous implementation walked one bit at a time (O(N²) where
        // N is bit-count); this walks one *limb* at a time (O(M²) where M is
        // limb-count, M = N/32), so the inner loop count drops by 32×.
        //
        // Inputs:
        //   self    — `u`, dividend; |u| > |v|, |v| > 1 limb (caller invariants).
        //   divisor — `v`, divisor.
        // Output: `(q, r)` with `u = q*v + r` and `0 <= r < v`.
        //
        // Steps:
        //   D1. Normalize: shift `v` left so its top bit is set; shift `u`
        //       by the same amount (gains an extra leading limb). The
        //       guarantee `v_top >= 2^31` makes the per-digit estimate
        //       `q_hat = (u_top:u_top-1) / v_top` accurate to within 2.
        //   D2..D7 The classical loop: estimate q_hat, multiply-subtract, add-back
        //       on the rare overflow case.

        let shift = divisor.limbs.last().unwrap().leading_zeros();
        let v = divisor.shl_bits(shift);
        let mut u = self.shl_bits(shift);

        let n = v.limbs.len();
        // Make sure u has exactly one more limb than the leading position
        // of v so the (u[j+n] : u[j+n-1]) "double-limb" exists. After
        // shl_bits, u may have either m+n or m+n+1 limbs depending on
        // whether the shift produced an extra carry — pad explicitly.
        let m_init = u.limbs.len().saturating_sub(n);
        let want = m_init + n + 1;
        while u.limbs.len() < want {
            u.limbs.push(0);
        }
        let m = u.limbs.len() - n - 1;

        let v_top = v.limbs[n - 1] as u64;
        let v_top2 = v.limbs[n - 2] as u64;

        let mut q = vec![0u32; m + 1];
        let base: u64 = 1u64 << 32;

        for j in (0..=m).rev() {
            // D3. Calculate q_hat — estimate of the j-th quotient digit.
            // See `estimate_quotient_digit`: the estimate can exceed 64 bits
            // and is nearly unreachable from outside, so it lives on its own
            // where a unit test can aim at it directly.
            let mut q_hat = estimate_quotient_digit(
                u.limbs[j + n] as u64,
                u.limbs[j + n - 1] as u64,
                u.limbs[j + n - 2] as u64,
                v_top,
                v_top2,
            );

            // D4. Multiply and subtract: u[j..j+n+1] -= q_hat * v.
            let mut borrow: i64 = 0;
            let mut carry: u64 = 0;
            for i in 0..n {
                let prod = q_hat * v.limbs[i] as u64 + carry;
                carry = prod >> 32;
                let prod_lo = prod & 0xFFFF_FFFF;
                let cur = u.limbs[j + i] as i64 - prod_lo as i64 - borrow;
                if cur < 0 {
                    u.limbs[j + i] = (cur + (1i64 << 32)) as u32;
                    borrow = 1;
                } else {
                    u.limbs[j + i] = cur as u32;
                    borrow = 0;
                }
            }
            // Final limb: subtract leftover carry from u[j+n].
            let cur = u.limbs[j + n] as i64 - carry as i64 - borrow;
            let underflow = cur < 0;
            u.limbs[j + n] = if underflow {
                (cur + (1i64 << 32)) as u32
            } else {
                cur as u32
            };

            // D5. Test remainder. If we underflowed, q_hat was 1 too large.
            if underflow {
                // D6. Add back v to u[j..j+n+1] and decrement q_hat.
                q_hat -= 1;
                let mut add_carry: u64 = 0;
                for i in 0..n {
                    let sum = u.limbs[j + i] as u64 + v.limbs[i] as u64 + add_carry;
                    u.limbs[j + i] = sum as u32;
                    add_carry = sum >> 32;
                }
                // The add-back carry should cancel the borrow we noted above.
                u.limbs[j + n] = u.limbs[j + n].wrapping_add(add_carry as u32);
            }

            q[j] = q_hat as u32;
        }

        // D8. Unnormalize the remainder by shifting right.
        let mut rem = BigUint {
            limbs: u.limbs[..n].to_vec(),
        };
        rem.normalize();
        let rem = rem.shr_bits(shift);

        let mut quotient = BigUint { limbs: q };
        quotient.normalize();
        (quotient, rem)
    }

    fn shl_bits(&self, shift: u32) -> BigUint {
        if shift == 0 || self.is_zero() {
            return self.clone();
        }
        let word_shift = (shift / 32) as usize;
        let bit_shift = shift % 32;
        let mut result = vec![0u32; self.limbs.len() + word_shift + 1];
        let mut carry = 0u32;
        for i in 0..self.limbs.len() {
            let v = self.limbs[i] as u64;
            let shifted = (v << bit_shift) | carry as u64;
            result[i + word_shift] = shifted as u32;
            carry = (shifted >> 32) as u32;
        }
        if carry > 0 {
            result[self.limbs.len() + word_shift] = carry;
        }
        let mut r = BigUint { limbs: result };
        r.normalize();
        r
    }

    pub fn shr_bits(&self, shift: u32) -> BigUint {
        if shift == 0 || self.is_zero() {
            return self.clone();
        }
        let word_shift = (shift / 32) as usize;
        let bit_shift = shift % 32;
        if word_shift >= self.limbs.len() {
            return Self::zero();
        }
        let mut result = Vec::with_capacity(self.limbs.len() - word_shift);
        for i in word_shift..self.limbs.len() {
            let lo = self.limbs[i] >> bit_shift;
            let hi = if bit_shift > 0 && i + 1 < self.limbs.len() {
                self.limbs[i + 1] << (32 - bit_shift)
            } else {
                0
            };
            result.push(lo | hi);
        }
        let mut r = BigUint { limbs: result };
        r.normalize();
        r
    }

    fn set_bit(mut self, idx: usize) -> BigUint {
        let limb_idx = idx / 32;
        let bit_idx = idx % 32;
        while self.limbs.len() <= limb_idx {
            self.limbs.push(0);
        }
        self.limbs[limb_idx] |= 1 << bit_idx;
        self
    }

    pub fn cmp(&self, other: &BigUint) -> std::cmp::Ordering {
        let a_len = self.limbs.len();
        let b_len = other.limbs.len();
        // compare effective lengths (skip trailing zeros)
        let a_eff = if self.is_zero() { 0 } else { a_len };
        let b_eff = if other.is_zero() { 0 } else { b_len };
        if a_eff != b_eff {
            return a_eff.cmp(&b_eff);
        }
        for i in (0..a_eff).rev() {
            let a = self.limbs[i];
            let b = other.limbs[i];
            if a != b {
                return a.cmp(&b);
            }
        }
        std::cmp::Ordering::Equal
    }

    /// self mod other
    pub fn modulo(&self, m: &BigUint) -> BigUint {
        self.div_rem(m).1
    }

    /// Modular exponentiation: `self^exp mod m`.
    ///
    /// nb-crypto-impl VULN(2): this is plain left-to-right square-and-multiply
    /// over a *variable-time* `BigUint` (the `mul`/`modulo`/`div_rem` limb
    /// routines are not constant-time, and the per-bit code path is selected by
    /// `exp.bit(i)`). The previous doc comment claimed a "constant-time
    /// Montgomery ladder", which it never was. This routine is therefore NOT
    /// side-channel resistant: when `exp` is a secret (RSA `d`, an ECDSA nonce
    /// inverse, etc.) the running time and branch pattern leak information about
    /// the exponent. It is retained only because routing every RSA/EC private-key
    /// operation through the vendored RustCrypto primitives is a larger, cross-
    /// crate change (new dependency + call-site rewrites) outside this module's
    /// scope. Do NOT rely on this for secrets exposed to a timing adversary;
    /// prefer the audited `rsa` / `p256` crates for such paths.
    pub fn modpow(&self, exp: &BigUint, m: &BigUint) -> BigUint {
        if m.is_one() {
            return Self::zero();
        }
        match crate::montgomery::Montgomery::new(&m.limbs) {
            // Odd modulus — every RSA/DSA/DH modulus in practice.
            Some(mont) => self.modpow_montgomery(exp, m, &mont),
            // Even (or degenerate) modulus: no Montgomery form, keep dividing.
            None => self.modpow_dividing(exp, m),
        }
    }

    /// The ladder above with Montgomery products instead of a Knuth-D division
    /// per step — retires
    /// `perf/biginteger-modpow-has-no-montgomery-reduction-20260817` on the
    /// native RSA path.
    ///
    /// The **ladder shape is preserved deliberately**: one multiply and one
    /// square per exponent bit, with only the *operands* selected by the bit,
    /// and no windowed precomputation table. A window would be faster still,
    /// and `bigint::BigInt::modpow` does use one — that routine serves
    /// `java.math.BigInteger.modPow`, where HotSpot's own implementation is
    /// windowed Montgomery, so matching it is the compatible choice. Here the
    /// caller is this crate's RSA/DSA private-key path with a known-secret
    /// exponent, and a window indexes its table with secret exponent bits, so
    /// this routine does not take that trade. It does not thereby become
    /// constant-time — see the VULN(2) note on `modpow` for what this routine
    /// does and does not promise.
    fn modpow_montgomery(
        &self,
        exp: &BigUint,
        m: &BigUint,
        mont: &crate::montgomery::Montgomery,
    ) -> BigUint {
        let n = mont.limbs();
        // Widen to exactly `n` limbs. `BigUint` does not guarantee a trimmed
        // representation (`is_zero` explicitly tolerates all-zero limbs), and a
        // value carrying trailing zeros would be *truncated* by a bare resize,
        // so trim first.
        let pad = |mut v: Vec<u32>| -> Vec<u32> {
            while v.last() == Some(&0) {
                v.pop();
            }
            debug_assert!(v.len() <= n, "operand wider than the modulus");
            v.resize(n, 0);
            v
        };
        // R mod m and R^2 mod m, where R = 2^(32n). Two divisions, once,
        // instead of one per exponent bit.
        let r1 = pad(Self::pow2(32 * n).modulo(m).limbs);
        let r2 = pad(Self::pow2(64 * n).modulo(m).limbs);

        let mut acc = r1; // 1, in Montgomery form
        let mut base = mont.mul(&pad(self.modulo(m).limbs), &r2);
        for i in (0..exp.bit_length()).rev() {
            if exp.bit(i) {
                acc = mont.mul(&acc, &base);
                base = mont.mul(&base, &base);
            } else {
                base = mont.mul(&acc, &base);
                acc = mont.mul(&acc, &acc);
            }
        }
        let mut out = BigUint {
            limbs: mont.from_mont(&acc),
        };
        out.normalize();
        out
    }

    /// Division-based ladder — the fallback for an even modulus, which has no
    /// Montgomery form. Byte-for-byte the routine `modpow` was before the
    /// Montgomery split, so the even case is unchanged.
    fn modpow_dividing(&self, exp: &BigUint, m: &BigUint) -> BigUint {
        let mut r0 = BigUint::one();
        let mut r1 = self.modulo(m);
        let bits = exp.bit_length();
        for i in (0..bits).rev() {
            if exp.bit(i) {
                r0 = r0.mul(&r1).modulo(m);
                r1 = r1.mul(&r1).modulo(m);
            } else {
                r1 = r0.mul(&r1).modulo(m);
                r0 = r0.mul(&r0).modulo(m);
            }
        }
        r0
    }

    /// `2^k` as a magnitude.
    fn pow2(k: usize) -> BigUint {
        let mut limbs = vec![0u32; k / 32];
        limbs.push(1u32 << (k % 32));
        BigUint { limbs }
    }

    /// Extended GCD. Returns (gcd, x, y) such that a*x + b*y = gcd.
    /// x and y may be negative, returned as (BigUint, bool) pairs.
    pub fn extended_gcd(a: &BigUint, b: &BigUint) -> (BigUint, BigUint, bool, BigUint, bool) {
        if b.is_zero() {
            return (a.clone(), BigUint::one(), false, BigUint::zero(), false);
        }
        let (q, r) = a.div_rem(b);
        let (g, x1, x1_neg, y1, y1_neg) = BigUint::extended_gcd(b, &r);
        // x = y1, y = x1 - q * y1
        let qy = q.mul(&y1);
        let (y, y_neg) = if x1_neg == y1_neg {
            // x1 and q*y1 have same sign => y = x1 - q*y1
            if x1.cmp(&qy) != std::cmp::Ordering::Less {
                (x1.sub(&qy), x1_neg)
            } else {
                (qy.sub(&x1), !x1_neg)
            }
        } else {
            // Different signs => y = x1 + q*y1 (they add)
            (x1.add(&qy), x1_neg)
        };
        (g, y1, y1_neg, y, y_neg)
    }

    /// Modular inverse: self^-1 mod m.
    pub fn modinv(&self, m: &BigUint) -> Option<BigUint> {
        let (g, x, x_neg, _, _) = BigUint::extended_gcd(self, m);
        if !g.is_one() {
            return None;
        }
        if x_neg {
            Some(m.sub(&x.modulo(m)))
        } else {
            Some(x.modulo(m))
        }
    }

    pub fn from_random_bytes(rng: &mut SecureRandom, byte_len: usize) -> BigUint {
        let mut bytes = vec![0u8; byte_len];
        rng.next_bytes(&mut bytes);
        BigUint::from_bytes_be(&bytes)
    }
}

// ---------------------------------------------------------------------------
// RSA private-operation blinding (timing side-channel mitigation).
//
// nb-crypto-impl VULN(2): `BigUint::modpow` is a *variable-time* square-and-
// multiply whose per-bit branch pattern and limb-routine timing depend on both
// the exponent and the operand magnitudes. When the operand is an
// attacker-influenced message/ciphertext and the exponent is the secret RSA
// `d`, the running time directly leaks information that, across many adaptive
// queries, recovers `d` (the classic RSA timing attack, Kocher '96 / Brumley-
// Boneh '03). We cannot make `modpow` itself constant-time without routing to
// an audited crate (out of scope here), but we CAN remove the *message-
// dependent* channel by RSA base blinding: the secret-exponent modpow then runs
// on a uniformly random, message-independent operand, so its timing reveals
// nothing about the actual message/ciphertext.
// ---------------------------------------------------------------------------

/// Draw a random `BigUint` in `[2, n)` that is coprime to `n`, using the OS
/// CSPRNG directly (`os_random_bytes`). Returns `None` only if the OS source is
/// unavailable for every retry (callers then proceed unblinded rather than
/// fail). Trivial moduli (`n < 3`) also yield `None` — there is no usable
/// blinding factor and such a modulus is never a real RSA key.
fn rsa_random_coprime(n: &BigUint) -> Option<BigUint> {
    if n.cmp(&BigUint::from_u64(3)) == std::cmp::Ordering::Less {
        return None;
    }
    let byte_len = (n.bit_length() + 7) / 8;
    if byte_len == 0 {
        return None;
    }
    let two = BigUint::from_u64(2);
    // Bounded retries: rejection (out-of-range / non-coprime) is rare for an RSA
    // modulus, and an unbounded loop on a pathological OS-entropy failure would
    // hang the signing path.
    for _ in 0..64 {
        let mut bytes = vec![0u8; byte_len];
        if !os_random_bytes(&mut bytes) {
            return None;
        }
        let mut r = BigUint::from_bytes_be(&bytes).modulo(n);
        if r.cmp(&two) == std::cmp::Ordering::Less {
            r = two.clone();
        }
        // Coprimality is required so `r` is invertible mod `n`.
        let (g, _, _, _, _) = BigUint::extended_gcd(&r, n);
        if g.is_one() {
            return Some(r);
        }
    }
    None
}

/// Compute `base^d mod n` with RSA base blinding, given the public exponent `e`.
///
/// Blinding identity (gcd(r, n) == 1): `(base * r^e)^d == base^d * r^{e*d} ==
/// base^d * r (mod n)`, so `base^d == (base * r^e)^d * r^{-1} (mod n)`. The
/// single secret-exponent modpow runs on the random, message-independent
/// operand `base * r^e`. Falls back to a plain `modpow` only if a blinding
/// factor cannot be drawn (OS entropy down) — never silently producing a wrong
/// result.
fn rsa_private_modpow_blinded(base: &BigUint, d: &BigUint, e: &BigUint, n: &BigUint) -> BigUint {
    if let Some(r) = rsa_random_coprime(n) {
        if let Some(r_inv) = r.modinv(n) {
            let re = r.modpow(e, n);
            let blinded = base.mul(&re).modulo(n);
            let s_blinded = blinded.modpow(d, n);
            return s_blinded.mul(&r_inv).modulo(n);
        }
    }
    base.modpow(d, n)
}

/// `base^d mod n` through the Chinese Remainder Theorem, using the
/// `(p, q, dP, dQ, qInv)` the key already carries.
///
/// Two half-width exponentiations replace one full-width one. A modexp is
/// `O(exponent_bits x limbs^2)`, so halving both is `2 * 1/2 * 1/4 = 1/4` — the
/// ~4x that `internal/performance/rsa-private-key-op-crt-FIXED-20260817`
/// named.
///
/// Returns `None` — sending the caller back to the full-width `d` — in every
/// case where the CRT answer cannot be *trusted*, not merely where it cannot be
/// computed:
///
/// * the key carries no CRT parameters (a bare `(n, d)` import), or they are
///   structurally degenerate;
/// * the public exponent cannot verify the result (see below);
/// * **the fault check fails.**
///
/// ## The fault check is not optional
///
/// CRT-RSA that returns an unverified result is the Bellcore fault attack: if
/// exactly one half computes wrongly — a bit flip, a malformed imported
/// parameter, or a bug in this function — then `gcd(s - s_correct, n)` hands
/// the attacker a factor of `n`, and a *single* faulty signature is enough.
/// This matters here specifically because CRT parameters do not only come from
/// `generate_keypair`: `parse_rsa_private_key_der` reads all five straight out
/// of a PKCS#8 file with no consistency check, so `p` and `q` can be attacker-
/// supplied and need not satisfy `n == p*q` at all.
///
/// So the result is verified with one public exponentiation before it is
/// returned. `e` is small (65537 in practice, 17 bits), which makes the check
/// ~3% of the operation it protects. A key whose `e` is too small to verify
/// with gets no CRT at all rather than an unverified fast path — declining to
/// go fast is always available, and going fast unverified is not.
fn rsa_crt_exponentiate(key: &RsaPrivateKey, base: &BigUint) -> Option<BigUint> {
    let (Some(p), Some(q), Some(dp), Some(dq), Some(qinv)) = (
        key.p.as_ref(),
        key.q.as_ref(),
        key.dp.as_ref(),
        key.dq.as_ref(),
        key.qinv.as_ref(),
    ) else {
        return None; // bare (n, d) import — nothing to use
    };
    // A zero or one modulus has no residue ring worth the name, and `modulo`
    // by zero is not a question this should be asking.
    if p.is_zero() || q.is_zero() || p.is_one() || q.is_one() {
        return None;
    }
    // No verifiable public exponent => no CRT. See the fault-check note above.
    if key.e.cmp(&BigUint::from_u64(3)) == std::cmp::Ordering::Less {
        return None;
    }

    // The two halves. Reducing the base first is what makes each exponentiation
    // half-width in the *modulus* as well as the exponent.
    let m1 = base.modulo(p).modpow(dp, p);
    let m2 = base.modulo(q).modpow(dq, q);

    // Garner recombination: h = qInv * (m1 - m2) mod p, m = m2 + q*h.
    // `BigUint::sub` panics on underflow and `m1 < m2` is perfectly ordinary,
    // so the difference is taken in [0, p) explicitly. `m2` is reduced mod `p`
    // first because nothing here guarantees `q < p` — the JDK convention holds
    // for generated keys, but an imported key may carry them either way round,
    // and Garner does not actually care as long as `qInv*q == 1 (mod p)`.
    let m2p = m2.modulo(p);
    let diff = if m1.cmp(&m2p) == std::cmp::Ordering::Less {
        m1.add(p).sub(&m2p)
    } else {
        m1.sub(&m2p)
    };
    let h = qinv.mul(&diff).modulo(p);
    let m = m2.add(&q.mul(&h));

    // Bellcore. Everything above is unverified arithmetic until this passes.
    if m.modpow(&key.e, &key.n).cmp(&base.modulo(&key.n)) != std::cmp::Ordering::Equal {
        return None;
    }
    Some(m)
}

/// `base^d mod n`, by CRT when the key can support a *verified* one and by the
/// full-width exponent otherwise. The fallback is what makes every refusal in
/// [`rsa_crt_exponentiate`] safe: declining CRT costs speed, never correctness.
fn rsa_private_exponentiate(key: &RsaPrivateKey, base: &BigUint) -> BigUint {
    match rsa_crt_exponentiate(key, base) {
        Some(m) => m,
        None => base.modpow(&key.d, &key.n),
    }
}

/// The private-key operation every signing path should call: base blinding on
/// the outside, CRT on the inside.
///
/// The composition order is the one OpenSSL uses and it is the only one that
/// works: blind first, so the secret-exponent arithmetic — both CRT halves
/// included — runs on a uniformly random, message-independent operand, then
/// unblind the result. Blinding the *output* would leave the halves running on
/// attacker-chosen data and defeat the point (VULN(2)).
///
/// The fault check inside runs against the blinded base, which is exactly
/// right: it verifies the arithmetic that actually executed.
fn rsa_private_op_blinded(key: &RsaPrivateKey, base: &BigUint) -> BigUint {
    if let Some(r) = rsa_random_coprime(&key.n) {
        if let Some(r_inv) = r.modinv(&key.n) {
            let re = r.modpow(&key.e, &key.n);
            let blinded = base.mul(&re).modulo(&key.n);
            let s_blinded = rsa_private_exponentiate(key, &blinded);
            return s_blinded.mul(&r_inv).modulo(&key.n);
        }
    }
    // Blinding material unavailable (OS entropy down). Still CRT, still
    // verified — just without the message-independence property.
    rsa_private_exponentiate(key, base)
}

/// Compute `base^d mod n` with blinding when the public exponent `e` is NOT
/// available (the `Cipher` decrypt path only carries `(n, d)`).
///
/// Without `e` we cannot use the `r^e` identity, so we use two independent
/// secret-exponent modpows on uniformly random, message-independent operands:
///   * `m_blinded = (base * r)^d mod n  = base^d * r^d`
///   * `rd        = r^d mod n`
///   * `base^d    = m_blinded * (r^d)^{-1} mod n`
/// Both modpows operate on data independent of `base`, so the *message-
/// dependent* timing channel that a decryption-timing attacker exploits is
/// removed. Residual: the fixed exponent `d` still drives a variable-time
/// modpow, so a fixed (message-independent) timing profile of `d` remains —
/// eliminating that requires a constant-time core (out of scope; see VULN(2)).
/// Falls back to a single plain `modpow` if blinding material is unavailable.
fn rsa_private_modpow_blinded_no_e(base: &BigUint, d: &BigUint, n: &BigUint) -> BigUint {
    if let Some(r) = rsa_random_coprime(n) {
        let rd = r.modpow(d, n);
        if let Some(rd_inv) = rd.modinv(n) {
            let blinded = base.mul(&r).modulo(n);
            let m_blinded = blinded.modpow(d, n);
            return m_blinded.mul(&rd_inv).modulo(n);
        }
    }
    base.modpow(d, n)
}

impl PartialEq for BigUint {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for BigUint {}

// ---------------------------------------------------------------------------
// RSA implementation
// ---------------------------------------------------------------------------

pub struct RsaPublicKey {
    pub n: BigUint,
    pub e: BigUint,
}

pub struct RsaPrivateKey {
    pub n: BigUint,
    pub d: BigUint,
    pub e: BigUint,
    // CRT parameters (Chinese Remainder Theorem). `Some` for a freshly
    // generated key (see `generate_keypair`); `None` for a key reconstructed
    // from a non-CRT source (a placeholder, or a bare `(n, d)` import). When
    // all five are present, `private_key_to_der` emits the complete PKCS#1
    // `RSAPrivateKey` (9 elements) that real JDK / rustls need — a bare
    // `(n, e, d)` DER parses back (via `RSAKeyFactory$Legacy`, the default
    // `route_rsa_to_real` path) as `sun.security.rsa.RSAPrivateKeyImpl`
    // (non-CRT), whose `getEncoded()` is an incomplete 572-byte PKCS#8 that
    // rustls rejects (`failed to parse private key as RSA`). See
    // fixed-suite-bugs/http-server-sslengine-identity-singleton-clobber-FIXED.md.
    // Convention: `p > q`, matching JDK's `RSAKeyPairGenerator`.
    pub p: Option<BigUint>,
    pub q: Option<BigUint>,
    pub dp: Option<BigUint>,   // d mod (p-1)
    pub dq: Option<BigUint>,   // d mod (q-1)
    pub qinv: Option<BigUint>, // q^{-1} mod p
}

impl Drop for RsaPrivateKey {
    fn drop(&mut self) {
        // Zeroize private key material
        for limb in &mut self.d.limbs {
            *limb = 0;
        }
        for limb in &mut self.n.limbs {
            *limb = 0;
        }
        for crt in [
            &mut self.p,
            &mut self.q,
            &mut self.dp,
            &mut self.dq,
            &mut self.qinv,
        ] {
            if let Some(v) = crt {
                for limb in &mut v.limbs {
                    *limb = 0;
                }
            }
        }
    }
}

pub struct Rsa;

impl Rsa {
    /// Miller-Rabin primality test with `k` rounds.
    fn is_probably_prime(n: &BigUint, k: usize, rng: &mut SecureRandom) -> bool {
        if n.cmp(&BigUint::from_u64(2)) == std::cmp::Ordering::Less {
            return false;
        }
        if n.cmp(&BigUint::from_u64(2)) == std::cmp::Ordering::Equal {
            return true;
        }
        if n.is_even() {
            return false;
        }

        // Write n-1 as 2^r * d
        let n_minus_1 = n.sub(&BigUint::one());
        let mut d = n_minus_1.clone();
        let mut r = 0u32;
        while d.is_even() {
            d = d.shr_bits(1);
            r += 1;
        }

        let two = BigUint::from_u64(2);
        'witness: for _ in 0..k {
            // Random a in [2, n-2]
            let byte_len = (n.bit_length() + 7) / 8;
            let mut a = BigUint::from_random_bytes(rng, byte_len);
            a = a.modulo(n);
            if a.cmp(&two) == std::cmp::Ordering::Less {
                a = two.clone();
            }

            let mut x = a.modpow(&d, n);
            if x.is_one() || x.cmp(&n_minus_1) == std::cmp::Ordering::Equal {
                continue 'witness;
            }
            for _ in 0..r - 1 {
                x = x.mul(&x).modulo(n);
                if x.cmp(&n_minus_1) == std::cmp::Ordering::Equal {
                    continue 'witness;
                }
            }
            return false;
        }
        true
    }

    /// Generate a random prime of `bits` bit length.
    fn gen_prime(bits: usize, rng: &mut SecureRandom) -> BigUint {
        let byte_len = (bits + 7) / 8;
        loop {
            let mut candidate = BigUint::from_random_bytes(rng, byte_len);
            // Set MSB and LSB
            let top_bit = bits - 1;
            candidate = candidate.set_bit(top_bit);
            candidate.limbs[0] |= 1; // make odd
                                     // Trim to exact bit length
            let target_limbs = (bits + 31) / 32;
            while candidate.limbs.len() > target_limbs {
                candidate.limbs.pop();
            }
            if candidate.limbs.len() == target_limbs && bits % 32 != 0 {
                let mask = (1u32 << (bits % 32)) - 1;
                *candidate.limbs.last_mut().unwrap() &= mask;
                *candidate.limbs.last_mut().unwrap() |= 1u32 << ((bits % 32) - 1);
            }

            // Quick small-factor check
            let small_primes: &[u64] = &[3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47];
            let mut skip = false;
            for &sp in small_primes {
                let spb = BigUint::from_u64(sp);
                if candidate.modulo(&spb).is_zero()
                    && candidate.cmp(&spb) != std::cmp::Ordering::Equal
                {
                    skip = true;
                    break;
                }
            }
            if skip {
                continue;
            }

            if Self::is_probably_prime(&candidate, 20, rng) {
                return candidate;
            }
        }
    }

    /// Generate an RSA key pair with the given bit length (e.g. 2048).
    pub fn generate_keypair(bits: usize) -> (RsaPublicKey, RsaPrivateKey) {
        let mut rng = SecureRandom::new();
        let half = bits / 2;
        let e = BigUint::from_u64(65537);
        loop {
            let mut p = Self::gen_prime(half, &mut rng);
            let mut q = Self::gen_prime(half, &mut rng);
            if p.cmp(&q) == std::cmp::Ordering::Equal {
                continue;
            }
            // JDK's `RSAKeyPairGenerator` orders `p > q` (the CRT coefficient
            // is `q^{-1} mod p`, which requires the prime it is reduced modulo
            // to be the larger one). Swap so the emitted key matches that
            // convention exactly.
            if p.cmp(&q) == std::cmp::Ordering::Less {
                std::mem::swap(&mut p, &mut q);
            }
            let n = p.mul(&q);
            if n.bit_length() != bits {
                continue;
            }
            let p1 = p.sub(&BigUint::one());
            let q1 = q.sub(&BigUint::one());
            let phi = p1.mul(&q1);
            // Verify gcd(e, phi) == 1 (coprimality requirement)
            let (gcd, _, _, _, _) = BigUint::extended_gcd(&e, &phi);
            if !gcd.is_one() {
                continue;
            }
            if let Some(d) = e.modinv(&phi) {
                // CRT parameters, computed once here so the full PKCS#1
                // `RSAPrivateKey` can be emitted (see `private_key_to_der`).
                // `qinv = q^{-1} mod p` exists because p and q are distinct
                // primes (gcd(q, p) == 1); the `Option`-guarded fallback keeps
                // keygen infallible if `modinv` ever returns `None`.
                let dp = d.modulo(&p1);
                let dq = d.modulo(&q1);
                let qinv = match q.modinv(&p) {
                    Some(v) => v,
                    None => continue,
                };
                let pub_key = RsaPublicKey {
                    n: n.clone(),
                    e: e.clone(),
                };
                let priv_key = RsaPrivateKey {
                    n,
                    d,
                    e: e.clone(),
                    p: Some(p),
                    q: Some(q),
                    dp: Some(dp),
                    dq: Some(dq),
                    qinv: Some(qinv),
                };
                return (pub_key, priv_key);
            }
        }
    }

    /// PKCS#1 v1.5 SHA-256 signature.
    ///
    /// Returns an empty `Vec` if the key is too small to hold the DigestInfo
    /// plus the minimum PKCS#1 v1.5 padding (RFC 8017 §9.2 requires `k >=
    /// tLen + 11`). Callers must treat an empty result as a failure; a real
    /// RSA key used for SHA-256 signing is always large enough.
    pub fn sign_sha256(key: &RsaPrivateKey, message: &[u8]) -> Vec<u8> {
        let hash = Sha256::digest(message);
        let k = (key.n.bit_length() + 7) / 8;
        let Some(em) = Self::pkcs1v15_encode(&hash, k) else {
            return Vec::new();
        };
        let m = BigUint::from_bytes_be(&em);
        // Base-blinded private exponentiation: removes the message-dependent
        // timing channel of the variable-time `modpow` (VULN(2)). The public
        // exponent `e` is available on `RsaPrivateKey`, so use the efficient
        // single-secret-modpow `r^e` blinding.
        let s = rsa_private_op_blinded(key, &m);
        s.to_bytes_be_padded(k)
    }

    /// PKCS#1 v1.5 SHA-256 verification, **checked**.
    ///
    /// `Ok(true)` / `Ok(false)` is the genuine cryptographic answer: these
    /// bytes are, or are not, a valid signature over this message under this
    /// key. `Err` means the question was never asked — the backend rejected
    /// the *key* (an even exponent, `e < 2`, `e > 2³³−1`, a modulus over
    /// `RsaPublicKey::MAX_SIZE` = 4096 bits, or an absent/zero component) or
    /// the signature's *length* was wrong for the modulus, both of which are
    /// refusals before any RSA operation runs. See
    /// `docs/security/crypto-failure-contract.md` §2.1 items 1–4.
    ///
    /// The distinction matters because a legitimate 8192-bit signer key is
    /// rejected by the backend, and collapsing that to `false` reports
    /// "signature did not verify" when nothing was checked.
    pub fn try_verify_sha256(
        key: &RsaPublicKey,
        message: &[u8],
        signature: &[u8],
    ) -> Result<bool, cratonvm_native_builtins_crypto::failure::CryptoFailure> {
        cratonvm_native_builtins_crypto::signature::verify_rsa_pkcs1_v15_checked(
            &key.n.to_bytes_be(),
            &key.e.to_bytes_be(),
            cratonvm_native_builtins_crypto::signature::DigestAlgorithm::Sha256,
            message,
            signature,
        )
    }

    /// PKCS#1 v1.5 SHA-256 verification, **fail-closed `bool`**.
    ///
    /// Retained for the callers whose surface is a `bool` — certificate-chain
    /// validation (`x509_manager`, `checkServerTrusted`), where a refusal and
    /// a mismatch both mean "do not trust this chain" and the caller has no
    /// exception channel. `Err` can never surface as `true`; `matches!` is
    /// used rather than `unwrap_or(false)` so this stays a deliberate,
    /// greppable collapse.
    ///
    /// Callers that DO have an exception channel — `Signature.verify()`, via
    /// [`rsa_verify`] — must use [`try_verify_sha256`] instead, so that
    /// "unusable key" is not reported to Java as "forged signature".
    ///
    /// A modulus too small to hold the DigestInfo + padding can never carry a
    /// valid PKCS#1 v1.5 signature. Rejecting it (rather than underflowing the
    /// padding-length math) is reachable from
    /// `verify_signature`/`checkServerTrusted` with an attacker-supplied
    /// issuer key carrying a tiny RSA modulus.
    pub fn verify_sha256(key: &RsaPublicKey, message: &[u8], signature: &[u8]) -> bool {
        matches!(Self::try_verify_sha256(key, message, signature), Ok(true))
    }

    /// PKCS#1 v1.5 verification for an arbitrary digest, **fail-closed
    /// `bool`** — [`Self::verify_sha256`] generalised.
    ///
    /// The certificate-chain verifier (`x509_manager::verify_one_signature`)
    /// dispatches on the signature-algorithm OID, and every
    /// `sha*WithRSAEncryption` differs from the next ONLY in which digest goes
    /// into the DigestInfo. The core it delegates to has taken a
    /// [`DigestAlgorithm`] all along, so the whole RSA family is this one
    /// function rather than four near-copies — and in particular nothing here
    /// touches the in-tree [`Self::pkcs1v15_encode`], whose DigestInfo prefix
    /// is hard-coded to SHA-256.
    ///
    /// Same fail-closed collapse and the same reason as `verify_sha256`: the
    /// caller (chain validation) has no exception channel, and a refusal and a
    /// mismatch both mean "do not trust this chain".
    pub fn verify_pkcs1_v15(
        key: &RsaPublicKey,
        digest: cratonvm_native_builtins_crypto::signature::DigestAlgorithm,
        message: &[u8],
        signature: &[u8],
    ) -> bool {
        matches!(
            cratonvm_native_builtins_crypto::signature::verify_rsa_pkcs1_v15_checked(
                &key.n.to_bytes_be(),
                &key.e.to_bytes_be(),
                digest,
                message,
                signature,
            ),
            Ok(true)
        )
    }

    /// Build the PKCS#1 v1.5 EMSA encoding (DigestInfo for SHA-256 wrapped in
    /// `00 01 FF.. 00 || T`).
    ///
    /// Returns `None` when the encoded-message length `k` is smaller than the
    /// minimum permitted by RFC 8017 §9.2 (`k >= tLen + 11`). Without this
    /// check the `k - t_len - 3` padding-length computation underflows on a
    /// small key (panic in debug; in release a wrap to ~`usize::MAX` then a
    /// multi-exabyte `repeat(0xff).take(..)` allocation → abort) — a DoS
    /// reachable from a malicious certificate chain.
    fn pkcs1v15_encode(hash: &[u8], k: usize) -> Option<Vec<u8>> {
        Self::pkcs1v15_encode_digest(
            cratonvm_native_builtins_crypto::signature::DigestAlgorithm::Sha256,
            hash,
            k,
        )
    }

    /// PKCS#1 v1.5 SHA-256 signature over an already-computed digest, with the
    /// DigestInfo prefix of the digest that produced it.
    ///
    /// The verify side has been digest-parameterised all along
    /// ([`Self::verify_pkcs1_v15`]); the SIGN side was SHA-256 only, so
    /// `Signature.getInstance("SHA1withRSA"|"SHA384withRSA"|"SHA512withRSA")`
    /// reached `sign()` and then refused with "this VM has no native
    /// implementation for that algorithm". netty's
    /// `JdkDelegatingPrivateKeyMethod` asks for exactly those three by name.
    pub fn sign_pkcs1_v15(
        key: &RsaPrivateKey,
        digest: cratonvm_native_builtins_crypto::signature::DigestAlgorithm,
        message: &[u8],
    ) -> Vec<u8> {
        use cratonvm_native_builtins_crypto::signature::DigestAlgorithm as D;
        let hash: Vec<u8> = match digest {
            D::Sha1 => {
                use sha1::Digest;
                let mut h = sha1::Sha1::new();
                h.update(message);
                h.finalize().to_vec()
            }
            D::Sha256 => Sha256::digest(message).to_vec(),
            D::Sha384 => Sha384::digest(message).to_vec(),
            D::Sha512 => Sha512::digest(message).to_vec(),
        };
        let k = (key.n.bit_length() + 7) / 8;
        let Some(em) = Self::pkcs1v15_encode_digest(digest, &hash, k) else {
            return Vec::new();
        };
        let m = BigUint::from_bytes_be(&em);
        rsa_private_op_blinded(key, &m).to_bytes_be_padded(k)
    }

    fn pkcs1v15_encode_digest(
        digest: cratonvm_native_builtins_crypto::signature::DigestAlgorithm,
        hash: &[u8],
        k: usize,
    ) -> Option<Vec<u8>> {
        use cratonvm_native_builtins_crypto::signature::DigestAlgorithm as D;
        // DigestInfo DER prefixes (RFC 8017 §9.2 note 1).
        let digest_info_prefix: &[u8] = match digest {
            D::Sha1 => &[
                0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04,
                0x14,
            ],
            D::Sha256 => &[
                0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x01, 0x05, 0x00, 0x04, 0x20,
            ],
            D::Sha384 => &[
                0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x02, 0x05, 0x00, 0x04, 0x30,
            ],
            D::Sha512 => &[
                0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x03, 0x05, 0x00, 0x04, 0x40,
            ],
        };
        let t_len = digest_info_prefix.len() + hash.len();
        // Need: 00 01 || PS(>=8 bytes of FF) || 00 || T  => k >= t_len + 11.
        // `ps_len = k - t_len - 3` must be >= 8, equivalently k >= t_len + 11.
        let ps_len = k.checked_sub(t_len + 3).filter(|&ps| ps >= 8)?;
        let mut em = Vec::with_capacity(k);
        em.push(0x00);
        em.push(0x01);
        em.extend(std::iter::repeat(0xff).take(ps_len));
        em.push(0x00);
        em.extend_from_slice(digest_info_prefix);
        em.extend_from_slice(hash);
        Some(em)
    }

    /// Build the PKCS#1 v1.5 **block type 1** encoding with NO DigestInfo:
    /// `00 01 FF..FF 00 || M`.
    ///
    /// This is what `NONEwithRSA` signs. The algorithm takes the caller's bytes
    /// as the already-computed digest and does NOT wrap them in a DigestInfo —
    /// the caller who chose `NONEwithRSA` is the one who decides what the
    /// payload means, which is the whole point of the algorithm and also why it
    /// is the one RSA signature scheme SunJCE serves rather than SunRsaSign
    /// (SunJCE implements it by encrypting under the private key through
    /// `com.sun.crypto.provider.RSACipherAdaptor`, i.e. exactly this padding).
    ///
    /// `None` for a payload that cannot fit: RFC 8017 §9.2 wants at least eight
    /// `FF` bytes, so `k >= m.len() + 11`. Returning `None` rather than
    /// underflowing `k - m_len - 3` is the same guard, and for the same reason,
    /// as [`Self::pkcs1v15_encode`].
    fn pkcs1v15_encode_raw(m: &[u8], k: usize) -> Option<Vec<u8>> {
        let ps_len = k.checked_sub(m.len() + 3).filter(|&ps| ps >= 8)?;
        let mut em = Vec::with_capacity(k);
        em.push(0x00);
        em.push(0x01);
        em.extend(std::iter::repeat(0xff).take(ps_len));
        em.push(0x00);
        em.extend_from_slice(m);
        Some(em)
    }

    /// `NONEwithRSA` sign — PKCS#1 v1.5 block type 1 over the raw payload.
    ///
    /// `None` when the payload is too long for the modulus, which is the
    /// condition SunJCE reports as `SignatureException`; every caller here has
    /// that channel.
    pub fn sign_none(key: &RsaPrivateKey, data: &[u8]) -> Option<Vec<u8>> {
        let k = (key.n.bit_length() + 7) / 8;
        let em = Self::pkcs1v15_encode_raw(data, k)?;
        let m = BigUint::from_bytes_be(&em);
        // Blinded, for the same VULN(2) reason `sign_sha256` is blinded: the
        // variable-time `modpow` otherwise leaks a message-dependent timing
        // signal, and `NONEwithRSA` payloads are frequently attacker-chosen.
        let sig = rsa_private_op_blinded(key, &m);
        Some(sig.to_bytes_be_padded(k))
    }

    /// `NONEwithRSA` verify.
    ///
    /// `Some(bool)` is the genuine answer; `None` means the question was never
    /// asked — a signature of the wrong length for the modulus, or a payload
    /// that cannot be encoded under it. The same three-valued contract
    /// [`Self::try_verify_sha256`] carries, and for the same reason: collapsing
    /// a refusal to `false` reports "forged" where nothing was checked.
    ///
    /// The comparison is against the RE-ENCODED expected block, so a signature
    /// whose recovered block has the right payload but malformed padding is
    /// rejected — the Bleichenbacher'06 signature-forgery shape. Nothing here
    /// parses the recovered bytes.
    pub fn verify_none(key: &RsaPublicKey, data: &[u8], signature: &[u8]) -> Option<bool> {
        let k = (key.n.bit_length() + 7) / 8;
        if signature.len() != k {
            return None;
        }
        let expected = Self::pkcs1v15_encode_raw(data, k)?;
        let c = BigUint::from_bytes_be(signature);
        if c.cmp(&key.n) != std::cmp::Ordering::Less {
            return None;
        }
        let m = c.modpow(&key.e, &key.n);
        Some(m.to_bytes_be_padded(k) == expected)
    }

    /// Serialize public key to DER (SubjectPublicKeyInfo).
    pub fn public_key_to_der(key: &RsaPublicKey) -> Vec<u8> {
        let n_bytes = key.n.to_bytes_be();
        let e_bytes = key.e.to_bytes_be();
        let n_der = der_encode_integer(&n_bytes);
        let e_der = der_encode_integer(&e_bytes);
        let mut seq_inner = Vec::new();
        seq_inner.extend_from_slice(&n_der);
        seq_inner.extend_from_slice(&e_der);
        let rsa_key_seq = der_encode_sequence(&seq_inner);

        // AlgorithmIdentifier for RSA: OID 1.2.840.113549.1.1.1 + NULL
        let alg_oid: &[u8] = &[
            0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
        ];
        let alg_id = der_encode_sequence(alg_oid);

        // BIT STRING wrapping the RSA key sequence
        let mut bit_string = vec![0x03];
        let bs_content_len = rsa_key_seq.len() + 1; // +1 for unused bits byte
        bit_string.extend_from_slice(&der_encode_length(bs_content_len));
        bit_string.push(0x00); // unused bits
        bit_string.extend_from_slice(&rsa_key_seq);

        let mut spki = Vec::new();
        spki.extend_from_slice(&alg_id);
        spki.extend_from_slice(&bit_string);
        der_encode_sequence(&spki)
    }

    /// Serialize private key to PKCS#8 DER.
    /// Encode a private key as the PKCS#1 `RSAPrivateKey` ASN.1 SEQUENCE.
    ///
    /// When the CRT parameters are present (every freshly generated key — see
    /// `generate_keypair`), emits the COMPLETE 9-element form
    /// `{version, n, e, d, p, q, dP, dQ, qInv}` that RFC 8017 / real JDK /
    /// rustls require. A key without CRT params (an imported bare `(n, d)`)
    /// falls back to the legacy 4-element `{version, n, e, d}` — accepted by
    /// JDK as a non-CRT key, but not usable by rustls as a server identity.
    ///
    /// The prior implementation ALWAYS emitted only the 4-element form, so a
    /// generated key round-tripped through `RSAKeyFactory$Legacy`
    /// (`route_rsa_to_real`) came back as `sun.security.rsa.RSAPrivateKeyImpl`
    /// (non-CRT), whose `getEncoded()` is the incomplete 572-byte PKCS#8 that
    /// broke TLS server identities built from generated keys. See
    /// fixed-suite-bugs/http-server-sslengine-identity-singleton-clobber-FIXED.md.
    pub fn private_key_to_der(key: &RsaPrivateKey) -> Vec<u8> {
        let n_bytes = key.n.to_bytes_be();
        let d_bytes = key.d.to_bytes_be();
        let e_bytes = key.e.to_bytes_be();
        let mut inner = Vec::new();
        inner.extend_from_slice(&der_encode_integer(&[0])); // version = 0 (two-prime)
        inner.extend_from_slice(&der_encode_integer(&n_bytes));
        inner.extend_from_slice(&der_encode_integer(&e_bytes));
        inner.extend_from_slice(&der_encode_integer(&d_bytes));
        if let (Some(p), Some(q), Some(dp), Some(dq), Some(qinv)) =
            (&key.p, &key.q, &key.dp, &key.dq, &key.qinv)
        {
            inner.extend_from_slice(&der_encode_integer(&p.to_bytes_be()));
            inner.extend_from_slice(&der_encode_integer(&q.to_bytes_be()));
            inner.extend_from_slice(&der_encode_integer(&dp.to_bytes_be()));
            inner.extend_from_slice(&der_encode_integer(&dq.to_bytes_be()));
            inner.extend_from_slice(&der_encode_integer(&qinv.to_bytes_be()));
        }
        der_encode_sequence(&inner)
    }
}

// ---------------------------------------------------------------------------
// RSA encryption / decryption with PKCS#1 v1.5 (type 2) and OAEP (MGF1)
// padding — drives the `javax.crypto.Cipher` "RSA/ECB/{PKCS1Padding,
// OAEPWithSHA-1AndMGF1Padding, OAEPWithSHA-256AndMGF1Padding}" transformations.
//
// The caller supplies the raw big-endian `(modulus, exponent)` magnitudes
// extracted from the `Key` object, so this works uniformly for genuine
// `sun.security.rsa.RSAPublic/PrivateKeyImpl` keys (route_rsa_to_real, the
// default) and for the bare synthetic keys (CRATONVM_SYNTHETIC_RSA=1) — both
// honour `getModulus()`/`getPublic/PrivateExponent()` or carry a `key_id` the
// caller resolves first. Padding is implemented per RFC 8017 (§7.1 OAEP, §7.2
// RSAES-PKCS1-v1_5); the RSA primitive reuses the existing `BigUint::modpow`.
// This avoids the JceSecurity provider-list machinery that the real
// `RSACipher.engineSetPadding("OAEP…")` path needs (and which CratonVM's empty
// provider list cannot satisfy → `getService on null`).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RsaCipherPadding {
    Pkcs1,
    OaepSha1,
    OaepSha256,
}

impl RsaCipherPadding {
    /// Parse the padding component of a `Cipher` transformation string
    /// (the part after `RSA/ECB/`). Returns `None` for unsupported paddings.
    pub fn from_transformation(padding: &str) -> Option<Self> {
        let p = padding.trim();
        if p.eq_ignore_ascii_case("PKCS1Padding") {
            Some(RsaCipherPadding::Pkcs1)
        } else if p.eq_ignore_ascii_case("OAEPWithSHA-1AndMGF1Padding")
            || p.eq_ignore_ascii_case("OAEPWithSHA1AndMGF1Padding")
            || p.eq_ignore_ascii_case("OAEPPadding")
        {
            Some(RsaCipherPadding::OaepSha1)
        } else if p.eq_ignore_ascii_case("OAEPWithSHA-256AndMGF1Padding")
            || p.eq_ignore_ascii_case("OAEPWithSHA256AndMGF1Padding")
        {
            Some(RsaCipherPadding::OaepSha256)
        } else {
            None
        }
    }

    fn hlen(self) -> usize {
        match self {
            RsaCipherPadding::OaepSha1 => 20,
            _ => 32,
        }
    }

    /// The largest plaintext this padding can carry under a `k`-byte modulus —
    /// SunJCE's `RSAPadding.getMaxDataSize()`: `k - 11` for PKCS#1 v1.5,
    /// `k - 2*hLen - 2` for OAEP. `None` when the modulus is too small to hold
    /// the padding at all, which SunJCE reports from `RSAPadding.getInstance`
    /// as an `InvalidKeyException` rather than as a data failure.
    ///
    /// This is the number the LENGTH refusal has to be measured against, and
    /// keeping it here rather than inside `rsa_pkcs1_type2_pad` /
    /// `rsa_oaep_pad` is deliberate: those two decide padding VALIDITY and
    /// their error strings are held to a single opaque constant so they cannot
    /// become a Bleichenbacher/Manger oracle. A length that does not fit is not
    /// secret — the caller chose it — and it is a different JCA exception, so
    /// it is decided before either of them is entered.
    fn max_data_size(self, k: usize) -> Option<usize> {
        let overhead = match self {
            RsaCipherPadding::Pkcs1 => 11,
            _ => 2 * self.hlen() + 2,
        };
        k.checked_sub(overhead).filter(|_| k > overhead)
    }
}

/// Which JCA exception an RSA `Cipher` failure has to be raised as.
///
/// The two `Cipher.doFinal` DECLARES are both CHECKED members of
/// `java.security.GeneralSecurityException`, and the class is the only thing a
/// caller's `catch` selects on. `rsa_cipher_encrypt`/`rsa_cipher_decrypt`
/// returned `Result<_, String>` and every caller collapsed the whole set into
/// an unchecked `IllegalStateException`, so a caller who wrote the JDK's own
/// `catch (BadPaddingException e)` around an RSA decrypt did NOT catch it: the
/// failure escaped as an unchecked throw through code that believed it had
/// handled it. This is the same species as `W7-41`'s `IllegalFormatException`
/// subclasses and `W7-46`'s `ProcessBuilder("").start()`.
///
/// Measured on Temurin 25.0.3+9 (`probes/JcaExceptionTypeProbe.java`):
///
/// ```text
/// OAEP / PKCS1 decrypt under the wrong private key   BadPaddingException: Padding error in decryption
/// OAEP / PKCS1 decrypt of a corrupted ciphertext     BadPaddingException: Padding error in decryption
/// PKCS1 decrypt of a SHORTER-than-modulus ciphertext BadPaddingException: Padding error in decryption
/// PKCS1 decrypt of a LONGER-than-modulus ciphertext  IllegalBlockSizeException: Data must not be longer than 256 bytes
/// PKCS1 encrypt of 246 bytes under RSA-2048          IllegalBlockSizeException: Data must not be longer than 245 bytes
/// OAEP-SHA-256 encrypt of 191 bytes under RSA-2048   IllegalBlockSizeException: Data must not be longer than 190 bytes
/// NoPadding encrypt of a value >= the modulus        BadPaddingException: Message is larger than modulus
/// ```
///
/// Note the short-vs-long asymmetry, which is the row a length check written
/// as `ct.len() != k` gets wrong in both directions at once: SunJCE's
/// `RSACipher.doFinal` refuses only `bufOfs > buffer.length`, and a SHORTER
/// ciphertext is simply a smaller integer that goes through the modexp and
/// fails to unpad.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RsaCipherError {
    /// A LENGTH failure — `javax.crypto.IllegalBlockSizeException`.
    BlockSize(String),
    /// A PADDING or integrity failure — `javax.crypto.BadPaddingException`.
    Padding(String),
    /// An unusable KEY — `java.security.InvalidKeyException`. SunJCE raises
    /// this at `init`; this engine captures the key components at `init` and
    /// can only discover an unusable one here, so the class is kept truthful
    /// even though the point differs.
    Key(String),
}

impl RsaCipherError {
    /// The internal-form JCA class name to raise. Every one of the three is a
    /// class the real JDK carries, so `throw_jca_exc` resolves it — raising a
    /// name that does not resolve would convert a wrong-exception defect into a
    /// `NoClassDefFoundError`, which is worse.
    pub fn jca_class(&self) -> &'static str {
        match self {
            RsaCipherError::BlockSize(_) => "javax/crypto/IllegalBlockSizeException",
            RsaCipherError::Padding(_) => "javax/crypto/BadPaddingException",
            RsaCipherError::Key(_) => "java/security/InvalidKeyException",
        }
    }

    /// The detail text.
    pub fn message(&self) -> &str {
        match self {
            RsaCipherError::BlockSize(m) | RsaCipherError::Padding(m) | RsaCipherError::Key(m) => m,
        }
    }
}

/// Hash `data` with the OAEP padding's digest (SHA-1 or SHA-256).
fn rsa_oaep_hash(pad: RsaCipherPadding, data: &[u8]) -> Vec<u8> {
    match pad {
        RsaCipherPadding::OaepSha1 => {
            use sha1::Digest;
            let mut h = sha1::Sha1::new();
            h.update(data);
            h.finalize().to_vec()
        }
        _ => {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(data);
            h.finalize().to_vec()
        }
    }
}

/// The digest MGF1 uses, which is NOT the OAEP message digest.
///
/// `OAEPWith<md>AndMGF1Padding` names only `<md>`, and SunJCE reads the rest
/// from `OAEPParameterSpec`'s default rather than from the name: MGF1 is
/// **SHA-1** whatever `<md>` is. That is a documented JDK quirk and it is
/// interop-visible, because a peer that defaults MGF1 to `<md>` instead
/// produces ciphertext this engine cannot read and vice versa.
///
/// Measured on jdk-25: ciphertext from SunJCE's
/// `OAEPWithSHA-256AndMGF1Padding` decrypts under BouncyCastle only when BC is
/// given `MGF1ParameterSpec.SHA1`, and refuses `MGF1ParameterSpec.SHA256`.
/// This engine answers as SunJCE, so it has to make SunJCE's choice —
/// `RSATest.oaepCompatibilityTest`, which encrypts with SunJCE and decrypts
/// with BC, failed `BadBlockException: unable to decrypt block` for every
/// digest but SHA-1, where the two happen to coincide.
///
/// NOTE: an explicit `OAEPParameterSpec` is still ignored by the `Cipher`
/// registrations (the digest is taken from the transformation string), so this
/// matches SunJCE's DEFAULT only. Honouring a caller-supplied MGF1 digest needs
/// the spec to be read at `init` first.
const OAEP_MGF1_DIGEST: RsaCipherPadding = RsaCipherPadding::OaepSha1;

/// MGF1 mask generation (RFC 8017 Appendix B.2.1). `pad` selects the MGF1
/// digest, which callers pass as [`OAEP_MGF1_DIGEST`] — not the padding's own
/// message digest.
fn rsa_mgf1(pad: RsaCipherPadding, seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + pad.hlen());
    let mut counter: u32 = 0;
    while out.len() < len {
        let mut input = Vec::with_capacity(seed.len() + 4);
        input.extend_from_slice(seed);
        input.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&rsa_oaep_hash(pad, &input));
        counter = counter.wrapping_add(1);
    }
    out.truncate(len);
    out
}

/// EME-PKCS1-v1_5 encode (RFC 8017 §7.2.1): `00 02 || PS || 00 || M`.
fn rsa_pkcs1_type2_pad(msg: &[u8], k: usize) -> Result<Vec<u8>, String> {
    if k < 11 || msg.len() > k - 11 {
        return Err(format!(
            "RSA PKCS1: message too long for key ({} > {})",
            msg.len(),
            k.saturating_sub(11)
        ));
    }
    let ps_len = k - msg.len() - 3;
    let mut rng = SecureRandom::new();
    let mut ps = Vec::with_capacity(ps_len);
    while ps.len() < ps_len {
        let mut b = [0u8; 16];
        rng.next_bytes(&mut b);
        for &x in b.iter() {
            if x != 0 {
                ps.push(x);
                if ps.len() == ps_len {
                    break;
                }
            }
        }
    }
    let mut em = Vec::with_capacity(k);
    em.push(0x00);
    em.push(0x02);
    em.extend_from_slice(&ps);
    em.push(0x00);
    em.extend_from_slice(msg);
    Ok(em)
}

/// Single, value-independent error string for every RSA decryption-padding
/// failure (PKCS#1 v1.5 and OAEP).
///
/// nb-crypto-impl VULN(1): both decoders previously returned DISTINGUISHABLE
/// error strings ("decryption error" vs "decryption error (lHash mismatch)" vs
/// "(no 0x01 marker)") and short-circuited on the first failing check with
/// data-dependent control flow. That is a classic Bleichenbacher (PKCS#1) /
/// Manger (OAEP) decryption oracle: a caller that surfaces the distinct error /
/// timing learns *which* structural check failed and can recover plaintext one
/// query at a time. All padding failures now collapse to this one message and
/// are decided by a single branch over a bitwise-accumulated failure mask.
///
/// The text is SunJCE's own, measured on Temurin 25.0.3+9 — `RSACipher.doFinal`
/// raises `BadPaddingException("Padding error in decryption")` for the wrong
/// key, for a corrupted ciphertext and for a short one alike. Matching it costs
/// nothing (it is still exactly one constant, so it still distinguishes
/// nothing) and it removes a second-order tell: an attacker who can see the
/// message at all should not be able to tell which VM produced it.
const RSA_PADDING_ERROR: &str = "Padding error in decryption";

/// Constant-time non-zero test: returns `0xFF` if `x != 0`, else `0x00`,
/// without a data-dependent branch.
#[inline(always)]
fn ct_is_nonzero_u8(x: u8) -> u8 {
    // Widen to u16, OR with its two's-complement negation: for any x != 0 the
    // result has its high bit (bit 15) set; for x == 0 it is 0. Shift that bit
    // down to a full 0xFF / 0x00 mask. Uses wrapping ops only — no overflow,
    // no data-dependent branch.
    let v = x as u16;
    let nz = v | v.wrapping_neg(); // high bit set iff v != 0
    ((nz >> 15) as u8).wrapping_neg() // 1 -> 0xFF, 0 -> 0x00
}

/// Constant-time byte equality: returns `0xFF` if `a == b`, else `0x00`.
#[inline(always)]
fn ct_eq_u8(a: u8, b: u8) -> u8 {
    !ct_is_nonzero_u8(a ^ b)
}

/// Constant-time slice equality over equal-length slices: `0xFF` if all bytes
/// match, else `0x00`. Always touches every byte (no early exit).
#[inline(always)]
fn ct_eq_bytes(a: &[u8], b: &[u8]) -> u8 {
    if a.len() != b.len() {
        return 0x00;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    !ct_is_nonzero_u8(diff)
}

/// Widen a byte selector mask (`0x00` or `0xFF`) to a full-width `usize` mask
/// (`0` or `usize::MAX`) for branch-free conditional index selection.
///
/// This replaces the earlier `(mask as u64).wrapping_neg()` idiom, which was a
/// BUG: for `mask == 0xFF` it produced `0xFF..FF01` (only the low byte negated),
/// not all-ones, so `index & widen(mask)` corrupted the recorded separator /
/// marker index. We sign-extend the byte instead: `0xFF as i8 = -1` → all-ones,
/// `0x00 as i8 = 0` → zero.
#[inline(always)]
fn ct_mask_usize(mask: u8) -> usize {
    (mask as i8 as isize) as usize
}

/// EME-PKCS1-v1_5 decode: strip `00 02 || PS || 00` and return `M`.
///
/// nb-crypto-impl VULN(1): rewritten to be constant-time in the padding
/// structure and to leak no information about *which* check failed. We scan the
/// whole buffer once, accumulating (a) a single `bad` failure mask and (b) the
/// 0x00-separator index, all without branching on secret bytes, then take a
/// single decision at the end and always return `RSA_PADDING_ERROR`.
fn rsa_pkcs1_type2_unpad(em: &[u8]) -> Result<Vec<u8>, String> {
    // Length is public (it equals the modulus byte length); a too-short buffer
    // cannot hold `00 02 || PS(>=8) || 00 || M` so it is a structural reject.
    if em.len() < 11 {
        return Err(RSA_PADDING_ERROR.into());
    }
    let mut bad: u8 = 0;
    // First two bytes must be 00 02.
    bad |= ct_is_nonzero_u8(em[0]);
    bad |= !ct_eq_u8(em[1], 0x02);

    // Walk the padding string looking for the first 0x00 separator at index
    // >= 2. `found` latches once we see it; `sep` records its index. We never
    // break out early — every byte is visited regardless of content.
    let mut found: u8 = 0; // 0xFF once the separator has been seen
    let mut sep: usize = 0;
    for (i, &b) in em.iter().enumerate().skip(2) {
        let is_zero = ct_eq_u8(b, 0x00);
        // first-zero = is_zero & !found
        let first = is_zero & !found;
        // conditionally record the index for the first zero only
        sep |= i & ct_mask_usize(first);
        found |= is_zero;
    }
    // A valid separator must exist (found) and leave PS >= 8 bytes, i.e. the
    // separator index must be >= 10 (bytes 2..sep are the >=8 non-zero PS).
    bad |= !found;
    // sep < 10  =>  invalid PS length. Compare without branching: build a mask
    // that is 0xFF when sep < 10. Since sep is small, a direct comparison here
    // does not leak plaintext (it only reflects the padding length, which the
    // attacker already influences), but we still fold it into `bad`.
    let ps_too_short = if found != 0 && sep < 10 {
        0xFFu8
    } else {
        0x00u8
    };
    bad |= ps_too_short;

    if bad != 0 {
        return Err(RSA_PADDING_ERROR.into());
    }
    Ok(em[sep + 1..].to_vec())
}

/// EME-OAEP encode (RFC 8017 §7.1.1) with an empty label.
fn rsa_oaep_pad(pad: RsaCipherPadding, msg: &[u8], k: usize) -> Result<Vec<u8>, String> {
    let hlen = pad.hlen();
    if k < 2 * hlen + 2 || msg.len() > k - 2 * hlen - 2 {
        return Err(format!(
            "RSA OAEP: message too long for key ({} > {})",
            msg.len(),
            k.saturating_sub(2 * hlen + 2)
        ));
    }
    let lhash = rsa_oaep_hash(pad, &[]);
    let ps_len = k - msg.len() - 2 * hlen - 2;
    // DB = lHash || PS(0x00 * ps_len) || 0x01 || M   (length k - hlen - 1)
    let mut db = Vec::with_capacity(k - hlen - 1);
    db.extend_from_slice(&lhash);
    db.extend(std::iter::repeat(0u8).take(ps_len));
    db.push(0x01);
    db.extend_from_slice(msg);
    let mut seed = vec![0u8; hlen];
    SecureRandom::new().next_bytes(&mut seed);
    let db_mask = rsa_mgf1(OAEP_MGF1_DIGEST, &seed, k - hlen - 1);
    let masked_db: Vec<u8> = db.iter().zip(db_mask.iter()).map(|(a, b)| a ^ b).collect();
    let seed_mask = rsa_mgf1(OAEP_MGF1_DIGEST, &masked_db, hlen);
    let masked_seed: Vec<u8> = seed
        .iter()
        .zip(seed_mask.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    let mut em = Vec::with_capacity(k);
    em.push(0x00);
    em.extend_from_slice(&masked_seed);
    em.extend_from_slice(&masked_db);
    Ok(em)
}

/// EME-OAEP decode (RFC 8017 §7.1.2) with an empty label.
///
/// nb-crypto-impl VULN(1): rewritten to Manger-resistant constant time. The
/// previous version checked `em[0] != 0x00` first (early reject), then the
/// lHash, then the 0x01 marker, each returning a *different* error string —
/// exactly the structure Manger's attack exploits to recover plaintext from a
/// single chosen-ciphertext byte at a time. We now decode unconditionally, fold
/// the leading-byte check, the lHash comparison, and the marker search into one
/// bitwise `bad` accumulator (visiting every DB byte without early exit), branch
/// exactly once, and always return the single `RSA_PADDING_ERROR` string.
fn rsa_oaep_unpad(pad: RsaCipherPadding, em: &[u8]) -> Result<Vec<u8>, String> {
    let hlen = pad.hlen();
    // Length is public (modulus byte length). A buffer that cannot even hold
    // `00 || maskedSeed || maskedDB` with a non-empty DB is a structural reject.
    if em.len() < 2 * hlen + 2 {
        return Err(RSA_PADDING_ERROR.into());
    }
    let masked_seed = &em[1..1 + hlen];
    let masked_db = &em[1 + hlen..];
    let seed_mask = rsa_mgf1(OAEP_MGF1_DIGEST, masked_db, hlen);
    let seed: Vec<u8> = masked_seed
        .iter()
        .zip(seed_mask.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    let db_mask = rsa_mgf1(OAEP_MGF1_DIGEST, &seed, masked_db.len());
    let db: Vec<u8> = masked_db
        .iter()
        .zip(db_mask.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    let lhash = rsa_oaep_hash(pad, &[]);

    let mut bad: u8 = 0;
    // Leading byte Y must be 0x00.
    bad |= ct_is_nonzero_u8(em[0]);
    // lHash' (first hlen bytes of DB) must equal lHash. ct_eq_bytes touches
    // every byte and returns 0xFF on full match, so invert to a failure mask.
    bad |= !ct_eq_bytes(&db[..hlen], &lhash[..]);

    // Scan DB[hlen..] for `PS(0x00*) || 0x01 || M`. We walk every byte once,
    // latch the first 0x01 marker, and require that everything strictly before
    // it is 0x00. `found_one` latches at the marker; once latched we stop
    // updating `msg_start` but keep iterating (no early exit).
    let mut found_one: u8 = 0; // 0xFF once the 0x01 marker has been seen
    let mut bad_before: u8 = 0; // any non-zero byte seen before the marker
    let mut msg_start: usize = 0;
    for (j, &b) in db.iter().enumerate().skip(hlen) {
        let is_one = ct_eq_u8(b, 0x01);
        let is_zero = ct_eq_u8(b, 0x00);
        // Marker is the first 0x01 while still in the PS region (!found_one).
        let marker_here = is_one & !found_one;
        // Record message start = index just after the marker (only the first).
        msg_start |= (j + 1) & ct_mask_usize(marker_here);
        // Before the marker, every byte must be 0x00 (and not the marker).
        // active = bytes we are still scrutinising as PS (before any marker).
        let active = !found_one;
        let not_pad = !is_zero & !marker_here; // byte that is neither 0x00 nor the marker
        bad_before |= active & not_pad;
        found_one |= is_one;
    }
    bad |= bad_before;
    bad |= !found_one; // a 0x01 marker must exist

    if bad != 0 {
        return Err(RSA_PADDING_ERROR.into());
    }
    Ok(db[msg_start..].to_vec())
}

/// RSA public-key encryption (ENCRYPT/WRAP): pad then `m^e mod n`.
///
/// The error type is `RsaCipherError`, not `String`: see that enum for why the
/// CLASS is the whole point and what each variant was measured against.
pub fn rsa_cipher_encrypt(
    n: &[u8],
    e: &[u8],
    pad: RsaCipherPadding,
    msg: &[u8],
) -> Result<Vec<u8>, RsaCipherError> {
    let n_big = BigUint::from_bytes_be(n);
    let e_big = BigUint::from_bytes_be(e);
    let k = (n_big.bit_length() + 7) / 8;
    if k == 0 {
        return Err(RsaCipherError::Key("RSA: invalid (zero) modulus".into()));
    }
    // The length refusal, decided here and in HotSpot's own words. SunJCE sizes
    // the encrypt buffer to `getMaxDataSize()` and reports an overflow of it as
    // `IllegalBlockSizeException("Data must not be longer than N bytes")` — a
    // CHECKED exception, and one a caller distinguishes from a padding failure
    // because it means "re-chunk", not "this ciphertext is not for you".
    let Some(max) = pad.max_data_size(k) else {
        return Err(RsaCipherError::Key(format!(
            "Key is too short for encryption using {pad:?} (modulus {k} bytes)"
        )));
    };
    if msg.len() > max {
        return Err(RsaCipherError::BlockSize(format!(
            "Data must not be longer than {max} bytes"
        )));
    }
    let em = match pad {
        RsaCipherPadding::Pkcs1 => rsa_pkcs1_type2_pad(msg, k),
        _ => rsa_oaep_pad(pad, msg, k),
    }
    .map_err(RsaCipherError::Padding)?;
    let m = BigUint::from_bytes_be(&em);
    let c = m.modpow(&e_big, &n_big);
    Ok(c.to_bytes_be_padded(k))
}

/// RSA private-key decryption (DECRYPT/UNWRAP): `c^d mod n` then unpad.
pub fn rsa_cipher_decrypt(
    n: &[u8],
    d: &[u8],
    pad: RsaCipherPadding,
    ct: &[u8],
) -> Result<Vec<u8>, RsaCipherError> {
    let n_big = BigUint::from_bytes_be(n);
    let d_big = BigUint::from_bytes_be(d);
    // Blinded private exponentiation (VULN(2)): this entry point carries only
    // `(n, d)` — no public exponent, no CRT parameters — so it takes the no-`e`
    // two-modpow blinding and the full-width exponent. Callers that can name a
    // key handle should prefer `rsa_cipher_decrypt_by_id`, which reaches the
    // CRT parameters and is ~3x faster.
    rsa_cipher_decrypt_with(&n_big, pad, ct, |c| {
        rsa_private_modpow_blinded_no_e(c, &d_big, &n_big)
    })
}

/// `rsa_cipher_decrypt` for a caller that holds a `crypto_impl` key handle.
///
/// The handle reaches the whole `RsaPrivateKey` — including the CRT parameters
/// the byte-slice entry point above cannot see — so this takes the CRT private
/// op. Returns `None` when the handle cannot be trusted to name this key, which
/// the caller treats as "fall back to the `(n, d)` form", not as a decryption
/// failure.
///
/// ## `expect_n` is a safety interlock, not a sanity check
///
/// A handle can be *wrong* rather than merely absent. The JCA layer resolves it
/// from an identity side-table and, failing that, from a fixed field slot on the
/// key object — and on a genuine JDK key that slot holds whatever that class
/// puts there. Key ids are small consecutive integers, so an unrelated small
/// `int` in that slot can easily collide with a live id and name **a different
/// key**, which would decrypt to a wrong plaintext rather than to an error.
///
/// So the handle is only honoured when the key it names carries the modulus the
/// caller actually initialised with. A collision then declines instead of
/// decrypting, and the cost is one big-endian comparison.
///
/// Retires the `Cipher` decrypt half of
/// `internal/performance/rsa-private-key-op-crt-FIXED-20260817`.
pub fn rsa_cipher_decrypt_by_id(
    id: u64,
    expect_n: &[u8],
    pad: RsaCipherPadding,
    ct: &[u8],
) -> Option<Result<Vec<u8>, RsaCipherError>> {
    let guard = RSA_KEY_STORE.read();
    let key = &guard.as_ref()?.get(&id)?.private_key;
    // The interlock. `BigUint` round-trips through `to_bytes_be`, so comparing
    // the parsed values rather than the raw slices tolerates leading-zero
    // padding differences between the two sources.
    if key.n.cmp(&BigUint::from_bytes_be(expect_n)) != std::cmp::Ordering::Equal {
        return None;
    }
    let n_big = key.n.clone();
    Some(rsa_cipher_decrypt_with(&n_big, pad, ct, |c| {
        rsa_private_op_blinded(key, c)
    }))
}

/// The shared body of both decrypt entry points. Everything about length
/// refusal, representative range, and the collapsed padding-failure surface is
/// identical; only the private-key operation differs, so only that is a
/// parameter. Keeping one body is what stops the two paths from drifting into
/// different exception classes for the same input.
fn rsa_cipher_decrypt_with(
    n_big: &BigUint,
    pad: RsaCipherPadding,
    ct: &[u8],
    private_op: impl FnOnce(&BigUint) -> BigUint,
) -> Result<Vec<u8>, RsaCipherError> {
    let k = (n_big.bit_length() + 7) / 8;
    if k == 0 {
        return Err(RsaCipherError::Key("RSA: invalid (zero) modulus".into()));
    }
    // ONE-SIDED, and the asymmetry is HotSpot's. `RSACipher.doFinal` refuses
    // only `bufOfs > buffer.length`; a ciphertext SHORTER than the modulus is a
    // smaller integer, goes through the modexp, and fails to unpad — measured
    // `BadPaddingException: Padding error in decryption` for a 200-byte input
    // to an RSA-2048 decrypt. The `ct.len() != k` check this replaced refused
    // both directions with one unchecked `IllegalStateException`, so it got the
    // class wrong on both and the side wrong on one.
    if ct.len() > k {
        return Err(RsaCipherError::BlockSize(format!(
            "Data must not be longer than {k} bytes"
        )));
    }
    let c = BigUint::from_bytes_be(ct);
    // `RSACore.parseMsg`: a ciphertext numerically at or above the modulus is
    // not a decryptable representative and SunJCE says so with a checked
    // `BadPaddingException("Message is larger than modulus")`. Reachable only
    // through `NoPadding`, where the caller supplies the integer directly.
    if c.cmp(n_big) != std::cmp::Ordering::Less {
        return Err(RsaCipherError::Padding(
            "Message is larger than modulus".into(),
        ));
    }
    // The blinded private exponentiation the caller chose (VULN(2)): blinding
    // is what removes the message-dependent timing channel an adaptive
    // ciphertext attacker probes, so both forms of `private_op` blind.
    let m = private_op(&c);
    let em = m.to_bytes_be_padded(k);
    // Every padding failure — wrong key, flipped byte, short ciphertext — has
    // already been collapsed to the single opaque `RSA_PADDING_ERROR` string by
    // `rsa_pkcs1_type2_unpad` / `rsa_oaep_unpad` (VULN(1)). Mapping the whole
    // set to one variant preserves that: the CLASS a caller catches is the same
    // for all of them, so widening the exception surface does not reopen the
    // Bleichenbacher/Manger oracle those functions were rewritten to close.
    match pad {
        RsaCipherPadding::Pkcs1 => rsa_pkcs1_type2_unpad(&em),
        _ => rsa_oaep_unpad(pad, &em),
    }
    .map_err(RsaCipherError::Padding)
}

/// Resolve a synthetic key's `crypto_impl` private components `(n, d)`.
pub fn rsa_key_get_priv(id: u64) -> Option<(Vec<u8>, Vec<u8>)> {
    let guard = RSA_KEY_STORE.read();
    guard.as_ref().and_then(|m| m.get(&id)).map(|kp| {
        (
            kp.private_key.n.to_bytes_be(),
            kp.private_key.d.to_bytes_be(),
        )
    })
}

// ---------------------------------------------------------------------------
// RSASSA-PSS signature verification (RFC 8017 §8.1.2 / §9.1.2, EMSA-PSS-VERIFY).
//
// JWA's PS256 / PS384 / PS512 map to RSASSA-PSS with MGF1 over the same hash and
// a salt length equal to the hash length. Keycloak's `JavaAlgorithm` resolves
// them to BouncyCastle's `SHA{256,384,512}withRSAandMGF1`; the empty CratonVM
// provider list can't service the real BC SPI, so we sign and verify natively.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PssHash {
    /// Only reachable through `RSASSA-PSS-params`, whose `hashAlgorithm` and
    /// `maskGenAlgorithm` both DEFAULT to SHA-1 (RFC 4055 §3.1). No signer in
    /// this tree emits it.
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl PssHash {
    /// The digest OIDs `RSASSA-PSS-params` can name, as they appear in a
    /// certificate's `signatureAlgorithm` parameters.
    pub fn from_digest_oid(oid: &[u8]) -> Option<PssHash> {
        // 1.3.14.3.2.26 sha1, 2.16.840.1.101.3.4.2.{1,2,3} sha256/384/512
        match oid {
            [0x2b, 0x0e, 0x03, 0x02, 0x1a] => Some(PssHash::Sha1),
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01] => Some(PssHash::Sha256),
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02] => Some(PssHash::Sha384),
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03] => Some(PssHash::Sha512),
            _ => None,
        }
    }

    pub fn hlen(self) -> usize {
        match self {
            PssHash::Sha1 => 20,
            PssHash::Sha256 => 32,
            PssHash::Sha384 => 48,
            PssHash::Sha512 => 64,
        }
    }

    fn hash(self, data: &[u8]) -> Vec<u8> {
        match self {
            PssHash::Sha1 => {
                use sha1::Digest;
                let mut h = sha1::Sha1::new();
                h.update(data);
                h.finalize().to_vec()
            }
            PssHash::Sha256 => Sha256::digest(data).to_vec(),
            PssHash::Sha384 => Sha384::digest(data).to_vec(),
            PssHash::Sha512 => Sha512::digest(data).to_vec(),
        }
    }
}

/// MGF1 over the PSS digest.
fn pss_mgf1(hash: PssHash, seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + hash.hlen());
    let mut counter: u32 = 0;
    while out.len() < len {
        let mut input = Vec::with_capacity(seed.len() + 4);
        input.extend_from_slice(seed);
        input.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&hash.hash(&input));
        counter = counter.wrapping_add(1);
    }
    out.truncate(len);
    out
}

/// RSASSA-PSS sign with MGF1 over the selected digest and a digest-sized random
/// salt (RFC 8017 §8.1.1 / §9.1). Returns an empty vector when the modulus is
/// too small for the chosen digest or secure OS entropy is unavailable.
pub fn rsa_sign_pss(key: &RsaPrivateKey, hash: PssHash, message: &[u8]) -> Vec<u8> {
    rsa_sign_pss_ex(key, hash, hash, hash.hlen(), message)
}

/// RSASSA-PSS sign with an explicit MGF1 digest and salt length, the
/// counterpart of [`rsa_verify_pss_ex`].
pub fn rsa_sign_pss_ex(
    key: &RsaPrivateKey,
    hash: PssHash,
    mgf_hash: PssHash,
    slen: usize,
    message: &[u8],
) -> Vec<u8> {
    let mod_bits = key.n.bit_length();
    if mod_bits <= 1 {
        return Vec::new();
    }
    let k = (mod_bits + 7) / 8;
    let em_bits = mod_bits - 1;
    let em_len = (em_bits + 7) / 8;
    let hlen = hash.hlen();
    if em_len < hlen + slen + 2 {
        return Vec::new();
    }

    let mut salt = vec![0u8; slen];
    if !os_random_bytes(&mut salt) {
        return Vec::new();
    }
    let m_hash = hash.hash(message);
    let mut m_prime = Vec::with_capacity(8 + hlen + slen);
    m_prime.extend_from_slice(&[0u8; 8]);
    m_prime.extend_from_slice(&m_hash);
    m_prime.extend_from_slice(&salt);
    let h = hash.hash(&m_prime);

    let ps_len = em_len - hlen - slen - 2;
    let mut db = vec![0u8; ps_len];
    db.push(0x01);
    db.extend_from_slice(&salt);
    let db_mask = pss_mgf1(mgf_hash, &h, db.len());
    let mut masked_db: Vec<u8> = db.iter().zip(db_mask.iter()).map(|(a, b)| a ^ b).collect();
    let zero_bits = 8 * em_len - em_bits;
    masked_db[0] &= 0xFFu8 >> zero_bits;

    let mut em = masked_db;
    em.extend_from_slice(&h);
    em.push(0xbc);
    let m = BigUint::from_bytes_be(&em);
    rsa_private_op_blinded(key, &m).to_bytes_be_padded(k)
}

/// RSASSA-PSS verify with MGF1 over the same digest and salt length == hLen
/// (the JWA convention for PS256/PS384/PS512, and what TLS 1.3 mandates for
/// `rsa_pss_*` handshake signatures). Returns `false` for any
/// malformed/invalid signature.
///
/// **X.509 certificates are not on this convention.** `RSASSA-PSS-params`
/// carries its own `saltLength`, defaulting to 20 whatever the digest is
/// (RFC 4055 §3.1), and its own MGF digest — use [`rsa_verify_pss_ex`] with
/// the parsed parameters there. netty's `rsapss-ca-cert.cert` is exactly this
/// case: SHA-256 with a 20-byte salt, which `slen == hlen` cannot verify.
pub fn rsa_verify_pss(n: &[u8], e: &[u8], hash: PssHash, message: &[u8], signature: &[u8]) -> bool {
    rsa_verify_pss_ex(n, e, hash, hash, hash.hlen(), message, signature)
}

/// RSASSA-PSS verify with an explicit MGF1 digest and salt length
/// (RFC 8017 §9.1.2, EMSA-PSS-VERIFY). Returns `false` for any
/// malformed/invalid signature.
pub fn rsa_verify_pss_ex(
    n: &[u8],
    e: &[u8],
    hash: PssHash,
    mgf_hash: PssHash,
    slen: usize,
    message: &[u8],
    signature: &[u8],
) -> bool {
    let n_big = BigUint::from_bytes_be(n);
    let e_big = BigUint::from_bytes_be(e);
    let mod_bits = n_big.bit_length();
    if mod_bits == 0 {
        return false;
    }
    let k = (mod_bits + 7) / 8;
    if signature.len() != k {
        return false;
    }
    let hlen = hash.hlen();

    // RSAVP1: s^e mod n (reject s >= n).
    let s = BigUint::from_bytes_be(signature);
    if s.cmp(&n_big) != std::cmp::Ordering::Less {
        return false;
    }
    let m = s.modpow(&e_big, &n_big);

    // EMSA-PSS-VERIFY (emBits = modBits - 1).
    let em_bits = mod_bits - 1;
    let em_len = (em_bits + 7) / 8;
    if em_len < hlen + slen + 2 {
        return false;
    }
    let em = m.to_bytes_be_padded(em_len);
    if em[em_len - 1] != 0xbc {
        return false;
    }
    let masked_db = &em[..em_len - hlen - 1];
    let h = &em[em_len - hlen - 1..em_len - 1];

    // The leftmost (8*emLen - emBits) bits of the leftmost maskedDB byte must be 0.
    let zero_bits = 8 * em_len - em_bits; // in 1..=8
    if zero_bits < 8 {
        if masked_db[0] & (0xFFu8 << (8 - zero_bits)) != 0 {
            return false;
        }
    } else if masked_db[0] != 0 {
        return false;
    }

    let db_mask = pss_mgf1(mgf_hash, h, em_len - hlen - 1);
    let mut db: Vec<u8> = masked_db
        .iter()
        .zip(db_mask.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    // Clear the leftmost zero_bits bits of db[0].
    if zero_bits == 8 {
        db[0] = 0;
    } else {
        db[0] &= 0xFFu8 >> zero_bits;
    }

    // DB = PS(0x00…) || 0x01 || salt.
    let ps_len = em_len - hlen - slen - 2;
    if db[..ps_len].iter().any(|&b| b != 0) {
        return false;
    }
    if db[ps_len] != 0x01 {
        return false;
    }
    let salt = &db[db.len() - slen..];

    // H' = Hash(0x00*8 || mHash || salt) must equal H.
    let m_hash = hash.hash(message);
    let mut m_prime = Vec::with_capacity(8 + hlen + slen);
    m_prime.extend_from_slice(&[0u8; 8]);
    m_prime.extend_from_slice(&m_hash);
    m_prime.extend_from_slice(salt);
    hash.hash(&m_prime) == h
}

/// RSASSA-PSS verify against a stored key id (real or synthetic). `None` only
/// when the key id is unknown.
pub fn rsa_verify_pss_by_id(
    id: u64,
    hash: PssHash,
    message: &[u8],
    signature: &[u8],
) -> Option<bool> {
    let (n, e) = rsa_key_get_pub(id)?;
    Some(rsa_verify_pss(&n, &e, hash, message, signature))
}

/// [`rsa_verify_pss_by_id`] with an explicit MGF digest and salt length — what
/// a `PSSParameterSpec` supplied through `Signature.setParameter` names.
pub fn rsa_verify_pss_ex_by_id(
    id: u64,
    hash: PssHash,
    mgf_hash: PssHash,
    slen: usize,
    message: &[u8],
    signature: &[u8],
) -> Option<bool> {
    let (n, e) = rsa_key_get_pub(id)?;
    Some(rsa_verify_pss_ex(
        &n, &e, hash, mgf_hash, slen, message, signature,
    ))
}

/// [`rsa_sign_pss_by_id`] with an explicit MGF digest and salt length.
pub fn rsa_sign_pss_ex_by_id(
    id: u64,
    hash: PssHash,
    mgf_hash: PssHash,
    slen: usize,
    message: &[u8],
) -> Option<Vec<u8>> {
    let guard = RSA_KEY_STORE.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&id))
        .map(|kp| rsa_sign_pss_ex(&kp.private_key, hash, mgf_hash, slen, message))
}

/// RSASSA-PSS sign against a stored key id. `None` only when the key id is
/// unknown; an empty signature signals an invalid modulus or unavailable entropy.
pub fn rsa_sign_pss_by_id(id: u64, hash: PssHash, message: &[u8]) -> Option<Vec<u8>> {
    let guard = RSA_KEY_STORE.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&id))
        .map(|kp| rsa_sign_pss(&kp.private_key, hash, message))
}

// ---------------------------------------------------------------------------
// ECDSA P-256 implementation
// ---------------------------------------------------------------------------

/// 256-bit field element represented as 4 x u64 limbs (little-endian).
#[derive(Clone, Copy, Debug)]
pub struct FieldElement256 {
    pub limbs: [u64; 4],
}

// P-256 prime: p = 2^256 - 2^224 + 2^192 + 2^96 - 1
const P256_P: FieldElement256 = FieldElement256 {
    limbs: [
        0xFFFFFFFFFFFFFFFF,
        0x00000000FFFFFFFF,
        0x0000000000000000,
        0xFFFFFFFF00000001,
    ],
};

// P-256 order n
const P256_N: FieldElement256 = FieldElement256 {
    limbs: [
        0xF3B9CAC2FC632551,
        0xBCE6FAADA7179E84,
        0xFFFFFFFFFFFFFFFF,
        0xFFFFFFFF00000000,
    ],
};

// P-256 parameter b
const P256_B: FieldElement256 = FieldElement256 {
    limbs: [
        0x3BCE3C3E27D2604B,
        0x651D06B0CC53B0F6,
        0xB3EBBD55769886BC,
        0x5AC635D8AA3A93E7,
    ],
};

// Generator point G
const P256_GX: FieldElement256 = FieldElement256 {
    limbs: [
        0xF4A13945D898C296,
        0x77037D812DEB33A0,
        0xF8BCE6E563A440F2,
        0x6B17D1F2E12C4247,
    ],
};
const P256_GY: FieldElement256 = FieldElement256 {
    limbs: [
        0xCBB6406837BF51F5,
        0x2BCE33576B315ECE,
        0x8EE7EB4A7C0F9E16,
        0x4FE342E2FE1A7F9B,
    ],
};

impl FieldElement256 {
    pub const ZERO: FieldElement256 = FieldElement256 {
        limbs: [0, 0, 0, 0],
    };
    pub const ONE: FieldElement256 = FieldElement256 {
        limbs: [1, 0, 0, 0],
    };

    pub fn from_bytes_be(bytes: &[u8]) -> Self {
        let mut padded = [0u8; 32];
        let start = 32usize.saturating_sub(bytes.len());
        padded[start..].copy_from_slice(&bytes[..bytes.len().min(32)]);
        let mut limbs = [0u64; 4];
        limbs[3] = u64::from_be_bytes(padded[0..8].try_into().unwrap());
        limbs[2] = u64::from_be_bytes(padded[8..16].try_into().unwrap());
        limbs[1] = u64::from_be_bytes(padded[16..24].try_into().unwrap());
        limbs[0] = u64::from_be_bytes(padded[24..32].try_into().unwrap());
        FieldElement256 { limbs }
    }

    pub fn to_bytes_be(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[0..8].copy_from_slice(&self.limbs[3].to_be_bytes());
        out[8..16].copy_from_slice(&self.limbs[2].to_be_bytes());
        out[16..24].copy_from_slice(&self.limbs[1].to_be_bytes());
        out[24..32].copy_from_slice(&self.limbs[0].to_be_bytes());
        out
    }

    pub fn is_zero(&self) -> bool {
        self.limbs == [0, 0, 0, 0]
    }

    /// Add two 256-bit field elements mod p.
    pub fn add_mod(a: &Self, b: &Self, p: &Self) -> Self {
        let (sum, carry) = add_u256(&a.limbs, &b.limbs);
        let mut result = FieldElement256 { limbs: sum };
        if carry || cmp_u256(&sum, &p.limbs) != std::cmp::Ordering::Less {
            result.limbs = sub_u256(&result.limbs, &p.limbs).0;
        }
        result
    }

    /// Subtract two 256-bit field elements mod p.
    pub fn sub_mod(a: &Self, b: &Self, p: &Self) -> Self {
        let (diff, borrow) = sub_u256(&a.limbs, &b.limbs);
        if borrow {
            let (sum, _) = add_u256(&diff, &p.limbs);
            FieldElement256 { limbs: sum }
        } else {
            FieldElement256 { limbs: diff }
        }
    }

    /// Multiply two 256-bit field elements mod p.
    pub fn mul_mod(a: &Self, b: &Self, p: &Self) -> Self {
        let product = mul_u256(&a.limbs, &b.limbs); // 512-bit
        mod_u512_by_u256(&product, &p.limbs)
    }

    /// Modular inverse using Fermat's little theorem: a^(p-2) mod p.
    pub fn inv_mod(a: &Self, p: &Self) -> Self {
        let p_minus_2 = {
            let (r, _) = sub_u256(&p.limbs, &[2, 0, 0, 0]);
            FieldElement256 { limbs: r }
        };
        Self::pow_mod(a, &p_minus_2, p)
    }

    /// Modular exponentiation via a Montgomery ladder.
    ///
    /// nb-crypto-impl VULN(2): the ladder gives a *uniform operation sequence*
    /// (exactly one multiply + one square per bit), but the per-bit register
    /// assignment is still selected by a secret-dependent `if`, and the
    /// underlying `mul_mod` is not formally constant-time. Treat as
    /// timing-hardened-but-not-guaranteed, NOT a certified side-channel-resistant
    /// primitive. (`pow_mod` is used here only for field inversion / square
    /// roots over the public prime `p`, not over secret exponents.)
    pub fn pow_mod(base: &Self, exp: &Self, p: &Self) -> Self {
        let mut r0 = Self::ONE;
        let mut r1 = *base;
        for i in (0..256).rev() {
            let limb = exp.limbs[i / 64];
            if (limb >> (i % 64)) & 1 == 1 {
                r0 = Self::mul_mod(&r0, &r1, p);
                r1 = Self::mul_mod(&r1, &r1, p);
            } else {
                r1 = Self::mul_mod(&r0, &r1, p);
                r0 = Self::mul_mod(&r0, &r0, p);
            }
        }
        r0
    }

    /// Convert a BigUint to FieldElement256.
    pub fn from_biguint(n: &BigUint) -> Self {
        Self::from_bytes_be(&n.to_bytes_be())
    }

    /// Convert to BigUint.
    pub fn to_biguint(&self) -> BigUint {
        BigUint::from_bytes_be(&self.to_bytes_be())
    }
}

// 256-bit arithmetic helpers (u64 limbs, little-endian)

fn add_u256(a: &[u64; 4], b: &[u64; 4]) -> ([u64; 4], bool) {
    let mut result = [0u64; 4];
    let mut carry = 0u64;
    for i in 0..4 {
        let (s1, c1) = a[i].overflowing_add(b[i]);
        let (s2, c2) = s1.overflowing_add(carry);
        result[i] = s2;
        carry = (c1 as u64) + (c2 as u64);
    }
    (result, carry > 0)
}

fn sub_u256(a: &[u64; 4], b: &[u64; 4]) -> ([u64; 4], bool) {
    let mut result = [0u64; 4];
    let mut borrow = 0u64;
    for i in 0..4 {
        let (s1, c1) = a[i].overflowing_sub(b[i]);
        let (s2, c2) = s1.overflowing_sub(borrow);
        result[i] = s2;
        borrow = (c1 as u64) + (c2 as u64);
    }
    (result, borrow > 0)
}

fn cmp_u256(a: &[u64; 4], b: &[u64; 4]) -> std::cmp::Ordering {
    for i in (0..4).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    std::cmp::Ordering::Equal
}

/// Multiply two 256-bit numbers, producing 512 bits.
fn mul_u256(a: &[u64; 4], b: &[u64; 4]) -> [u64; 8] {
    let mut result = [0u64; 8];
    for i in 0..4 {
        let mut carry = 0u128;
        for j in 0..4 {
            let prod = a[i] as u128 * b[j] as u128 + result[i + j] as u128 + carry;
            result[i + j] = prod as u64;
            carry = prod >> 64;
        }
        result[i + 4] = carry as u64;
    }
    result
}

/// Reduce a 512-bit number mod a 256-bit modulus using BigUint division.
fn mod_u512_by_u256(a: &[u64; 8], m: &[u64; 4]) -> FieldElement256 {
    // Convert to BigUint for modular reduction
    let mut a_bytes = [0u8; 64];
    for i in 0..8 {
        let b = a[7 - i].to_be_bytes();
        a_bytes[i * 8..(i + 1) * 8].copy_from_slice(&b);
    }
    let mut m_bytes = [0u8; 32];
    for i in 0..4 {
        let b = m[3 - i].to_be_bytes();
        m_bytes[i * 8..(i + 1) * 8].copy_from_slice(&b);
    }
    let a_big = BigUint::from_bytes_be(&a_bytes);
    let m_big = BigUint::from_bytes_be(&m_bytes);
    let rem = a_big.modulo(&m_big);
    FieldElement256::from_bytes_be(&rem.to_bytes_be())
}

/// Point on the P-256 curve (affine coordinates, or point at infinity).
#[derive(Clone, Copy, Debug)]
pub struct EcPoint {
    pub x: FieldElement256,
    pub y: FieldElement256,
    pub infinity: bool,
}

impl EcPoint {
    pub fn infinity() -> Self {
        EcPoint {
            x: FieldElement256::ZERO,
            y: FieldElement256::ZERO,
            infinity: true,
        }
    }

    pub fn new(x: FieldElement256, y: FieldElement256) -> Self {
        EcPoint {
            x,
            y,
            infinity: false,
        }
    }

    /// Point addition on P-256 (affine).
    pub fn add(p1: &EcPoint, p2: &EcPoint) -> EcPoint {
        if p1.infinity {
            return *p2;
        }
        if p2.infinity {
            return *p1;
        }

        let p = &P256_P;

        if cmp_u256(&p1.x.limbs, &p2.x.limbs) == std::cmp::Ordering::Equal {
            if cmp_u256(&p1.y.limbs, &p2.y.limbs) == std::cmp::Ordering::Equal {
                return Self::double(p1);
            } else {
                return Self::infinity();
            }
        }

        // lambda = (y2 - y1) / (x2 - x1)
        let dy = FieldElement256::sub_mod(&p2.y, &p1.y, p);
        let dx = FieldElement256::sub_mod(&p2.x, &p1.x, p);
        let dx_inv = FieldElement256::inv_mod(&dx, p);
        let lambda = FieldElement256::mul_mod(&dy, &dx_inv, p);

        // x3 = lambda^2 - x1 - x2
        let l2 = FieldElement256::mul_mod(&lambda, &lambda, p);
        let x3 = FieldElement256::sub_mod(&FieldElement256::sub_mod(&l2, &p1.x, p), &p2.x, p);

        // y3 = lambda * (x1 - x3) - y1
        let dx13 = FieldElement256::sub_mod(&p1.x, &x3, p);
        let y3 = FieldElement256::sub_mod(&FieldElement256::mul_mod(&lambda, &dx13, p), &p1.y, p);

        EcPoint::new(x3, y3)
    }

    /// Point doubling on P-256.
    pub fn double(p1: &EcPoint) -> EcPoint {
        if p1.infinity || p1.y.is_zero() {
            return Self::infinity();
        }

        let p = &P256_P;
        let three = FieldElement256 {
            limbs: [3, 0, 0, 0],
        };
        let two = FieldElement256 {
            limbs: [2, 0, 0, 0],
        };

        // lambda = (3*x1^2 + a) / (2*y1), where a = -3 for P-256
        let x1_sq = FieldElement256::mul_mod(&p1.x, &p1.x, p);
        let three_x1_sq = FieldElement256::mul_mod(&three, &x1_sq, p);
        // a = p - 3 (which is -3 mod p)
        let a = FieldElement256::sub_mod(p, &three, p);
        let numerator = FieldElement256::add_mod(&three_x1_sq, &a, p);
        let denominator = FieldElement256::mul_mod(&two, &p1.y, p);
        let denom_inv = FieldElement256::inv_mod(&denominator, p);
        let lambda = FieldElement256::mul_mod(&numerator, &denom_inv, p);

        // x3 = lambda^2 - 2*x1
        let l2 = FieldElement256::mul_mod(&lambda, &lambda, p);
        let two_x1 = FieldElement256::mul_mod(&two, &p1.x, p);
        let x3 = FieldElement256::sub_mod(&l2, &two_x1, p);

        // y3 = lambda * (x1 - x3) - y1
        let dx = FieldElement256::sub_mod(&p1.x, &x3, p);
        let y3 = FieldElement256::sub_mod(&FieldElement256::mul_mod(&lambda, &dx, p), &p1.y, p);

        EcPoint::new(x3, y3)
    }

    /// Scalar multiplication via a Montgomery ladder.
    ///
    /// nb-crypto-impl VULN(2): the ladder keeps the operation sequence uniform
    /// (one point-add + one point-double per scalar bit), but the register
    /// assignment is chosen by a secret-dependent `if`, and `EcPoint::add` /
    /// `double` / `mul_mod` are not formally constant-time (e.g. the `infinity`
    /// early-out and modular reduction branch). This is timing-hardened in
    /// structure but NOT a certified side-channel-resistant scalar multiply; for
    /// adversary-exposed key operations prefer the audited `p256` crate.
    pub fn scalar_mul(point: &EcPoint, scalar: &FieldElement256) -> EcPoint {
        let mut r0 = EcPoint::infinity();
        let mut r1 = *point;
        for i in (0..256).rev() {
            let limb = scalar.limbs[i / 64];
            if (limb >> (i % 64)) & 1 == 1 {
                r0 = EcPoint::add(&r0, &r1);
                r1 = EcPoint::double(&r1);
            } else {
                r1 = EcPoint::add(&r0, &r1);
                r0 = EcPoint::double(&r0);
            }
        }
        r0
    }
}

pub struct EcdsaPublicKey {
    pub point: EcPoint,
}

pub struct EcdsaPrivateKey {
    pub d: FieldElement256,
}

impl Drop for EcdsaPrivateKey {
    fn drop(&mut self) {
        // Zeroize private key material
        self.d = FieldElement256::ZERO;
    }
}

pub struct Ecdsa;

impl Ecdsa {
    pub fn generator() -> EcPoint {
        EcPoint::new(P256_GX, P256_GY)
    }

    /// Generate a P-256 key pair.
    pub fn generate_keypair() -> (EcdsaPublicKey, EcdsaPrivateKey) {
        let mut rng = SecureRandom::new();
        let g = Self::generator();
        loop {
            let mut d_bytes = [0u8; 32];
            rng.next_bytes(&mut d_bytes);
            let d = FieldElement256::from_bytes_be(&d_bytes);
            // Ensure d is in [1, n-1]
            if d.is_zero() {
                continue;
            }
            if cmp_u256(&d.limbs, &P256_N.limbs) != std::cmp::Ordering::Less {
                continue;
            }
            let q = EcPoint::scalar_mul(&g, &d);
            if q.infinity {
                continue;
            }
            return (EcdsaPublicKey { point: q }, EcdsaPrivateKey { d });
        }
    }

    /// ECDSA sign with SHA-256 hash (algorithm: SHA256withECDSA / NONEwithECDSA-32).
    ///
    /// Uses SHA-256 (32 bytes) as the message digest — which is what
    /// `Signature.getInstance("SHA256withECDSA")` produces and what every
    /// real-world TLS / JWT / X.509-on-EC stack consumes.
    ///
    /// nb-crypto-impl VULN(2): the prior doc claimed an "RFC 6979" construction.
    /// It is NOT — see `sign_with_digest`, which draws a fresh random per-message
    /// nonce. Both signing entry points share the same non-deterministic nonce.
    pub fn sign_sha256(key: &EcdsaPrivateKey, message: &[u8]) -> Vec<u8> {
        let hash_full = Sha256::digest(message);
        Self::sign_with_digest(key, &hash_full)
    }

    /// ECDSA verify with SHA-256.
    pub fn verify_sha256(key: &EcdsaPublicKey, message: &[u8], signature: &[u8]) -> bool {
        let hash_full = Sha256::digest(message);
        Self::verify_with_digest(key, &hash_full, signature)
    }

    /// Internal: ECDSA sign on an already-hashed digest (truncated/extended
    /// to the curve order's bit length).  Shared by `sign_sha256` /
    /// `sign_sha384`.
    ///
    /// nb-crypto-impl VULN(2): the per-signature nonce `k` is drawn FRESH from
    /// the OS CSPRNG (`SecureRandom`) on every call — this is NOT the RFC 6979
    /// deterministic-nonce construction, so two signatures over the same digest
    /// differ. That is safe *as long as* the CSPRNG is sound and never repeats a
    /// `k` for a given key (a repeated or biased `k` trivially recovers the
    /// private key). The point-multiply (`EcPoint::scalar_mul`) and the modular
    /// inverse (`k_inv`) also run in variable time over the non-constant-time
    /// `BigUint`/`FieldElement256` cores, so this path is NOT hardened against a
    /// timing/power adversary. For adversary-exposed signing prefer the audited
    /// `p256`/`ecdsa` crates (RFC 6979 + constant-time scalar mul). Retained as a
    /// native fallback only because that routing is a cross-crate change outside
    /// this module's scope.
    pub fn sign_with_digest(key: &EcdsaPrivateKey, digest: &[u8]) -> Vec<u8> {
        let mut z_bytes = [0u8; 32];
        let dn = digest.len().min(32);
        z_bytes[..dn].copy_from_slice(&digest[..dn]);
        let z = FieldElement256::from_bytes_be(&z_bytes);
        let n = &P256_N;
        let n_big = n.to_biguint();
        let g = Self::generator();
        let mut rng = SecureRandom::new();

        loop {
            let mut k_bytes = [0u8; 32];
            rng.next_bytes(&mut k_bytes);
            let k = FieldElement256::from_bytes_be(&k_bytes);
            if k.is_zero() {
                continue;
            }
            if cmp_u256(&k.limbs, &n.limbs) != std::cmp::Ordering::Less {
                continue;
            }

            // nb-crypto-impl VULN(2) — ECDSA variable-time residual (UNFIXED):
            // unlike the RSA private path above (which is now base-blinded), this
            // EC scalar path is NOT blinded. The secret-dependent operations that
            // remain variable-time are, precisely:
            //   1. `EcPoint::scalar_mul(&g, &k)` — the double-and-add ladder
            //      branches on each bit of the per-signature nonce `k`, so its
            //      timing/power profile leaks `k`'s bit pattern. Recovering even a
            //      few bits of `k` across several signatures breaks the key
            //      (lattice/HNP attack), because `d = (s*k - z) / r mod n`.
            //   2. `k_big.modinv(&n_big)` — the extended-GCD inversion of the
            //      secret nonce runs in input-dependent time (its quotient
            //      sequence depends on `k`), a second `k`-dependent channel.
            //   3. `r_big.mul(&d_big)` — the limb multiply by the long-term secret
            //      `d` is not constant-time, leaking `d` directly.
            // Implementing scalar blinding (k' = k + e*n, and projective/Montgomery
            // ladder scalar_mul) here is a larger rewrite of the EC core than the
            // RSA `r^e` blinding; per scope it is documented rather than fixed.
            // For an adversary-exposed EC signer prefer the audited `p256`/`ecdsa`
            // crates (RFC 6979 nonce + constant-time scalar mul).
            let r_point = EcPoint::scalar_mul(&g, &k);
            if r_point.infinity {
                continue;
            }
            let r_big = r_point.x.to_biguint().modulo(&n_big);
            if r_big.is_zero() {
                continue;
            }

            let k_big = k.to_biguint();
            let z_big = z.to_biguint();
            let d_big = key.d.to_biguint();
            let k_inv = match k_big.modinv(&n_big) {
                Some(v) => v,
                None => continue,
            };
            let rd = r_big.mul(&d_big).modulo(&n_big);
            let zrd = z_big.add(&rd).modulo(&n_big);
            let s_big = k_inv.mul(&zrd).modulo(&n_big);
            if s_big.is_zero() {
                continue;
            }

            let r_bytes = r_big.to_bytes_be();
            let s_bytes = s_big.to_bytes_be();
            return der_encode_ecdsa_signature(&r_bytes, &s_bytes);
        }
    }

    /// Internal: ECDSA verify on an already-hashed digest.
    pub fn verify_with_digest(key: &EcdsaPublicKey, digest: &[u8], signature: &[u8]) -> bool {
        let (r_bytes, s_bytes) = match der_decode_ecdsa_signature(signature) {
            Some(v) => v,
            None => return false,
        };
        let n_big = P256_N.to_biguint();
        let r_big = BigUint::from_bytes_be(&r_bytes);
        let s_big = BigUint::from_bytes_be(&s_bytes);

        if r_big.is_zero() || r_big.cmp(&n_big) != std::cmp::Ordering::Less {
            return false;
        }
        if s_big.is_zero() || s_big.cmp(&n_big) != std::cmp::Ordering::Less {
            return false;
        }

        let mut z_bytes = [0u8; 32];
        let dn = digest.len().min(32);
        z_bytes[..dn].copy_from_slice(&digest[..dn]);
        let z_big = BigUint::from_bytes_be(&z_bytes).modulo(&n_big);

        let s_inv = match s_big.modinv(&n_big) {
            Some(v) => v,
            None => return false,
        };
        let u1 = z_big.mul(&s_inv).modulo(&n_big);
        let u2 = r_big.mul(&s_inv).modulo(&n_big);

        let g = Self::generator();
        let u1_fe = FieldElement256::from_biguint(&u1);
        let u2_fe = FieldElement256::from_biguint(&u2);
        let p1 = EcPoint::scalar_mul(&g, &u1_fe);
        let p2 = EcPoint::scalar_mul(&key.point, &u2_fe);
        let r_point = EcPoint::add(&p1, &p2);

        if r_point.infinity {
            return false;
        }
        let rx = r_point.x.to_biguint().modulo(&n_big);
        rx.cmp(&r_big) == std::cmp::Ordering::Equal
    }

    /// ECDSA sign with SHA-384 hash (algorithm: SHA384withECDSA).
    pub fn sign_sha384(key: &EcdsaPrivateKey, message: &[u8]) -> Vec<u8> {
        let hash_full = Sha384::digest(message);
        // Truncate hash to 32 bytes (order bit length)
        let z = FieldElement256::from_bytes_be(&hash_full[..32]);
        let n = &P256_N;
        let n_big = n.to_biguint();
        let g = Self::generator();
        let mut rng = SecureRandom::new();

        loop {
            let mut k_bytes = [0u8; 32];
            rng.next_bytes(&mut k_bytes);
            let k = FieldElement256::from_bytes_be(&k_bytes);
            if k.is_zero() {
                continue;
            }
            if cmp_u256(&k.limbs, &n.limbs) != std::cmp::Ordering::Less {
                continue;
            }

            let r_point = EcPoint::scalar_mul(&g, &k);
            if r_point.infinity {
                continue;
            }
            let r_big = r_point.x.to_biguint().modulo(&n_big);
            if r_big.is_zero() {
                continue;
            }

            // s = k^-1 * (z + r * d) mod n
            let k_big = k.to_biguint();
            let z_big = z.to_biguint();
            let d_big = key.d.to_biguint();
            let k_inv = match k_big.modinv(&n_big) {
                Some(v) => v,
                None => continue,
            };
            let rd = r_big.mul(&d_big).modulo(&n_big);
            let zrd = z_big.add(&rd).modulo(&n_big);
            let s_big = k_inv.mul(&zrd).modulo(&n_big);
            if s_big.is_zero() {
                continue;
            }

            // DER encode (r, s)
            let r_bytes = r_big.to_bytes_be();
            let s_bytes = s_big.to_bytes_be();
            return der_encode_ecdsa_signature(&r_bytes, &s_bytes);
        }
    }

    /// ECDSA verify with SHA-384.
    pub fn verify_sha384(key: &EcdsaPublicKey, message: &[u8], signature: &[u8]) -> bool {
        let (r_bytes, s_bytes) = match der_decode_ecdsa_signature(signature) {
            Some(v) => v,
            None => return false,
        };
        let n_big = P256_N.to_biguint();
        let r_big = BigUint::from_bytes_be(&r_bytes);
        let s_big = BigUint::from_bytes_be(&s_bytes);

        if r_big.is_zero() || r_big.cmp(&n_big) != std::cmp::Ordering::Less {
            return false;
        }
        if s_big.is_zero() || s_big.cmp(&n_big) != std::cmp::Ordering::Less {
            return false;
        }

        let hash_full = Sha384::digest(message);
        let z_big = BigUint::from_bytes_be(&hash_full[..32]).modulo(&n_big);

        let s_inv = match s_big.modinv(&n_big) {
            Some(v) => v,
            None => return false,
        };
        let u1 = z_big.mul(&s_inv).modulo(&n_big);
        let u2 = r_big.mul(&s_inv).modulo(&n_big);

        let g = Self::generator();
        let u1_fe = FieldElement256::from_biguint(&u1);
        let u2_fe = FieldElement256::from_biguint(&u2);
        let p1 = EcPoint::scalar_mul(&g, &u1_fe);
        let p2 = EcPoint::scalar_mul(&key.point, &u2_fe);
        let r_point = EcPoint::add(&p1, &p2);

        if r_point.infinity {
            return false;
        }
        let rx = r_point.x.to_biguint().modulo(&n_big);
        rx.cmp(&r_big) == std::cmp::Ordering::Equal
    }

    /// Serialize public key to uncompressed form (0x04 || x || y).
    pub fn public_key_to_bytes(key: &EcdsaPublicKey) -> Vec<u8> {
        let mut out = Vec::with_capacity(65);
        out.push(0x04);
        out.extend_from_slice(&key.point.x.to_bytes_be());
        out.extend_from_slice(&key.point.y.to_bytes_be());
        out
    }

    /// Parse uncompressed public key (0x04 || x || y).
    pub fn public_key_from_bytes(bytes: &[u8]) -> Option<EcdsaPublicKey> {
        if bytes.len() != 65 || bytes[0] != 0x04 {
            return None;
        }
        let x = FieldElement256::from_bytes_be(&bytes[1..33]);
        let y = FieldElement256::from_bytes_be(&bytes[33..65]);
        Some(EcdsaPublicKey {
            point: EcPoint::new(x, y),
        })
    }

    /// Serialize private key (raw 32-byte scalar).
    pub fn private_key_to_bytes(key: &EcdsaPrivateKey) -> Vec<u8> {
        key.d.to_bytes_be().to_vec()
    }

    /// Serialize public key to DER SubjectPublicKeyInfo.
    pub fn public_key_to_der(key: &EcdsaPublicKey) -> Vec<u8> {
        // AlgorithmIdentifier: OID 1.2.840.10045.2.1 (ecPublicKey) + OID 1.2.840.10045.3.1.7 (P-256)
        let alg_oid: &[u8] = &[
            0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, // ecPublicKey OID
            0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, // P-256 OID
        ];
        let alg_id = der_encode_sequence(alg_oid);
        let pk_bytes = Self::public_key_to_bytes(key);
        let mut bit_string = vec![0x03];
        let bs_len = pk_bytes.len() + 1;
        bit_string.extend_from_slice(&der_encode_length(bs_len));
        bit_string.push(0x00); // unused bits
        bit_string.extend_from_slice(&pk_bytes);

        let mut spki = Vec::new();
        spki.extend_from_slice(&alg_id);
        spki.extend_from_slice(&bit_string);
        der_encode_sequence(&spki)
    }
}

fn der_encode_ecdsa_signature(r: &[u8], s: &[u8]) -> Vec<u8> {
    let r_int = der_encode_integer(r);
    let s_int = der_encode_integer(s);
    let mut inner = Vec::new();
    inner.extend_from_slice(&r_int);
    inner.extend_from_slice(&s_int);
    der_encode_sequence(&inner)
}

fn der_decode_ecdsa_signature(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if data.len() < 6 || data[0] != 0x30 {
        return None;
    }
    let (_, content) = der_read_tag_length(data)?;
    let (r, rest) = der_read_integer(content)?;
    let (s, _) = der_read_integer(rest)?;
    Some((r, s))
}

// ---------------------------------------------------------------------------
// G59 — DER / ASN.1 helpers and X.509 certificate parser
// ---------------------------------------------------------------------------

pub fn der_encode_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len < 0x100 {
        vec![0x81, len as u8]
    } else if len < 0x10000 {
        vec![0x82, (len >> 8) as u8, len as u8]
    } else {
        vec![0x83, (len >> 16) as u8, (len >> 8) as u8, len as u8]
    }
}

pub fn der_encode_sequence(content: &[u8]) -> Vec<u8> {
    let mut out = vec![0x30];
    out.extend_from_slice(&der_encode_length(content.len()));
    out.extend_from_slice(content);
    out
}

pub fn der_encode_integer(value: &[u8]) -> Vec<u8> {
    let mut out = vec![0x02];
    // If the high bit is set, prepend a zero byte
    if !value.is_empty() && value[0] & 0x80 != 0 {
        out.extend_from_slice(&der_encode_length(value.len() + 1));
        out.push(0x00);
    } else {
        // Strip leading zeros (keep at least one byte)
        let mut start = 0;
        while start + 1 < value.len() && value[start] == 0 {
            start += 1;
        }
        let trimmed = &value[start..];
        out.extend_from_slice(&der_encode_length(trimmed.len()));
        out.extend_from_slice(trimmed);
        return out;
    }
    out.extend_from_slice(value);
    out
}

/// Read a DER tag and length, return (total header+content length consumed, content slice).
fn der_read_tag_length(data: &[u8]) -> Option<(usize, &[u8])> {
    if data.len() < 2 {
        return None;
    }
    let _tag = data[0];
    let (len, hdr_size) = der_read_length(&data[1..])?;
    // `1 + hdr_size` cannot overflow (hdr_size <= 9), but the content end
    // `total_hdr + len` can wrap when `len` is large — use checked_add and
    // bound against the actual buffer so a forged length never produces an
    // inverted/out-of-range slice (panic / DoS on a malicious certificate).
    let total_hdr = 1 + hdr_size;
    let end = total_hdr.checked_add(len)?;
    if data.len() < end {
        return None;
    }
    Some((end, &data[total_hdr..end]))
}

fn der_read_length(data: &[u8]) -> Option<(usize, usize)> {
    if data.is_empty() {
        return None;
    }
    if data[0] < 0x80 {
        Some((data[0] as usize, 1))
    } else {
        let num_bytes = (data[0] & 0x7f) as usize;
        if num_bytes == 0 || data.len() < 1 + num_bytes {
            return None;
        }
        // A length encoded in more bytes than a `usize` can hold would wrap
        // the accumulator below to a small value that then slips past the
        // caller's bounds check, so reject an over-wide width outright. With
        // `num_bytes <= size_of::<usize>()` the shift/or accumulation fits
        // exactly and cannot overflow; any oversized-but-in-range `len` is
        // then caught by the caller's `checked_add` + buffer-length guard.
        if num_bytes > core::mem::size_of::<usize>() {
            return None;
        }
        let mut len = 0usize;
        for i in 0..num_bytes {
            len = (len << 8) | data[1 + i] as usize;
        }
        Some((len, 1 + num_bytes))
    }
}

fn der_read_integer(data: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    if data.is_empty() || data[0] != 0x02 {
        return None;
    }
    let (len, hdr_size) = der_read_length(&data[1..])?;
    let start = 1 + hdr_size;
    let end = start.checked_add(len)?;
    if data.len() < end {
        return None;
    }
    let mut bytes = data[start..end].to_vec();
    // Strip leading zero used for sign
    while bytes.len() > 1 && bytes[0] == 0 {
        bytes.remove(0);
    }
    Some((bytes, &data[end..]))
}

/// Parsed X.509 certificate.
#[derive(Clone, Debug)]
pub struct X509Cert {
    pub version: u8,
    pub serial_number: Vec<u8>,
    pub sig_algorithm: String,
    pub issuer_raw: Vec<u8>,
    pub issuer_cn: String,
    pub subject_raw: Vec<u8>,
    pub subject_cn: String,
    pub not_before: i64, // seconds since epoch
    pub not_after: i64,
    pub public_key_bytes: Vec<u8>,
    pub public_key_algorithm: String,
    pub signature_bytes: Vec<u8>,
    pub tbs_bytes: Vec<u8>,
    pub encoded: Vec<u8>,
}

impl X509Cert {
    /// Parse an X.509 certificate from DER-encoded bytes.
    pub fn parse_der(data: &[u8]) -> Result<Self, CryptoError> {
        if data.len() < 10 || data[0] != 0x30 {
            return Err(CryptoError::UnsupportedAlgorithm(
                "not a valid DER certificate".into(),
            ));
        }
        let (_, cert_content) = der_read_tag_length(data).ok_or_else(|| {
            CryptoError::UnsupportedAlgorithm("invalid certificate structure".into())
        })?;

        // TBSCertificate is the first SEQUENCE in the certificate content
        let (tbs_total_len, tbs_content) = der_read_tag_length(cert_content)
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm("missing TBSCertificate".into()))?;
        let tbs_bytes = cert_content[..tbs_total_len].to_vec();
        let rest_after_tbs = &cert_content[tbs_total_len..];

        // Parse TBS fields
        let mut pos = tbs_content;

        // Version (optional, explicit tag [0])
        let version = if !pos.is_empty() && pos[0] == 0xa0 {
            let (vlen, v_content) = der_read_tag_length(pos)
                .ok_or_else(|| CryptoError::UnsupportedAlgorithm("bad version tag".into()))?;
            let ver = if v_content.len() >= 3 && v_content[0] == 0x02 {
                v_content[2]
            } else {
                0
            };
            pos = &pos[vlen..];
            ver + 1 // X.509 version is 0-indexed in DER
        } else {
            1
        };

        // Serial number
        let (serial, rest) = der_read_integer(pos)
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm("bad serial number".into()))?;
        pos = rest;

        // Signature algorithm (SEQUENCE with OID)
        let (sig_alg_len, sig_alg_content) = der_read_tag_length(pos)
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm("bad sig algorithm".into()))?;
        let sig_algorithm = oid_to_sig_name(sig_alg_content);
        pos = &pos[sig_alg_len..];

        // Issuer (SEQUENCE of SETs of AttributeTypeAndValue)
        let (issuer_len, issuer_content) = der_read_tag_length(pos)
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm("bad issuer".into()))?;
        let issuer_raw = pos[..issuer_len].to_vec();
        let issuer_cn = extract_cn(issuer_content);
        pos = &pos[issuer_len..];

        // Validity (SEQUENCE of two times)
        let (validity_len, validity_content) = der_read_tag_length(pos)
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm("bad validity".into()))?;
        let (not_before, not_after) = parse_validity(validity_content);
        pos = &pos[validity_len..];

        // Subject
        let (subject_len, subject_content) = der_read_tag_length(pos)
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm("bad subject".into()))?;
        let subject_raw = pos[..subject_len].to_vec();
        let subject_cn = extract_cn(subject_content);
        pos = &pos[subject_len..];

        // SubjectPublicKeyInfo
        let (spki_len, _spki_content) = der_read_tag_length(pos)
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm("bad SPKI".into()))?;
        let public_key_bytes = pos[..spki_len].to_vec();
        let public_key_algorithm = detect_pk_algorithm(&public_key_bytes);

        // Signature algorithm (second copy, in outer certificate)
        // Skip to signature value
        let (outer_sig_alg_len, _) = der_read_tag_length(rest_after_tbs)
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm("bad outer sig alg".into()))?;
        let rest3 = &rest_after_tbs[outer_sig_alg_len..];

        // Signature value (BIT STRING)
        let signature_bytes = if !rest3.is_empty() && rest3[0] == 0x03 {
            let (_, bs_content) = der_read_tag_length(rest3)
                .ok_or_else(|| CryptoError::UnsupportedAlgorithm("bad signature".into()))?;
            if bs_content.is_empty() {
                Vec::new()
            } else {
                bs_content[1..].to_vec()
            } // skip unused-bits byte
        } else {
            Vec::new()
        };

        Ok(X509Cert {
            version,
            serial_number: serial,
            sig_algorithm,
            issuer_raw,
            issuer_cn,
            subject_raw,
            subject_cn,
            not_before,
            not_after,
            public_key_bytes,
            public_key_algorithm,
            signature_bytes,
            tbs_bytes,
            encoded: data.to_vec(),
        })
    }

    /// Verify the certificate signature against an issuer's public key (DER SPKI).
    pub fn verify_signature(&self, issuer_spki: &[u8]) -> bool {
        match self.sig_algorithm.as_str() {
            "SHA256withRSA" => {
                if let Some(pub_key) = parse_rsa_public_key(issuer_spki) {
                    Rsa::verify_sha256(&pub_key, &self.tbs_bytes, &self.signature_bytes)
                } else {
                    false
                }
            }
            "SHA384withECDSA" => {
                // Named-curve: `Ecdsa::verify_sha384` is P-256 with a SHA-384
                // hash, which is a real pairing but not the common one -- a
                // SHA-384 signature is overwhelmingly made by a P-384 key,
                // and that is precisely the case the P-256 parser refused.
                if let Some(pub_key) = parse_named_ec_public_key(issuer_spki) {
                    verify_named_ecdsa(
                        &pub_key,
                        &Sha384::digest(&self.tbs_bytes),
                        &self.signature_bytes,
                    )
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Check if the certificate is currently valid (time-wise).
    pub fn is_valid_at(&self, time_secs: i64) -> bool {
        time_secs >= self.not_before && time_secs <= self.not_after
    }

    /// Does this certificate's subject match the given (slash-form) CN string?
    ///
    /// **Do NOT use this for trust decisions.** CN-string equality is *not* a
    /// safe basis for chain construction or anchor matching: two distinct
    /// issuers can share a CN, enabling chain-confusion. This helper exists
    /// only for diagnostics / display. Trust matching must use full Name-DER
    /// equality — see [`X509Cert::subject_der_matches`] and
    /// [`x509_manager::validate_chain`].
    pub fn subject_cn_matches(&self, candidate_cn: &str) -> bool {
        !self.subject_cn.is_empty() && self.subject_cn == candidate_cn
    }

    /// Does this certificate's subject Name DER exactly equal `issuer_der`?
    ///
    /// This is the trust-safe predicate used for PKIX anchor matching and
    /// chain continuity: it compares the *entire* DER-encoded
    /// `subject`/`issuer` Name (all RDNs), mirroring
    /// `x509_manager::validate_chain`'s `issuer_der == subject_der` check.
    /// An empty subject Name never matches, so an unparsed certificate can
    /// never masquerade as a trust anchor.
    pub fn subject_der_matches(&self, issuer_der: &[u8]) -> bool {
        !self.subject_raw.is_empty() && self.subject_raw == issuer_der
    }
}

/// Result of a PKIX chain validation: either success or a named failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkixError {
    /// Chain was empty.
    EmptyChain,
    /// A certificate in the chain is expired or not yet valid.
    Expired { subject: String },
    /// No trust anchor (issuer) found for the top-of-chain certificate.
    UntrustedRoot { subject: String },
    /// A signature verification failed at some level.
    SignatureInvalid { subject: String, issuer: String },
    /// Chain length exceeds the safety limit.
    ChainTooLong,
}

impl core::fmt::Display for PkixError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PkixError::EmptyChain => write!(f, "empty certificate chain"),
            PkixError::Expired { subject } => {
                write!(f, "certificate expired or not yet valid: {subject}")
            }
            PkixError::UntrustedRoot { subject } => {
                write!(f, "no trust anchor matches issuer of: {subject}")
            }
            PkixError::SignatureInvalid { subject, issuer } => {
                write!(f, "signature invalid on {subject} by {issuer}")
            }
            PkixError::ChainTooLong => write!(f, "chain exceeds maximum depth"),
        }
    }
}

/// Validate a server certificate chain against a set of trust anchors.
///
/// `chain` must be ordered from leaf (index 0) to intermediate (higher
/// indices). The function walks the chain, at each step:
///
///   1. Checks time validity against `now_secs` (unix epoch seconds; pass
///      `0` to skip time checks, e.g. in offline test harnesses).
///   2. Verifies the current certificate's signature against the next
///      certificate in the chain or against a trust anchor.
///   3. Stops with `Ok(())` the moment an issuer is found in `trust_anchors`.
///
/// A hard cap of 10 levels guards against malformed chains looping.
///
/// # WARNING — not the production trust path
///
/// This validator is **test-only** and is deliberately **not wired** to the
/// live TLS / `X509TrustManager` path; the production validator is
/// [`x509_manager::validate_chain`], which performs BasicConstraints CA
/// checks, full RFC 5280 §6 anchor handling and fail-closed OID dispatch.
/// **Do not adopt this function for real trust decisions.** Matching here is
/// done on full subject/issuer Name DER (never on the CN string alone) so it
/// cannot become a silent CN-confusion bypass, but it still lacks the CA /
/// pathlen / keyUsage checks the production path enforces.
pub fn verify_cert_chain(
    chain: &[X509Cert],
    trust_anchors: &[X509Cert],
    now_secs: i64,
) -> Result<(), PkixError> {
    const MAX_DEPTH: usize = 10;
    if chain.is_empty() {
        return Err(PkixError::EmptyChain);
    }
    if chain.len() > MAX_DEPTH {
        return Err(PkixError::ChainTooLong);
    }

    for (i, cert) in chain.iter().enumerate() {
        if now_secs != 0 && !cert.is_valid_at(now_secs) {
            return Err(PkixError::Expired {
                subject: cert.subject_cn.clone(),
            });
        }
        // Find the issuer: first try the chain's next element (more specific),
        // then fall through to the trust anchor set. Both lookups match on the
        // FULL issuer/subject Name DER (`subject_der_matches`), never on the CN
        // string — CN-only equality is a chain-confusion bypass (two distinct
        // issuers can share a CN). When the next chain element is used as the
        // issuer we additionally require strict Name-DER continuity, matching
        // `x509_manager::validate_chain` step 3.
        let next_in_chain = chain
            .get(i + 1)
            .filter(|next| next.subject_der_matches(&cert.issuer_raw));
        let anchor = trust_anchors
            .iter()
            .find(|a| a.subject_der_matches(&cert.issuer_raw));
        let issuer = next_in_chain.or(anchor);
        let Some(issuer_cert) = issuer else {
            return Err(PkixError::UntrustedRoot {
                subject: cert.subject_cn.clone(),
            });
        };
        if !cert.verify_signature(&issuer_cert.public_key_bytes) {
            return Err(PkixError::SignatureInvalid {
                subject: cert.subject_cn.clone(),
                issuer: issuer_cert.subject_cn.clone(),
            });
        }
        // If the issuer is itself a trust anchor, the chain terminates.
        if anchor.is_some() {
            return Ok(());
        }
    }
    // Fell off the chain without ever touching a trust anchor.
    Err(PkixError::UntrustedRoot {
        subject: chain
            .last()
            .map(|c| c.subject_cn.clone())
            .unwrap_or_default(),
    })
}

fn oid_to_sig_name(alg_seq_content: &[u8]) -> String {
    // Extract OID bytes
    if alg_seq_content.len() < 2 || alg_seq_content[0] != 0x06 {
        return "Unknown".into();
    }
    let oid_len = alg_seq_content[1] as usize;
    if alg_seq_content.len() < 2 + oid_len {
        return "Unknown".into();
    }
    let oid = &alg_seq_content[2..2 + oid_len];

    // Match common OIDs
    match oid {
        // 1.2.840.113549.1.1.11 = sha256WithRSAEncryption
        [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b] => "SHA256withRSA".into(),
        // 1.2.840.113549.1.1.12 = sha384WithRSAEncryption
        [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c] => "SHA384withRSA".into(),
        // 1.2.840.113549.1.1.13 = sha512WithRSAEncryption
        [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d] => "SHA512withRSA".into(),
        // 1.2.840.113549.1.1.5 = sha1WithRSAEncryption
        [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05] => "SHA1withRSA".into(),
        // 1.2.840.10045.4.3.2 = ecdsa-with-SHA256
        [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02] => "SHA256withECDSA".into(),
        // 1.2.840.10045.4.3.3 = ecdsa-with-SHA384
        [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03] => "SHA384withECDSA".into(),
        _ => "Unknown".into(),
    }
}

fn extract_cn(name_content: &[u8]) -> String {
    // Walk through SET OF AttributeTypeAndValue looking for CN (OID 2.5.4.3)
    let cn_oid: &[u8] = &[0x55, 0x04, 0x03];
    let mut pos = name_content;
    while !pos.is_empty() && pos[0] == 0x31 {
        if let Some((set_len, set_content)) = der_read_tag_length(pos) {
            if !set_content.is_empty() && set_content[0] == 0x30 {
                if let Some((_, atv_content)) = der_read_tag_length(set_content) {
                    // OID
                    if atv_content.len() > 4 && atv_content[0] == 0x06 {
                        let oid_len = atv_content[1] as usize;
                        if oid_len == cn_oid.len() && atv_content.len() >= 2 + oid_len {
                            if &atv_content[2..2 + oid_len] == cn_oid {
                                let val_start = 2 + oid_len;
                                if atv_content.len() > val_start + 1 {
                                    let val_len = atv_content[val_start + 1] as usize;
                                    let val_start2 = val_start + 2;
                                    if atv_content.len() >= val_start2 + val_len {
                                        return String::from_utf8_lossy(
                                            &atv_content[val_start2..val_start2 + val_len],
                                        )
                                        .into_owned();
                                    }
                                }
                            }
                        }
                    }
                }
            }
            pos = &pos[set_len..];
        } else {
            break;
        }
    }
    String::new()
}

fn parse_validity(content: &[u8]) -> (i64, i64) {
    let mut pos = content;
    let not_before = parse_asn1_time(&mut pos);
    let not_after = parse_asn1_time(&mut pos);
    (not_before, not_after)
}

fn parse_asn1_time(pos: &mut &[u8]) -> i64 {
    if pos.is_empty() {
        return 0;
    }
    let tag = pos[0];
    let (total_len, content) = match der_read_tag_length(pos) {
        Some(v) => v,
        None => return 0,
    };
    *pos = &pos[total_len..];
    let time_str = std::str::from_utf8(content).unwrap_or("");

    match tag {
        0x17 => parse_utc_time(time_str),         // UTCTime
        0x18 => parse_generalized_time(time_str), // GeneralizedTime
        _ => 0,
    }
}

fn parse_utc_time(s: &str) -> i64 {
    // YYMMDDHHMMSSZ
    if s.len() < 12 {
        return 0;
    }
    let yy: i32 = s[0..2].parse().unwrap_or(0);
    let year = if yy >= 50 { 1900 + yy } else { 2000 + yy };
    let month: u32 = s[2..4].parse().unwrap_or(1);
    let day: u32 = s[4..6].parse().unwrap_or(1);
    let hour: u32 = s[6..8].parse().unwrap_or(0);
    let min: u32 = s[8..10].parse().unwrap_or(0);
    let sec: u32 = s[10..12].parse().unwrap_or(0);
    datetime_to_epoch(year, month, day, hour, min, sec)
}

fn parse_generalized_time(s: &str) -> i64 {
    // YYYYMMDDHHMMSSZ
    if s.len() < 14 {
        return 0;
    }
    let year: i32 = s[0..4].parse().unwrap_or(2000);
    let month: u32 = s[4..6].parse().unwrap_or(1);
    let day: u32 = s[6..8].parse().unwrap_or(1);
    let hour: u32 = s[8..10].parse().unwrap_or(0);
    let min: u32 = s[10..12].parse().unwrap_or(0);
    let sec: u32 = s[12..14].parse().unwrap_or(0);
    datetime_to_epoch(year, month, day, hour, min, sec)
}

fn datetime_to_epoch(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> i64 {
    // Simplified: days since epoch
    let mut days = 0i64;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    let mdays = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += mdays[m as usize] as i64;
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += (day as i64) - 1;
    days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn detect_pk_algorithm(spki: &[u8]) -> String {
    // Check for RSA OID: 1.2.840.113549.1.1.1
    let rsa_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    // Check for EC OID: 1.2.840.10045.2.1
    let ec_oid = [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];

    if spki.windows(rsa_oid.len()).any(|w| w == rsa_oid) {
        "RSA".into()
    } else if spki.windows(ec_oid.len()).any(|w| w == ec_oid) {
        "EC".into()
    } else {
        "Unknown".into()
    }
}

/// Parse an RSA public key from DER SubjectPublicKeyInfo.
pub fn parse_rsa_public_key(spki: &[u8]) -> Option<RsaPublicKey> {
    // Navigate: SEQUENCE { SEQUENCE { OID, NULL }, BIT STRING { SEQUENCE { INTEGER n, INTEGER e } } }
    let (_, outer) = der_read_tag_length(spki)?;
    // Skip AlgorithmIdentifier
    let (alg_len, _) = der_read_tag_length(outer)?;
    let rest = &outer[alg_len..];
    // BIT STRING
    if rest.is_empty() || rest[0] != 0x03 {
        return None;
    }
    let (_, bs_content) = der_read_tag_length(rest)?;
    if bs_content.is_empty() {
        return None;
    }
    let key_seq = &bs_content[1..]; // skip unused bits byte
                                    // SEQUENCE { INTEGER n, INTEGER e }
    let (_, seq_content) = der_read_tag_length(key_seq)?;
    let (n_bytes, rest2) = der_read_integer(seq_content)?;
    let (e_bytes, _) = der_read_integer(rest2)?;
    Some(RsaPublicKey {
        n: BigUint::from_bytes_be(&n_bytes),
        e: BigUint::from_bytes_be(&e_bytes),
    })
}

/// Parse an RSA private key from a PKCS#8 `PrivateKeyInfo` DER blob:
/// `SEQUENCE { version INTEGER, AlgorithmIdentifier, privateKey OCTET STRING }`
/// where the OCTET STRING contains the traditional (PKCS#1) `RSAPrivateKey`
/// `SEQUENCE { version, n, e, d, p, q, dp, dq, qinv }`.
pub fn parse_rsa_private_key_pkcs8(der: &[u8]) -> Option<RsaKeyPairData> {
    let (_, outer) = der_read_tag_length(der)?;
    let (_version, rest) = der_read_integer(outer)?;
    // Skip AlgorithmIdentifier SEQUENCE.
    let (alg_len, _) = der_read_tag_length(rest)?;
    let rest = &rest[alg_len..];
    // privateKey OCTET STRING wrapping the inner RSAPrivateKey SEQUENCE.
    if rest.is_empty() || rest[0] != 0x04 {
        return None;
    }
    let (_, octet_content) = der_read_tag_length(rest)?;
    let (_, inner_seq) = der_read_tag_length(octet_content)?;
    let (_inner_version, r) = der_read_integer(inner_seq)?;
    let (n, r) = der_read_integer(r)?;
    let (e, r) = der_read_integer(r)?;
    let (d, r) = der_read_integer(r)?;
    let (p, r) = der_read_integer(r)?;
    let (q, r) = der_read_integer(r)?;
    let (dp, r) = der_read_integer(r)?;
    let (dq, r) = der_read_integer(r)?;
    let (qinv, _r) = der_read_integer(r)?;
    Some(RsaKeyPairData {
        public_key: RsaPublicKey {
            n: BigUint::from_bytes_be(&n),
            e: BigUint::from_bytes_be(&e),
        },
        private_key: RsaPrivateKey {
            n: BigUint::from_bytes_be(&n),
            d: BigUint::from_bytes_be(&d),
            e: BigUint::from_bytes_be(&e),
            p: Some(BigUint::from_bytes_be(&p)),
            q: Some(BigUint::from_bytes_be(&q)),
            dp: Some(BigUint::from_bytes_be(&dp)),
            dq: Some(BigUint::from_bytes_be(&dq)),
            qinv: Some(BigUint::from_bytes_be(&qinv)),
        },
    })
}

/// Parse an ECDSA public key from DER SubjectPublicKeyInfo.
pub fn parse_ecdsa_public_key(spki: &[u8]) -> Option<EcdsaPublicKey> {
    let (_, outer) = der_read_tag_length(spki)?;
    let (alg_len, _) = der_read_tag_length(outer)?;
    let rest = &outer[alg_len..];
    if rest.is_empty() || rest[0] != 0x03 {
        return None;
    }
    let (_, bs_content) = der_read_tag_length(rest)?;
    if bs_content.is_empty() {
        return None;
    }
    let pk_bytes = &bs_content[1..]; // skip unused bits
    Ecdsa::public_key_from_bytes(pk_bytes)
}

// ---------------------------------------------------------------------------
// Named-curve ECDSA verification (P-256 / P-384 / P-521)
// ---------------------------------------------------------------------------
//
// WHY A SECOND ECDSA IMPLEMENTATION EXISTS BESIDE `Ecdsa`.
//
// `Ecdsa`/`EcPoint`/`FieldElement256` above are P-256 and only P-256:
// `public_key_from_bytes` requires exactly 65 bytes, the field element is a
// fixed 4x64-bit type, and the generator is hard-coded to P-256's. Every one
// of those is correct for what it was written for and none of them can be
// asked about another curve.
//
// That was invisible for as long as the certificate validator was never handed
// a real chain. MEASURED the moment it was
// (`tls-client-captures-only-the-leaf-...`, RealChainProbe, 20 live public
// sites): SIX rejected with `BadSignature`, and every one of the six had a
// P-384 issuer key -- Let's Encrypt's YE1/YE2 under Root YE, Sectigo's
// Server Authentication Root E46, DigiCert's Global G3 TLS ECC, Google's WE1
// under GTS Root R4. The modern public ECDSA hierarchy is P-384 at the top
// almost everywhere, so "P-256 only" is not a corner: it is most of the
// internet's ECDSA chains.
//
// The parameters below were read off OpenSSL 3.0
// (`openssl ecparam -name <curve> -param_enc explicit -text -noout`) rather
// than transcribed from a document, and `A` came back as `p - 3` on all three,
// which is what lets the doubling formula below assume `a = -3`. They are
// exercised end to end: the tests at the bottom verify signatures made by
// OpenSSL itself.
//
// Speed is deliberately not the goal. Point arithmetic runs in Jacobian
// coordinates (one modular inversion per scalar multiplication instead of one
// per addition), over the general-purpose `BigUint` rather than a
// curve-specialised field. A handshake verifies two or three signatures; the
// hot paths in this VM are elsewhere.

/// A NIST prime-field short-Weierstrass curve `y^2 = x^3 - 3x + b (mod p)`.
pub struct NistCurve {
    /// The named-curve OID's CONTENT bytes, as they appear inside the
    /// `AlgorithmIdentifier` parameters of a `SubjectPublicKeyInfo`.
    pub oid: &'static [u8],
    pub name: &'static str,
    p_hex: &'static str,
    b_hex: &'static str,
    gx_hex: &'static str,
    gy_hex: &'static str,
    n_hex: &'static str,
    /// Bytes per field element in the uncompressed point encoding.
    pub field_bytes: usize,
}

/// `1.2.840.10045.3.1.7` — prime256v1 / secp256r1 / NIST P-256.
pub const OID_EC_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// `1.3.132.0.34` — secp384r1 / NIST P-384.
pub const OID_EC_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
/// `1.3.132.0.35` — secp521r1 / NIST P-521.
pub const OID_EC_P521: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x23];

pub static NIST_P256: NistCurve = NistCurve {
    oid: OID_EC_P256,
    name: "P-256",
    p_hex: "ffffffff00000001000000000000000000000000ffffffffffffffffffffffff",
    b_hex: "5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b",
    gx_hex: "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
    gy_hex: "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
    n_hex: "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551",
    field_bytes: 32,
};

pub static NIST_P384: NistCurve = NistCurve {
    oid: OID_EC_P384,
    name: "P-384",
    p_hex: "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe\
            ffffffff0000000000000000ffffffff",
    b_hex: "b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875a\
            c656398d8a2ed19d2a85c8edd3ec2aef",
    gx_hex: "aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a38\
             5502f25dbf55296c3a545e3872760ab7",
    gy_hex: "3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c0\
             0a60b1ce1d7e819d7a431d7c90ea0e5f",
    n_hex: "ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf\
            581a0db248b0a77aecec196accc52973",
    field_bytes: 48,
};

pub static NIST_P521: NistCurve = NistCurve {
    oid: OID_EC_P521,
    name: "P-521",
    p_hex: "01ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff\
            ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff\
            ffff",
    b_hex: "51953eb9618e1c9a1f929a21a0b68540eea2da725b99b315f3b8b489918ef109\
            e156193951ec7e937b1652c0bd3bb1bf073573df883d2c34f1ef451fd46b503f\
            00",
    gx_hex: "00c6858e06b70404e9cd9e3ecb662395b4429c648139053fb521f828af606b4d\
             3dbaa14b5e77efe75928fe1dc127a2ffa8de3348b3c1856a429bf97e7e31c2e5\
             bd66",
    gy_hex: "011839296a789a3bc0045c8a5fb42c7d1bd998f54449579b446817afbd17273e\
             662c97ee72995ef42640c550b9013fad0761353c7086a272c24088be94769fd1\
             6650",
    n_hex: "01ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff\
            fffa51868783bf2f966b7fcc0148f709a5d03bb5c9b8899c47aebb6fb71e9138\
            6409",
    field_bytes: 66,
};

/// The curve a named-curve OID selects, or `None` for one this verifier does
/// not implement. `None` is a REFUSAL, never a silent success: the caller
/// reports it as a failed signature, which is the safe direction.
pub fn nist_curve_for_oid(oid_content: &[u8]) -> Option<&'static NistCurve> {
    for c in [&NIST_P256, &NIST_P384, &NIST_P521] {
        if c.oid == oid_content {
            return Some(c);
        }
    }
    None
}

fn hex_to_biguint(h: &str) -> BigUint {
    let mut bytes = Vec::with_capacity(h.len() / 2 + 1);
    let digits: Vec<u8> = h
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .map(|b| match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            // The literals above are compile-time constants in this file; a
            // non-hex byte here is a source-editing mistake, not input.
            _ => unreachable!("non-hex digit in a curve parameter literal"),
        })
        .collect();
    // An odd digit count would silently shift every byte; the constants are
    // written in whole bytes, so treat it as the editing mistake it is.
    assert!(
        digits.len() % 2 == 0,
        "curve parameter has an odd hex digit count"
    );
    for pair in digits.chunks(2) {
        bytes.push((pair[0] << 4) | pair[1]);
    }
    BigUint::from_bytes_be(&bytes)
}

/// The parsed parameters, built once per curve.
struct CurveParams {
    p: BigUint,
    b: BigUint,
    gx: BigUint,
    gy: BigUint,
    n: BigUint,
}

impl NistCurve {
    fn params(&'static self) -> &'static CurveParams {
        // LOCK LEVEL (lock-discipline ratchet): `Scratch`. `params` takes
        // `&'static self` and has no `NativeContext` at all, so the guard
        // cannot span a re-entry into the VM by construction — the body under
        // it is `hex_to_biguint` and a `Box::leak`, both pure.
        static CACHE: std::sync::OnceLock<
            cratonvm_types::lock_order::OrderedPlMutex<
                std::collections::HashMap<&'static str, &'static CurveParams>,
            >,
        > = std::sync::OnceLock::new();
        let cache = CACHE.get_or_init(|| {
            cratonvm_types::lock_order::OrderedPlMutex::new(
                std::collections::HashMap::new(),
                cratonvm_types::lock_order::LockLevel::Scratch,
            )
        });
        let mut guard = cache.lock();
        if let Some(found) = guard.get(self.name) {
            return found;
        }
        // Leaked deliberately: three curves, once each, for the process
        // lifetime. The alternative is re-parsing five big integers on every
        // signature verification.
        let params: &'static CurveParams = Box::leak(Box::new(CurveParams {
            p: hex_to_biguint(self.p_hex),
            b: hex_to_biguint(self.b_hex),
            gx: hex_to_biguint(self.gx_hex),
            gy: hex_to_biguint(self.gy_hex),
            n: hex_to_biguint(self.n_hex),
        }));
        guard.insert(self.name, params);
        params
    }

    /// The curve's group order, for the range checks and the `mod n`
    /// arithmetic in ECDSA verification.
    pub fn order(&'static self) -> &'static BigUint {
        &self.params().n
    }
}

/// A point in Jacobian coordinates: affine `(X/Z^2, Y/Z^3)`, with `Z == 0`
/// standing for the point at infinity.
#[derive(Clone)]
struct JPoint {
    x: BigUint,
    y: BigUint,
    z: BigUint,
}

fn mod_add(a: &BigUint, b: &BigUint, p: &BigUint) -> BigUint {
    a.add(b).modulo(p)
}

/// `a - b (mod p)`, with both operands already reduced. Written out rather
/// than `a.sub(b)` because `BigUint` is UNSIGNED: the borrow case has to be
/// turned into `a + p - b` before the subtraction, not after it.
fn mod_sub(a: &BigUint, b: &BigUint, p: &BigUint) -> BigUint {
    match a.cmp(b) {
        std::cmp::Ordering::Less => a.add(p).sub(b).modulo(p),
        _ => a.sub(b).modulo(p),
    }
}

fn mod_mul(a: &BigUint, b: &BigUint, p: &BigUint) -> BigUint {
    a.mul(b).modulo(p)
}

fn mod_sqr(a: &BigUint, p: &BigUint) -> BigUint {
    a.mul(a).modulo(p)
}

fn mod_mul_small(a: &BigUint, k: u32, p: &BigUint) -> BigUint {
    a.mul_u32(k).modulo(p)
}

impl JPoint {
    fn infinity() -> Self {
        JPoint {
            x: BigUint::one(),
            y: BigUint::one(),
            z: BigUint::zero(),
        }
    }

    fn is_infinity(&self) -> bool {
        self.z.is_zero()
    }

    fn from_affine(x: BigUint, y: BigUint) -> Self {
        JPoint {
            x,
            y,
            z: BigUint::one(),
        }
    }

    /// `dbl-2001-b`, the standard `a = -3` doubling. Every NIST prime curve
    /// here has `a = p - 3`, which OpenSSL's own explicit parameters confirm.
    fn double(&self, p: &BigUint) -> JPoint {
        if self.is_infinity() || self.y.is_zero() {
            return JPoint::infinity();
        }
        let delta = mod_sqr(&self.z, p);
        let gamma = mod_sqr(&self.y, p);
        let beta = mod_mul(&self.x, &gamma, p);
        let alpha = mod_mul(
            &mod_mul_small(&mod_sub(&self.x, &delta, p), 3, p),
            &mod_add(&self.x, &delta, p),
            p,
        );
        let x3 = mod_sub(&mod_sqr(&alpha, p), &mod_mul_small(&beta, 8, p), p);
        let z3 = mod_sub(
            &mod_sub(&mod_sqr(&mod_add(&self.y, &self.z, p), p), &gamma, p),
            &delta,
            p,
        );
        let y3 = mod_sub(
            &mod_mul(&alpha, &mod_sub(&mod_mul_small(&beta, 4, p), &x3, p), p),
            &mod_mul_small(&mod_sqr(&gamma, p), 8, p),
            p,
        );
        JPoint {
            x: x3,
            y: y3,
            z: z3,
        }
    }

    /// `add-2007-bl`.
    fn add(&self, other: &JPoint, p: &BigUint) -> JPoint {
        if self.is_infinity() {
            return other.clone();
        }
        if other.is_infinity() {
            return self.clone();
        }
        let z1z1 = mod_sqr(&self.z, p);
        let z2z2 = mod_sqr(&other.z, p);
        let u1 = mod_mul(&self.x, &z2z2, p);
        let u2 = mod_mul(&other.x, &z1z1, p);
        let s1 = mod_mul(&mod_mul(&self.y, &other.z, p), &z2z2, p);
        let s2 = mod_mul(&mod_mul(&other.y, &self.z, p), &z1z1, p);
        if u1.cmp(&u2) == std::cmp::Ordering::Equal {
            return if s1.cmp(&s2) == std::cmp::Ordering::Equal {
                self.double(p)
            } else {
                JPoint::infinity()
            };
        }
        let h = mod_sub(&u2, &u1, p);
        let i = mod_sqr(&mod_mul_small(&h, 2, p), p);
        let j = mod_mul(&h, &i, p);
        let r = mod_mul_small(&mod_sub(&s2, &s1, p), 2, p);
        let v = mod_mul(&u1, &i, p);
        let x3 = mod_sub(
            &mod_sub(&mod_sqr(&r, p), &j, p),
            &mod_mul_small(&v, 2, p),
            p,
        );
        let y3 = mod_sub(
            &mod_mul(&r, &mod_sub(&v, &x3, p), p),
            &mod_mul_small(&mod_mul(&s1, &j, p), 2, p),
            p,
        );
        let z3 = mod_mul(
            &mod_sub(
                &mod_sub(&mod_sqr(&mod_add(&self.z, &other.z, p), p), &z1z1, p),
                &z2z2,
                p,
            ),
            &h,
            p,
        );
        JPoint {
            x: x3,
            y: y3,
            z: z3,
        }
    }

    fn scalar_mul(&self, k: &BigUint, p: &BigUint) -> JPoint {
        let mut acc = JPoint::infinity();
        let bits = k.bit_length();
        if bits == 0 {
            return acc;
        }
        for i in (0..bits).rev() {
            acc = acc.double(p);
            if k.bit(i) {
                acc = acc.add(self, p);
            }
        }
        acc
    }

    /// The affine x coordinate, or `None` at infinity.
    fn affine_x(&self, p: &BigUint) -> Option<BigUint> {
        if self.is_infinity() {
            return None;
        }
        let z_inv = self.z.modinv(p)?;
        Some(mod_mul(&self.x, &mod_sqr(&z_inv, p), p))
    }
}

/// A public key on a named curve, as recovered from a `SubjectPublicKeyInfo`.
pub struct NamedEcPublicKey {
    pub curve: &'static NistCurve,
    x: BigUint,
    y: BigUint,
}

/// Parse a `SubjectPublicKeyInfo` that names one of the curves above.
///
/// The curve OID is READ, not assumed. `parse_ecdsa_public_key` (the P-256
/// path beside this) discards the `AlgorithmIdentifier` parameters entirely
/// and then requires a 65-byte point, which is how a P-384 key came back as
/// `None` and the caller reported a bad signature -- a REFUSAL that reads
/// exactly like a forged certificate.
pub fn parse_named_ec_public_key(spki: &[u8]) -> Option<NamedEcPublicKey> {
    let (_, outer) = der_read_tag_length(spki)?;
    let (alg_total, alg_content) = der_read_tag_length(outer)?;
    // AlgorithmIdentifier ::= SEQUENCE { algorithm OID, parameters ANY }.
    // The first OID must be id-ecPublicKey; the second is the named curve.
    let (first_total, _) = der_read_tag_length(alg_content)?;
    if alg_content.first() != Some(&0x06) {
        return None;
    }
    let params = alg_content.get(first_total..)?;
    if params.first() != Some(&0x06) {
        // Explicit (non-named) curve parameters, or an absent one. Neither is
        // something a public PKIX chain uses, and guessing is not an option.
        return None;
    }
    let (_, curve_oid) = der_read_tag_length(params)?;
    let curve = nist_curve_for_oid(curve_oid)?;

    let rest = outer.get(alg_total..)?;
    if rest.first() != Some(&0x03) {
        return None;
    }
    let (_, bs_content) = der_read_tag_length(rest)?;
    if bs_content.is_empty() {
        return None;
    }
    let point = &bs_content[1..]; // skip the unused-bits octet
    let fb = curve.field_bytes;
    if point.len() != 1 + 2 * fb || point[0] != 0x04 {
        // Compressed points are legal ASN.1 and are not used by any CA whose
        // chain reaches this code; refusing is the safe answer.
        return None;
    }
    Some(NamedEcPublicKey {
        curve,
        x: BigUint::from_bytes_be(&point[1..1 + fb]),
        y: BigUint::from_bytes_be(&point[1 + fb..1 + 2 * fb]),
    })
}

/// ECDSA verification (FIPS 186-4 §6.4) on the key's own curve.
///
/// `digest` is the PRE-HASHED message. Its leftmost `min(bitlen(n), 8*len)`
/// bits become `z`, which is what makes SHA-256 usable with P-384 and
/// SHA-512 with P-256 without a separate path per pairing.
pub fn verify_named_ecdsa(key: &NamedEcPublicKey, digest: &[u8], der_sig: &[u8]) -> bool {
    let (r_bytes, s_bytes) = match der_decode_ecdsa_signature(der_sig) {
        Some(v) => v,
        None => return false,
    };
    let params = key.curve.params();
    let (p, n) = (&params.p, &params.n);
    let r = BigUint::from_bytes_be(&r_bytes);
    let s = BigUint::from_bytes_be(&s_bytes);
    if r.is_zero() || r.cmp(n) != std::cmp::Ordering::Less {
        return false;
    }
    if s.is_zero() || s.cmp(n) != std::cmp::Ordering::Less {
        return false;
    }
    // The public key must actually be ON the curve. Without this an attacker
    // can supply a point on a different (weaker) curve and have the group law
    // above compute in that group instead -- the classic invalid-curve attack.
    // Cheap here, and this verifier is reachable from certificate parsing.
    let lhs = mod_sqr(&key.y, p);
    let rhs = mod_add(
        &mod_sub(
            &mod_mul(&mod_sqr(&key.x, p), &key.x, p),
            &mod_mul_small(&key.x, 3, p),
            p,
        ),
        &params.b,
        p,
    );
    if lhs.cmp(&rhs) != std::cmp::Ordering::Equal {
        return false;
    }

    let n_bits = n.bit_length();
    let digest_bits = digest.len() * 8;
    let mut z = BigUint::from_bytes_be(digest);
    if digest_bits > n_bits {
        z = z.shr_bits((digest_bits - n_bits) as u32);
    }
    let z = z.modulo(n);

    let s_inv = match s.modinv(n) {
        Some(v) => v,
        None => return false,
    };
    let u1 = z.mul(&s_inv).modulo(n);
    let u2 = r.mul(&s_inv).modulo(n);

    let g = JPoint::from_affine(params.gx.clone(), params.gy.clone());
    let q = JPoint::from_affine(key.x.clone(), key.y.clone());
    let point = g.scalar_mul(&u1, p).add(&q.scalar_mul(&u2, p), p);
    match point.affine_x(p) {
        Some(x) => x.modulo(n).cmp(&r) == std::cmp::Ordering::Equal,
        None => false,
    }
}

/// The curve table, checked as a table.
///
/// A transcription error in `p`, `n` or `b` does not produce a wrong answer —
/// it produces a verifier that rejects EVERYTHING, which is indistinguishable
/// from "the signature was bad" and is exactly the failure mode this whole
/// section exists to fix. So the parameters are checked as parameters, with
/// relations that only hold for the real curve.
#[cfg(test)]
mod named_curve_param_tests {
    use super::*;

    #[test]
    fn the_generator_is_on_the_curve() {
        for curve in [&NIST_P256, &NIST_P384, &NIST_P521] {
            let params = curve.params();
            let p = &params.p;
            let lhs = mod_sqr(&params.gy, p);
            let rhs = mod_add(
                &mod_sub(
                    &mod_mul(&mod_sqr(&params.gx, p), &params.gx, p),
                    &mod_mul_small(&params.gx, 3, p),
                    p,
                ),
                &params.b,
                p,
            );
            assert_eq!(
                lhs.to_bytes_be(),
                rhs.to_bytes_be(),
                "{}: the generator is not on y^2 = x^3 - 3x + b — a parameter is wrong",
                curve.name
            );
        }
    }

    /// `n * G` is the identity: the definition of the group order, and the one
    /// relation that exercises the whole double-and-add ladder — both point
    /// formulas, on every curve — against a value that is not derived from
    /// them.
    #[test]
    fn the_generator_has_the_stated_order() {
        for curve in [&NIST_P256, &NIST_P384, &NIST_P521] {
            let params = curve.params();
            let g = JPoint::from_affine(params.gx.clone(), params.gy.clone());
            assert!(
                g.scalar_mul(&params.n, &params.p).is_infinity(),
                "{}: n*G is not the identity — the group law or a parameter is wrong",
                curve.name
            );
            // …and (n-1)*G is NOT, or a ladder that returned infinity for
            // every scalar would pass the line above just as well.
            assert!(
                !g.scalar_mul(&params.n.sub(&BigUint::one()), &params.p)
                    .is_infinity(),
                "{}: (n-1)*G is the identity — the ladder collapses everything",
                curve.name
            );
        }
    }

    #[test]
    fn field_and_order_have_the_documented_bit_lengths() {
        for (curve, bits) in [(&NIST_P256, 256), (&NIST_P384, 384), (&NIST_P521, 521)] {
            let params = curve.params();
            assert_eq!(params.p.bit_length(), bits, "{} p", curve.name);
            assert_eq!(params.n.bit_length(), bits, "{} n", curve.name);
            assert_eq!(
                curve.field_bytes,
                bits.div_ceil(8),
                "{} field_bytes",
                curve.name
            );
        }
    }

    /// The OID is READ, not assumed. That is the whole difference from
    /// `parse_ecdsa_public_key`, which discards the named-curve OID and then
    /// fails on the point LENGTH — so a perfectly good P-384 key read as a
    /// corrupt one and the caller reported a bad signature.
    #[test]
    fn an_unsupported_curve_oid_parses_to_none() {
        // secp256k1 (1.3.132.0.10): a real curve, deliberately not implemented.
        assert!(nist_curve_for_oid(&[0x2b, 0x81, 0x04, 0x00, 0x0a]).is_none());
        assert_eq!(
            nist_curve_for_oid(OID_EC_P256).map(|c| c.name),
            Some("P-256")
        );
        assert_eq!(
            nist_curve_for_oid(OID_EC_P384).map(|c| c.name),
            Some("P-384")
        );
        assert_eq!(
            nist_curve_for_oid(OID_EC_P521).map(|c| c.name),
            Some("P-521")
        );
    }

    /// A key that is not ON the curve must be refused before the group law
    /// touches it — the invalid-curve attack, which this verifier is reachable
    /// from certificate parsing by.
    #[test]
    fn a_public_key_off_the_curve_is_refused() {
        let params = NIST_P384.params();
        let key = NamedEcPublicKey {
            curve: &NIST_P384,
            x: params.gx.clone(),
            y: mod_add(&params.gy, &BigUint::one(), &params.p),
        };
        // A well-formed (r, s), so the refusal cannot be the signature
        // decoder's doing instead.
        let sig = der_encode_ecdsa_signature(&[0x01, 0x02], &[0x03, 0x04]);
        assert!(!verify_named_ecdsa(&key, &[7u8; 48], &sig));
    }
}

/// The verifier, checked against signatures it did not make.
///
/// A verifier tested only against its own signer proves the two AGREE, not
/// that either is right — and there is no signer here at all, only a verifier,
/// so the arithmetic has to be checked against an outside authority. OpenSSL
/// is that authority: it generates the key, makes the signature, and encodes
/// the SPKI; this code only reads and verifies.
///
/// Every acceptance is paired with a rejection of the SAME signature over a
/// changed message. Without the pair, a `verify` that returned `true`
/// unconditionally would pass every case here.
#[cfg(all(test, unix))]
mod named_curve_openssl_tests {
    use super::*;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::pkey::PKey;
    use openssl::sign::Signer;

    fn round_trip(nid: Nid, digest: MessageDigest, expect: &str) {
        let group = EcGroup::from_curve_name(nid).expect("group");
        let key = EcKey::generate(&group).expect("keygen");
        let spki = key.public_key_to_der().expect("spki der");
        let pkey = PKey::from_ec_key(key).expect("pkey");

        let msg = b"cratonvm named-curve verification vector";
        let mut signer = Signer::new(digest, &pkey).expect("signer");
        signer.update(msg).expect("update");
        let sig = signer.sign_to_vec().expect("sign");

        let parsed = parse_named_ec_public_key(&spki)
            .unwrap_or_else(|| panic!("{expect}: OpenSSL's own SPKI did not parse"));
        assert_eq!(parsed.curve.name, expect, "curve read from the SPKI");

        let hash = |m: &[u8]| -> Vec<u8> {
            match digest.type_() {
                t if t == MessageDigest::sha256().type_() => Sha256::digest(m).to_vec(),
                t if t == MessageDigest::sha384().type_() => Sha384::digest(m).to_vec(),
                _ => Sha512::digest(m).to_vec(),
            }
        };

        assert!(
            verify_named_ecdsa(&parsed, &hash(msg), &sig),
            "{expect}: a signature OpenSSL made was rejected"
        );
        assert!(
            !verify_named_ecdsa(
                &parsed,
                &hash(b"cratonvm named-curve verification vecto!"),
                &sig
            ),
            "{expect}: the same signature was accepted over a DIFFERENT message"
        );
    }

    #[test]
    fn p256_sha256_round_trips_against_openssl() {
        round_trip(Nid::X9_62_PRIME256V1, MessageDigest::sha256(), "P-256");
    }

    /// The one that was broken. Every ECDSA chain on the public internet whose
    /// issuer key is P-384 — Let's Encrypt Root YE, Sectigo Root E46, DigiCert
    /// Global G3 TLS ECC, Google GTS Root R4 — failed here.
    #[test]
    fn p384_sha384_round_trips_against_openssl() {
        round_trip(Nid::SECP384R1, MessageDigest::sha384(), "P-384");
    }

    #[test]
    fn p521_sha512_round_trips_against_openssl() {
        round_trip(Nid::SECP521R1, MessageDigest::sha512(), "P-521");
    }

    /// A digest WIDER than the curve order has to be truncated to the order's
    /// bit length (FIPS 186-4 §6.4), not reduced modulo it. Getting that wrong
    /// is invisible whenever the two happen to agree, so it is asked
    /// explicitly: SHA-512 over P-256 is the widest mismatch available.
    #[test]
    fn a_digest_wider_than_the_order_is_truncated_not_reduced() {
        round_trip(Nid::X9_62_PRIME256V1, MessageDigest::sha512(), "P-256");
    }

    /// …and a digest NARROWER than the order is used whole.
    #[test]
    fn a_digest_narrower_than_the_order_is_used_whole() {
        round_trip(Nid::SECP521R1, MessageDigest::sha256(), "P-521");
    }
}

// ---------------------------------------------------------------------------
// G60 — KeyStore real loading (JKS + PKCS12)
// ---------------------------------------------------------------------------

use std::collections::HashMap;

/// Represents a single entry in a KeyStore.
#[derive(Clone, Debug)]
pub enum KeyStoreEntry {
    TrustedCert {
        cert: X509Cert,
    },
    PrivateKeyEntry {
        key_bytes: Vec<u8>,
        cert_chain: Vec<X509Cert>,
    },
    SecretKeyEntry {
        key_bytes: Vec<u8>,
        algorithm: String,
    },
}

/// Parsed KeyStore data.
#[derive(Clone, Debug)]
pub struct KeyStoreData {
    pub store_type: String,
    pub entries: HashMap<String, KeyStoreEntry>,
}

/// Global KeyStore entry storage, keyed by a store ID.
static KEYSTORE_STORE: parking_lot::RwLock<Option<HashMap<u64, KeyStoreData>>> =
    parking_lot::RwLock::new(None);

static KEYSTORE_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn keystore_next_id() -> u64 {
    KEYSTORE_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub fn keystore_store(id: u64, data: KeyStoreData) {
    let mut guard = KEYSTORE_STORE.write();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(id, data);
}

pub fn keystore_get(id: u64) -> Option<KeyStoreData> {
    let guard = KEYSTORE_STORE.read();
    guard.as_ref().and_then(|m| m.get(&id).cloned())
}

/// Global certificate store for X.509 certs created via CertificateFactory.
static CERT_STORE: parking_lot::RwLock<Option<HashMap<u64, X509Cert>>> =
    parking_lot::RwLock::new(None);

static CERT_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn cert_next_id() -> u64 {
    CERT_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub fn cert_store(id: u64, cert: X509Cert) {
    let mut guard = CERT_STORE.write();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(id, cert);
}

pub fn cert_get(id: u64) -> Option<X509Cert> {
    let guard = CERT_STORE.read();
    guard.as_ref().and_then(|m| m.get(&id).cloned())
}

/// Global RSA key store for sign/verify operations.
static RSA_KEY_STORE: parking_lot::RwLock<Option<HashMap<u64, RsaKeyPairData>>> =
    parking_lot::RwLock::new(None);

static RSA_KEY_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct RsaKeyPairData {
    pub public_key: RsaPublicKey,
    pub private_key: RsaPrivateKey,
}

pub fn rsa_key_next_id() -> u64 {
    RSA_KEY_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub fn rsa_key_store(id: u64, data: RsaKeyPairData) {
    let mut guard = RSA_KEY_STORE.write();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(id, data);
}

pub fn rsa_key_get_pub(id: u64) -> Option<(Vec<u8>, Vec<u8>)> {
    let guard = RSA_KEY_STORE.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&id))
        .map(|kp| (kp.public_key.n.to_bytes_be(), kp.public_key.e.to_bytes_be()))
}

pub fn rsa_sign(id: u64, message: &[u8]) -> Option<Vec<u8>> {
    let guard = RSA_KEY_STORE.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&id))
        .map(|kp| Rsa::sign_sha256(&kp.private_key, message))
}

/// Is `id` a key this VM actually holds? `Signature.initSign`/`initVerify`
/// need the answer *at init time*: HotSpot refuses a key it cannot use with
/// `InvalidKeyException` there, which is what makes a caller that iterates
/// providers (netty's `JdkDelegatingPrivateKeyMethod.findCompatibleSignature`)
/// move on to the next one. Accepting the key and failing at `sign()` instead
/// makes the caller commit to a provider that can never work.
pub fn rsa_key_registered(id: u64) -> bool {
    if id == 0 {
        return false;
    }
    let guard = RSA_KEY_STORE.read();
    guard.as_ref().is_some_and(|m| m.contains_key(&id))
}

/// [`rsa_sign`] for an arbitrary PKCS#1 v1.5 digest.
pub fn rsa_sign_digest(
    id: u64,
    digest: cratonvm_native_builtins_crypto::signature::DigestAlgorithm,
    message: &[u8],
) -> Option<Vec<u8>> {
    let guard = RSA_KEY_STORE.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&id))
        .map(|kp| Rsa::sign_pkcs1_v15(&kp.private_key, digest, message))
}

/// `Signature.verify()`'s RSA backend for an arbitrary PKCS#1 v1.5 digest.
/// Same `Option` contract as [`rsa_verify`]: `None` means the question was
/// never asked.
pub fn rsa_verify_digest(
    id: u64,
    digest: cratonvm_native_builtins_crypto::signature::DigestAlgorithm,
    message: &[u8],
    signature: &[u8],
) -> Option<bool> {
    let (n, e) = rsa_key_get_pub(id)?;
    match cratonvm_native_builtins_crypto::signature::verify_rsa_pkcs1_v15_checked(
        &n, &e, digest, message, signature,
    ) {
        Ok(v) => Some(v),
        // A rejected key or a wrong-length signature is "never checked", not
        // "did not verify" — the distinction `rsa_verify` exists to keep.
        Err(_) => None,
    }
}

/// `MD5andSHA1withRSA` — the TLS 1.0/1.1 CertificateVerify signature, and the
/// JDK name netty maps `SSL_SIGN_RSA_PKCS1_MD5_SHA1` to.
///
/// The signed value is the 36-byte `MD5(m) || SHA1(m)` concatenation placed in
/// a PKCS#1 v1.5 block type 1 with **no DigestInfo** — there is no OID for the
/// pair, which is why SunJSSE rather than SunRsaSign implements it.
pub fn rsa_md5_sha1_digest(message: &[u8]) -> Vec<u8> {
    let mut out = crate::real_md5(message);
    {
        use sha1::Digest;
        let mut h = sha1::Sha1::new();
        h.update(message);
        out.extend_from_slice(&h.finalize());
    }
    out
}

pub fn rsa_sign_md5_sha1(id: u64, message: &[u8]) -> Option<Vec<u8>> {
    rsa_sign_none(id, &rsa_md5_sha1_digest(message))
}

pub fn rsa_verify_md5_sha1(id: u64, message: &[u8], signature: &[u8]) -> Option<bool> {
    rsa_verify_none(id, &rsa_md5_sha1_digest(message), signature)
}

/// `Signature.verify()`'s RSA backend.
///
/// The `Option` is load-bearing and is the contract `jca::signature`'s
/// `verify_dispatch` relies on:
///
/// * `Some(true)` / `Some(false)` — the signature really was checked against
///   this key, and really does or does not match. `Some(false)` is what a
///   forgery looks like and stays a `false` all the way out to Java.
/// * `None` — the question was never asked: either no key is registered under
///   `id`, **or** the backend refused the key or the signature encoding
///   outright (see [`Rsa::try_verify_sha256`]). `jca::signature` turns this
///   into a `SignatureException`.
///
/// The second `None` case is the migration off the ambiguous `bool` wrapper
/// (`native-builtins-crypto`'s `verify_rsa_pkcs1_v15`, whose only remaining
/// call site was `Rsa::verify_sha256`). Before it, a legitimate signer key the
/// backend rejects — an 8192-bit modulus, say — was reported to Java as a
/// **failed verification**: a trust decision made on no evidence.
pub fn rsa_verify(id: u64, message: &[u8], signature: &[u8]) -> Option<bool> {
    let guard = RSA_KEY_STORE.read();
    let key_pair = guard.as_ref().and_then(|m| m.get(&id))?;
    Rsa::try_verify_sha256(&key_pair.public_key, message, signature).ok()
}

/// `NONEwithRSA` sign, by `crypto_impl` key handle.
pub fn rsa_sign_none(id: u64, data: &[u8]) -> Option<Vec<u8>> {
    let guard = RSA_KEY_STORE.read();
    let key_pair = guard.as_ref().and_then(|m| m.get(&id))?;
    Rsa::sign_none(&key_pair.private_key, data)
}

/// `NONEwithRSA` verify, by `crypto_impl` key handle. Same `None`-is-a-refusal
/// contract as [`rsa_verify`].
pub fn rsa_verify_none(id: u64, data: &[u8], signature: &[u8]) -> Option<bool> {
    let guard = RSA_KEY_STORE.read();
    let key_pair = guard.as_ref().and_then(|m| m.get(&id))?;
    Rsa::verify_none(&key_pair.public_key, data, signature)
}

/// Maps a *real* RSA key object's GC-stable `identityHashCode` to its
/// `crypto_impl` `key_id`, so CratonVM's `Signature` natives keep using the
/// fast Rust sign/verify even when `route_rsa_to_real()` hands out genuine
/// `sun.security.rsa.RSAPublic/PrivateKeyImpl` objects (which carry no synthetic
/// `key_id` slot). This is the bridge that lets us keep BOTH optimizations —
/// fast Rust keygen AND fast Rust sign/verify — while returning spec-correct key
/// objects. Same GC-stable-identity precedent as the signature payload table.
///
/// VM scope: an `identityHashCode` is unique only *within one heap*, but this
/// table is `static`. Rust tests (and any embedder) create several independent
/// `Vm`s in one process, so without a VM component VM B's real RSA key whose
/// identity hash happens to equal an entry VM A registered would resolve to VM
/// A's `crypto_impl` handle - and `Signature.sign()`/`verify()`
/// (`jca::signature::extract_key_id_from_key`) plus `Cipher`'s RSA component
/// lookup (`jca::cipher::rsa_key_components`) would silently use the WRONG KEY.
/// That is a *correctness* failure, not a crash: the table holds `u64` handles,
/// never `ObjectRef`s, so nothing dangles - the signature is simply made with
/// another VM's key. `NativeContext::vm_identity`'s own doc states the rule
/// ("Native side caches ... must scope entries to this value",
/// `native-api/src/registry.rs`); the same omission in native-collections'
/// `widened_obj_key` aliased two VMs' collections and aborted the process.
/// Keys are therefore `(vm_identity, identity_hash_code)` - the shape already
/// used by `jca::signature`'s `SigKey` and `jca::key_factory`'s `KpgObjKey`.
///
/// GC: no `ObjectRef` is stored (key = a pair of integers, value = a `u64`
/// `crypto_impl` handle), and `identity_hash_code` is preserved across moving
/// collection (`HashCodeTable::update_after_gc`, `gc/src/compact_header.rs`),
/// so this table needs no collector scan/remap companion.
static RSA_REALKEY_MAP: parking_lot::RwLock<Option<HashMap<RsaRealKeyKey, u64>>> =
    parking_lot::RwLock::new(None);

/// `(NativeContext::vm_identity(), identity_hash_code(key))`.
type RsaRealKeyKey = (usize, i32);

/// Register `key_id` for the real RSA key whose identity hash is
/// `identity_hash`, inside VM `vm` (`NativeContext::vm_identity()`).
pub fn rsa_realkey_map_set(vm: usize, identity_hash: i32, key_id: u64) {
    let mut guard = RSA_REALKEY_MAP.write();
    guard
        .get_or_insert_with(HashMap::new)
        .insert((vm, identity_hash), key_id);
}

/// Look up the `crypto_impl` key handle registered for `identity_hash` **in VM
/// `vm`**. Never pass a bare identity hash from one VM to look up another's.
pub fn rsa_realkey_map_get(vm: usize, identity_hash: i32) -> Option<u64> {
    let guard = RSA_REALKEY_MAP.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&(vm, identity_hash)).copied())
}

/// Global ECDSA key store.
static ECDSA_KEY_STORE: parking_lot::RwLock<Option<HashMap<u64, EcdsaKeyPairData>>> =
    parking_lot::RwLock::new(None);

static ECDSA_KEY_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct EcdsaKeyPairData {
    pub public_key: EcdsaPublicKey,
    pub private_key: EcdsaPrivateKey,
}

pub fn ecdsa_key_next_id() -> u64 {
    ECDSA_KEY_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub fn ecdsa_key_store(id: u64, data: EcdsaKeyPairData) {
    let mut guard = ECDSA_KEY_STORE.write();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(id, data);
}

pub fn ecdsa_sign(id: u64, message: &[u8]) -> Option<Vec<u8>> {
    let guard = ECDSA_KEY_STORE.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&id))
        .map(|kp| Ecdsa::sign_sha384(&kp.private_key, message))
}

pub fn ecdsa_verify(id: u64, message: &[u8], signature: &[u8]) -> Option<bool> {
    let guard = ECDSA_KEY_STORE.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&id))
        .map(|kp| Ecdsa::verify_sha384(&kp.public_key, message, signature))
}

/// ECDSA sign with SHA-256 (the JCA algorithm `SHA256withECDSA`).  Used by
/// the WP6.4 `Signature` natives in `jca::signature` to handle the most
/// common P-256 signing variant.
pub fn ecdsa_sign_sha256(id: u64, message: &[u8]) -> Option<Vec<u8>> {
    let guard = ECDSA_KEY_STORE.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&id))
        .map(|kp| Ecdsa::sign_sha256(&kp.private_key, message))
}

pub fn ecdsa_verify_sha256(id: u64, message: &[u8], signature: &[u8]) -> Option<bool> {
    let guard = ECDSA_KEY_STORE.read();
    guard
        .as_ref()
        .and_then(|m| m.get(&id))
        .map(|kp| Ecdsa::verify_sha256(&kp.public_key, message, signature))
}

// ---------------------------------------------------------------------------
// G61 — Ed25519 KeyPairGenerator + Signature (T2.6.8 / T2.6.9)
// ---------------------------------------------------------------------------

/// Ed25519 key pair wrapper.
pub struct Ed25519KeyPairData {
    pub signing_key: ed25519_dalek::SigningKey,
}

static ED25519_KEY_STORE: parking_lot::RwLock<Option<HashMap<u64, Ed25519KeyPairData>>> =
    parking_lot::RwLock::new(None);

static ED25519_KEY_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn ed25519_key_next_id() -> u64 {
    ED25519_KEY_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub fn ed25519_key_store(id: u64, data: Ed25519KeyPairData) {
    let mut guard = ED25519_KEY_STORE.write();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(id, data);
}

/// Generate an Ed25519 key pair. Returns (public_key_bytes_32, key_store_id).
pub fn ed25519_generate_keypair() -> (Vec<u8>, u64) {
    use ed25519_dalek::SigningKey;
    // Generate 32 bytes of entropy from the OS CSPRNG for the Ed25519 seed.
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).expect("OS CSPRNG failed");
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = signing_key.verifying_key();
    let pk_bytes = verifying_key.to_bytes().to_vec();
    let id = ed25519_key_next_id();
    ed25519_key_store(id, Ed25519KeyPairData { signing_key });
    (pk_bytes, id)
}

/// Sign a message with an Ed25519 key.
pub fn ed25519_sign(id: u64, message: &[u8]) -> Option<Vec<u8>> {
    use ed25519_dalek::Signer;
    let guard = ED25519_KEY_STORE.read();
    guard.as_ref().and_then(|m| m.get(&id)).map(|kp| {
        let sig = kp.signing_key.sign(message);
        sig.to_bytes().to_vec()
    })
}

/// Verify an Ed25519 signature.
pub fn ed25519_verify(id: u64, message: &[u8], signature: &[u8]) -> Option<bool> {
    use ed25519_dalek::{Signature, Verifier};
    let guard = ED25519_KEY_STORE.read();
    guard.as_ref().and_then(|m| m.get(&id)).map(|kp| {
        if signature.len() != 64 {
            return false;
        }
        let sig = match Signature::from_bytes(signature.try_into().unwrap_or(&[0u8; 64])) {
            sig => sig,
        };
        kp.signing_key.verifying_key().verify(message, &sig).is_ok()
    })
}

// ---------------------------------------------------------------------------
// C18: GC-stable Signature payload store
//
// `jca::signature` accumulates each `Signature.update(...)` call's bytes here
// so `sign()` / `verify()` can recover them later.  The original key type
// was `u64` — the receiver's raw heap pointer (`this.as_ptr() as u64`).
// Moving GCs (compaction, class-unloading) relocate `Signature` instances;
// their post-move pointer no longer hashes to the same bucket, so the stored
// payload is silently orphaned and `sign()` returns a signature over the
// empty string.  Same defect class as C12-C15
// (`securerandom::SEED_TABLE`, `jca::cipher::CIPHER_TABLE`,
// `jca::message_digest::accumulators()`, `jca::signature`'s sibling
// algo/state/keyid tables) and the canonical exemplar at
// `lang_invoke::VH_META_TABLE` (`native-builtins/src/lang_invoke.rs:178-203`).
//
// Fix: key on `NativeContext::identity_hash_code(this)` which is GC-stable
// (`HashCodeTable::update_after_gc`, `gc/src/compact_header.rs`).  The
// canonical surface is the new `sig_data_append_h` / `sig_data_take_h` /
// `sig_data_clear_h` taking `i32`, backed by a `HashMap<i32, Vec<u8>>`.
//
// The original `u64`-keyed `sig_data_append` / `_take` / `_clear` are
// retained for `crypto.rs` (the `legacy-synthetic-crypto`-feature shim)
// whose call sites this task does not have edit authority over.  Those
// entries live in a *separate* map (`SIG_DATA_STORE_RAW_PTR`) so a
// truncated raw pointer cannot alias a real identity-hash-code key in
// the canonical store.  They retain the original GC-aliasing defect;
// the `crypto.rs` synthetic-crypto path should migrate to the `_h` API
// in a follow-up.  (The legacy functions are deliberately *not* marked
// `#[deprecated]` because CI builds with `-D warnings`, which would
// promote the deprecation lint at the `crypto.rs` call sites into
// build errors.)
// ---------------------------------------------------------------------------

/// GC-stable signature-payload store: keyed on `identity_hash_code(this)`.
///
/// **VM-UNSCOPED — DO NOT WIRE UP AGAIN AS-IS.** This store and the six
/// `sig_data_*` functions below currently have ZERO callers anywhere in the
/// workspace: `jca::signature` moved the payload into its own
/// `sig_payload_table`, keyed `(vm_identity, identity_hash_code)`, and the
/// `crypto.rs` legacy-synthetic shim that used the raw-pointer API no longer
/// exists. The defect is therefore inert, not fixed: the key here is a bare
/// identity hash, which is unique only *within one heap*, while the table is a
/// process-global `static`. Two `Vm`s in one process would append to — and
/// `take` — each other's buffers. Any new caller MUST first re-key this on
/// `(NativeContext::vm_identity(), identity_hash_code(this))`, exactly like
/// `RSA_REALKEY_MAP` above and `jca::signature::SigKey`; prefer deleting the
/// whole block instead.
static SIG_DATA_STORE: parking_lot::RwLock<Option<HashMap<i32, Vec<u8>>>> =
    parking_lot::RwLock::new(None);

/// Append `data` to the payload accumulated against `key`
/// (the receiver's identity hash code).
pub fn sig_data_append_h(key: i32, data: &[u8]) {
    let mut guard = SIG_DATA_STORE.write();
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(key)
        .or_insert_with(Vec::new)
        .extend_from_slice(data);
}

/// Remove and return the payload accumulated against `key`.  Returns
/// `None` when no payload exists for the key — callers that treat the
/// absence as "the receiver was never initialised here" should raise a
/// loud `IllegalStateException` rather than substituting an empty buffer
/// (which would silently produce a signature over `b""`).
pub fn sig_data_take_h(key: i32) -> Option<Vec<u8>> {
    let mut guard = SIG_DATA_STORE.write();
    guard.as_mut().and_then(|m| m.remove(&key))
}

/// Reset the payload for `key` to an empty buffer (replacing any prior
/// accumulation).  Use from `init*` paths so a subsequent `sig_data_take_h`
/// can distinguish "no `update()` was called" (returns `Some(empty)`) from
/// "the side-table entry was orphaned post-GC or `init*` was never
/// invoked" (returns `None` — caller raises `IllegalStateException`).
pub fn sig_data_clear_h(key: i32) {
    let mut guard = SIG_DATA_STORE.write();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(key, Vec::new());
}

// ---------------------------------------------------------------------------
// Legacy raw-pointer-keyed API (retained for `crypto.rs`)
//
// The `u64`-keyed surface below keeps the `legacy-synthetic-crypto`-gated
// `crypto.rs::register_signature` shim compiling — that file's ~12 call
// sites are out of edit scope for C18 (and would need an orchestrator
// follow-up to migrate to the `_h` API).  These functions are NOT marked
// `#[deprecated]` because the project's CI builds with `-D warnings`;
// the deprecation warnings would turn the legacy callers into build
// errors.  Treat the unsuffixed API as soft-deprecated: it inherits the
// pre-C18 GC-aliasing defect and new code should use `sig_data_*_h(i32)`
// keyed on `NativeContext::identity_hash_code(this)`.  Entries live in a
// *separate* map so a truncated raw pointer cannot alias an
// identity-hash-code key in the canonical `SIG_DATA_STORE`.
// ---------------------------------------------------------------------------

static SIG_DATA_STORE_RAW_PTR: parking_lot::RwLock<Option<HashMap<u64, Vec<u8>>>> =
    parking_lot::RwLock::new(None);

/// Legacy raw-pointer-keyed append.  Prefer `sig_data_append_h(i32, ...)`.
pub fn sig_data_append(id: u64, data: &[u8]) {
    let mut guard = SIG_DATA_STORE_RAW_PTR.write();
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(id)
        .or_insert_with(Vec::new)
        .extend_from_slice(data);
}

/// Legacy raw-pointer-keyed take.  Prefer `sig_data_take_h(i32)`.
pub fn sig_data_take(id: u64) -> Vec<u8> {
    let mut guard = SIG_DATA_STORE_RAW_PTR.write();
    guard
        .as_mut()
        .and_then(|m| m.remove(&id))
        .unwrap_or_default()
}

/// Legacy raw-pointer-keyed clear.  Prefer `sig_data_clear_h(i32)`.
pub fn sig_data_clear(id: u64) {
    let mut guard = SIG_DATA_STORE_RAW_PTR.write();
    if let Some(m) = guard.as_mut() {
        m.remove(&id);
    }
}

impl KeyStoreData {
    /// Parse a JKS (Java KeyStore) file.
    pub fn load_jks(data: &[u8], _password: &[u8]) -> Result<Self, CryptoError> {
        if data.len() < 12 {
            return Err(CryptoError::UnsupportedAlgorithm(
                "JKS data too short".into(),
            ));
        }
        // Magic: 0xFEEDFEED
        let magic = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if magic != 0xFEEDFEED {
            return Err(CryptoError::UnsupportedAlgorithm("not a JKS file".into()));
        }
        let _version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let entry_count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;

        let mut entries = HashMap::new();
        let mut pos = 12;

        for _ in 0..entry_count {
            if pos + 4 > data.len() {
                break;
            }
            let tag = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
            pos += 4;

            // Read alias (2-byte length + UTF-16BE)
            if pos + 2 > data.len() {
                break;
            }
            let alias_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + alias_len * 2 > data.len() {
                break;
            }
            let alias: String = (0..alias_len)
                .filter_map(|i| {
                    let c = u16::from_be_bytes([data[pos + i * 2], data[pos + i * 2 + 1]]);
                    char::from_u32(c as u32)
                })
                .collect();
            pos += alias_len * 2;

            // Timestamp (8 bytes)
            if pos + 8 > data.len() {
                break;
            }
            pos += 8;

            match tag {
                2 => {
                    // Trusted cert entry
                    // cert type (2-byte length + string)
                    if pos + 2 > data.len() {
                        break;
                    }
                    let ct_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                    pos += 2;
                    if pos + ct_len > data.len() {
                        break;
                    }
                    pos += ct_len; // skip cert type string

                    // cert data (4-byte length + DER)
                    if pos + 4 > data.len() {
                        break;
                    }
                    let cert_data_len = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]) as usize;
                    pos += 4;
                    if pos + cert_data_len > data.len() {
                        break;
                    }
                    let cert_bytes = &data[pos..pos + cert_data_len];
                    pos += cert_data_len;

                    if let Ok(cert) = X509Cert::parse_der(cert_bytes) {
                        entries.insert(alias, KeyStoreEntry::TrustedCert { cert });
                    }
                }
                1 => {
                    // Private key entry
                    // key data (4-byte length + encrypted key)
                    if pos + 4 > data.len() {
                        break;
                    }
                    let key_data_len = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]) as usize;
                    pos += 4;
                    if pos + key_data_len > data.len() {
                        break;
                    }
                    let key_bytes = data[pos..pos + key_data_len].to_vec();
                    pos += key_data_len;

                    // cert chain count (4 bytes)
                    if pos + 4 > data.len() {
                        break;
                    }
                    let chain_count = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]) as usize;
                    pos += 4;

                    let mut chain = Vec::new();
                    for _ in 0..chain_count {
                        if pos + 2 > data.len() {
                            break;
                        }
                        let ct_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                        pos += 2;
                        if pos + ct_len > data.len() {
                            break;
                        }
                        pos += ct_len;

                        if pos + 4 > data.len() {
                            break;
                        }
                        let cd_len = u32::from_be_bytes([
                            data[pos],
                            data[pos + 1],
                            data[pos + 2],
                            data[pos + 3],
                        ]) as usize;
                        pos += 4;
                        if pos + cd_len > data.len() {
                            break;
                        }
                        if let Ok(cert) = X509Cert::parse_der(&data[pos..pos + cd_len]) {
                            chain.push(cert);
                        }
                        pos += cd_len;
                    }

                    entries.insert(
                        alias,
                        KeyStoreEntry::PrivateKeyEntry {
                            key_bytes,
                            cert_chain: chain,
                        },
                    );
                }
                _ => break,
            }
        }

        Ok(KeyStoreData {
            store_type: "JKS".into(),
            entries,
        })
    }

    /// Parse a PKCS#12 file (simplified — handles common structures).
    pub fn load_pkcs12(data: &[u8], _password: &[u8]) -> Result<Self, CryptoError> {
        if data.len() < 4 || data[0] != 0x30 {
            return Err(CryptoError::UnsupportedAlgorithm(
                "not a PKCS#12 file".into(),
            ));
        }

        // PFX: SEQUENCE { INTEGER version, SEQUENCE authSafe, [0] macData }
        let (_, pfx_content) = der_read_tag_length(data)
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm("invalid PFX".into()))?;

        let mut entries = HashMap::new();
        // Walk through and extract any certificates we find
        extract_certs_from_der(pfx_content, &mut entries, 0);

        Ok(KeyStoreData {
            store_type: "PKCS12".into(),
            entries,
        })
    }

    /// Load from raw bytes, auto-detecting format.
    pub fn load(data: &[u8], password: &[u8], type_hint: &str) -> Result<Self, CryptoError> {
        match type_hint {
            "JKS" | "jks" => Self::load_jks(data, password),
            "PKCS12" | "pkcs12" | "p12" => Self::load_pkcs12(data, password),
            _ => {
                // Try JKS first (check magic), then PKCS12
                if data.len() >= 4 && data[0..4] == [0xFE, 0xED, 0xFE, 0xED] {
                    Self::load_jks(data, password)
                } else {
                    Self::load_pkcs12(data, password)
                }
            }
        }
    }
}

/// Recursively walk DER structures looking for X.509 certificates.
fn extract_certs_from_der(data: &[u8], entries: &mut HashMap<String, KeyStoreEntry>, depth: usize) {
    if depth > 20 || data.len() < 2 {
        return;
    }

    let mut pos = 0;
    while pos < data.len() {
        if data.len() - pos < 2 {
            break;
        }
        let tag = data[pos];

        match der_read_tag_length(&data[pos..]) {
            Some((total_len, content)) => {
                // Try to parse as X.509 certificate
                if tag == 0x30 && content.len() > 10 {
                    if let Ok(cert) = X509Cert::parse_der(&data[pos..pos + total_len]) {
                        let alias = if cert.subject_cn.is_empty() {
                            format!("cert_{}", entries.len())
                        } else {
                            cert.subject_cn.clone()
                        };
                        entries.insert(alias, KeyStoreEntry::TrustedCert { cert });
                    } else {
                        // Recurse into sequences
                        if tag == 0x30 || tag == 0xa0 || tag == 0xa1 {
                            extract_certs_from_der(content, entries, depth + 1);
                        }
                    }
                } else if tag == 0x30 || tag == 0xa0 || tag == 0xa1 {
                    extract_certs_from_der(content, entries, depth + 1);
                }

                // OCTET STRING might contain nested DER
                if tag == 0x04 && content.len() > 4 {
                    extract_certs_from_der(content, entries, depth + 1);
                }

                pos += total_len;
            }
            None => break,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }

    fn from_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// This registrar must claim `java/security/SecureRandom` for exactly two
    /// triples, and it must NOT claim either `setSeed` or the seeded ctor.
    ///
    /// L8-securerandom-provider.md. It used to register five, and the extra
    /// three were shape-checking no-ops. In `--synthetic-jdk` this registrar is
    /// called from `register_synthetic_overrides` AFTER
    /// `securerandom::register_random_and_securerandom_natives`, and
    /// `register()` is last-registration-wins, so the no-ops silently replaced
    /// two working fixes: `securerandom.rs`'s `setSeed` bodies check
    /// `secure_random_is_sha1prng` and route SHA1PRNG through real reseeding
    /// (`getInstance("SHA1PRNG")` seeded twice alike yields identical bytes on
    /// HotSpot — the one replay guarantee the JDK gives a `SecureRandom`), and
    /// its `<init>([B)V` stamps the `algorithm` and `provider` fields whose
    /// absence is this record's headline defect.
    ///
    /// Asserted as a REGISTRATION census rather than a behavioural check
    /// because the defect is a registration: the no-op bodies were each
    /// individually defensible, and what made them wrong was which triple they
    /// claimed and in what order. A behavioural test would also need a
    /// `--synthetic-jdk` VM, which no scheduled corpus run builds.
    #[test]
    fn crypto_impl_registers_no_securerandom_seeding_triple() {
        let mut r = NativeMethodRegistry::new();
        register_crypto_impl_natives(&mut r);
        let dump = r.dump_registrations();
        let mut mine: Vec<String> = Vec::new();
        for row in dump.iter() {
            if row.0 == "java/security/SecureRandom" {
                mine.push(format!("{}{}", row.1, row.2));
            }
        }
        assert_eq!(
            mine,
            vec![
                "nextBytes([B)V".to_string(),
                "generateSeed(I)[B".to_string()
            ],
            "crypto_impl must own only the two OS-CSPRNG output triples; \
             re-registering setSeed or <init>([B)V here shadows securerandom.rs \
             in synthetic mode and undoes SHA1PRNG reseeding — L8"
        );
        // Stated separately so a future widening of the list above cannot
        // quietly re-admit the two rows this record is about.
        for row in dump.iter() {
            assert!(
                !(row.0 == "java/security/SecureRandom"
                    && (row.1 == "setSeed" || row.1 == "<init>")),
                "SecureRandom.setSeed / <init> must be served by securerandom.rs alone"
            );
        }
    }

    // -----------------------------------------------------------------------
    // SHA-256 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn sha256_empty() {
        let h = Sha256::digest(b"");
        assert_eq!(
            hex(&h),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_abc() {
        let h = Sha256::digest(b"abc");
        assert_eq!(
            hex(&h),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_longer_message() {
        let h = Sha256::digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            hex(&h),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_incremental() {
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        hasher.update(b"def");
        let h = hasher.finalize();
        assert_eq!(hex(&h), hex(&Sha256::digest(b"abcdef")));
    }

    #[test]
    fn sha256_single_byte() {
        let h = Sha256::digest(b"a");
        assert_eq!(
            hex(&h),
            "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"
        );
    }

    #[test]
    fn sha256_56_bytes() {
        // Exactly the boundary where padding fits in one block
        let data = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnop";
        let h = Sha256::digest(data);
        // Just verify it produces 32 bytes
        assert_eq!(h.len(), 32);
    }

    #[test]
    fn sha256_multi_block() {
        // 128 bytes = 2 blocks exactly
        let data = vec![0x61u8; 128];
        let h = Sha256::digest(&data);
        assert_eq!(h.len(), 32);
    }

    // -----------------------------------------------------------------------
    // SHA-512 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn sha512_empty() {
        let h = Sha512::digest(b"");
        assert_eq!(
            hex(&h),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
    }

    #[test]
    fn sha512_abc() {
        let h = Sha512::digest(b"abc");
        assert_eq!(
            hex(&h),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn sha512_longer() {
        let h = Sha512::digest(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu");
        assert_eq!(
            hex(&h),
            "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
        );
    }

    // -----------------------------------------------------------------------
    // SHA-384 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn sha384_empty() {
        let h = Sha384::digest(b"");
        assert_eq!(
            hex(&h),
            "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b"
        );
    }

    #[test]
    fn sha384_abc() {
        let h = Sha384::digest(b"abc");
        assert_eq!(
            hex(&h),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
        );
    }

    // -----------------------------------------------------------------------
    // AES Key Expansion Tests
    // -----------------------------------------------------------------------

    #[test]
    fn aes_key_expansion_128() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        assert_eq!(aes_key.nr, 10);
        assert!(matches!(aes_key.cipher, AesCipher::Aes128(_)));
    }

    #[test]
    fn aes_key_expansion_192() {
        let key = from_hex("8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b");
        let aes_key = Aes::key_expansion(&key).unwrap();
        assert_eq!(aes_key.nr, 12);
        assert!(matches!(aes_key.cipher, AesCipher::Aes192(_)));
    }

    #[test]
    fn aes_key_expansion_256() {
        let key = from_hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
        let aes_key = Aes::key_expansion(&key).unwrap();
        assert_eq!(aes_key.nr, 14);
        assert!(matches!(aes_key.cipher, AesCipher::Aes256(_)));
    }

    #[test]
    fn aes_key_expansion_invalid() {
        assert!(Aes::key_expansion(&[0u8; 15]).is_err());
        assert!(Aes::key_expansion(&[0u8; 17]).is_err());
    }

    // -----------------------------------------------------------------------
    // AES-128 Encrypt/Decrypt Block Tests (FIPS 197 Appendix B)
    // -----------------------------------------------------------------------

    #[test]
    fn aes128_encrypt_block() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let plaintext = from_hex("3243f6a8885a308d313198a2e0370734");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let mut input = [0u8; 16];
        input.copy_from_slice(&plaintext);
        let ct = Aes::encrypt_block(&aes_key, &input);
        assert_eq!(hex(&ct), "3925841d02dc09fbdc118597196a0b32");
    }

    #[test]
    fn aes128_decrypt_block() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let ciphertext = from_hex("3925841d02dc09fbdc118597196a0b32");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let mut input = [0u8; 16];
        input.copy_from_slice(&ciphertext);
        let pt = Aes::decrypt_block(&aes_key, &input);
        assert_eq!(hex(&pt), "3243f6a8885a308d313198a2e0370734");
    }

    #[test]
    fn aes128_encrypt_decrypt_roundtrip() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let plaintext = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let ct = Aes::encrypt_block(&aes_key, &plaintext);
        let pt = Aes::decrypt_block(&aes_key, &ct);
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn aes256_encrypt_decrypt_roundtrip() {
        let key = from_hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let plaintext = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        let ct = Aes::encrypt_block(&aes_key, &plaintext);
        let pt = Aes::decrypt_block(&aes_key, &ct);
        assert_eq!(pt, plaintext);
    }

    // -----------------------------------------------------------------------
    // AES ECB Mode Tests
    // -----------------------------------------------------------------------

    #[test]
    fn aes_ecb_encrypt_decrypt() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let plaintext = b"Hello, World!!!!"; // exactly 16 bytes
        let ct = AesEcb::encrypt(&aes_key, plaintext);
        let pt = AesEcb::decrypt(&aes_key, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn aes_ecb_empty() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let ct = AesEcb::encrypt(&aes_key, b"");
        // Empty input produces one block of padding
        assert_eq!(ct.len(), 16);
        let pt = AesEcb::decrypt(&aes_key, &ct).unwrap();
        assert_eq!(pt, b"");
    }

    #[test]
    fn aes_ecb_multi_block() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let plaintext = b"This is a test of multi-block ECB encryption!";
        let ct = AesEcb::encrypt(&aes_key, plaintext);
        let pt = AesEcb::decrypt(&aes_key, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn aes_ecb_invalid_ciphertext_length() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        assert!(AesEcb::decrypt(&aes_key, &[0u8; 15]).is_err());
    }

    // -----------------------------------------------------------------------
    // AES CBC Mode Tests
    // -----------------------------------------------------------------------

    #[test]
    fn aes_cbc_encrypt_decrypt() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = from_hex("000102030405060708090a0b0c0d0e0f");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let mut iv_arr = [0u8; 16];
        iv_arr.copy_from_slice(&iv);
        let plaintext = b"Hello CBC Mode!!"; // 16 bytes
        let ct = AesCbc::encrypt(&aes_key, &iv_arr, plaintext);
        let pt = AesCbc::decrypt(&aes_key, &iv_arr, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn aes_cbc_multi_block() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let iv = [0u8; 16];
        let plaintext =
            b"This is a longer message that spans multiple AES blocks for CBC mode testing.";
        let ct = AesCbc::encrypt(&aes_key, &iv, plaintext);
        let pt = AesCbc::decrypt(&aes_key, &iv, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn aes_cbc_empty() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let iv = [0u8; 16];
        let ct = AesCbc::encrypt(&aes_key, &iv, b"");
        let pt = AesCbc::decrypt(&aes_key, &iv, &ct).unwrap();
        assert_eq!(pt, b"");
    }

    #[test]
    fn aes_cbc_invalid_ciphertext() {
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let iv = [0u8; 16];
        assert!(AesCbc::decrypt(&aes_key, &iv, &[0u8; 7]).is_err());
    }

    // -----------------------------------------------------------------------
    // AES-GCM Tests
    // -----------------------------------------------------------------------

    #[test]
    fn aes_gcm_encrypt_decrypt_roundtrip() {
        let key = from_hex("00000000000000000000000000000000");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let nonce = [0u8; 12];
        let plaintext = b"Hello, GCM!";
        let aad = b"additional data";
        let output = AesGcm::encrypt(&aes_key, &nonce, plaintext, aad);
        let pt = AesGcm::decrypt(&aes_key, &nonce, &output.ciphertext, aad, &output.tag).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn aes_gcm_empty_plaintext() {
        let key = from_hex("00000000000000000000000000000000");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let nonce = [0u8; 12];
        let output = AesGcm::encrypt(&aes_key, &nonce, b"", b"");
        assert!(output.ciphertext.is_empty());
        let pt = AesGcm::decrypt(&aes_key, &nonce, &output.ciphertext, b"", &output.tag).unwrap();
        assert!(pt.is_empty());
    }

    #[test]
    fn aes_gcm_auth_failure() {
        let key = from_hex("00000000000000000000000000000000");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let nonce = [0u8; 12];
        let output = AesGcm::encrypt(&aes_key, &nonce, b"test", b"aad");
        // Tamper with tag
        let mut bad_tag = output.tag;
        bad_tag[0] ^= 0xff;
        assert!(AesGcm::decrypt(&aes_key, &nonce, &output.ciphertext, b"aad", &bad_tag).is_err());
    }

    #[test]
    fn aes_gcm_wrong_aad() {
        let key = from_hex("00000000000000000000000000000000");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let nonce = [0u8; 12];
        let output = AesGcm::encrypt(&aes_key, &nonce, b"test", b"correct aad");
        assert!(AesGcm::decrypt(
            &aes_key,
            &nonce,
            &output.ciphertext,
            b"wrong aad",
            &output.tag
        )
        .is_err());
    }

    #[test]
    fn aes_gcm_tampered_ciphertext() {
        let key = from_hex("00000000000000000000000000000000");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let nonce = [0u8; 12];
        let output = AesGcm::encrypt(&aes_key, &nonce, b"secret data", b"");
        let mut bad_ct = output.ciphertext.clone();
        if !bad_ct.is_empty() {
            bad_ct[0] ^= 0xff;
        }
        assert!(AesGcm::decrypt(&aes_key, &nonce, &bad_ct, b"", &output.tag).is_err());
    }

    #[test]
    fn aes_gcm_large_plaintext() {
        let key = from_hex("feffe9928665731c6d6a8f9467308308");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let nonce = from_hex("cafebabefacedbaddecaf888");
        let mut nonce_arr = [0u8; 12];
        nonce_arr.copy_from_slice(&nonce);
        let plaintext = vec![0xabu8; 256];
        let aad = b"test aad";
        let output = AesGcm::encrypt(&aes_key, &nonce_arr, &plaintext, aad);
        let pt =
            AesGcm::decrypt(&aes_key, &nonce_arr, &output.ciphertext, aad, &output.tag).unwrap();
        assert_eq!(pt, plaintext);
    }
    // -----------------------------------------------------------------------
    // C18 — NIST SP 800-38D AES-128-GCM Known-Answer Tests
    //
    // Vectors lifted from NIST SP 800-38D Appendix B, Test Case 3 —
    // the canonical AES-128-GCM KAT also reproduced in RFC 5288 and
    // every published GCM reference implementation. Proves the
    // RustCrypto-backed `AesGcm::{encrypt, decrypt}` matches the
    // bit-for-bit ciphertext + tag, and that a tampered tag is
    // rejected with `AuthenticationFailed`.
    // -----------------------------------------------------------------------

    #[test]
    fn c18_nist_aes128_gcm_test_case_3_encrypt() {
        // K  = feffe9928665731c6d6a8f9467308308
        // P  = d9313225..f391aafd255 (64 bytes)
        // IV = cafebabefacedbaddecaf888 (12 bytes)
        // A  = (empty)
        // C  = 42831ec2..73f5985
        // T  = 4d5c2af327cd64a62cf35abd2ba6fab4
        let key = from_hex("feffe9928665731c6d6a8f9467308308");
        let iv = from_hex("cafebabefacedbaddecaf888");
        let pt = from_hex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da\
             2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525\
             b16aedf5aa0de657ba637b391aafd255",
        );
        let aad: Vec<u8> = Vec::new();
        let expected_ct = from_hex(
            "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e0\
             35c17e2329aca12e21d514b25466931c7d8f6a5aac84aa05\
             1ba30b396a0aac973d58e091473f5985",
        );
        let expected_tag = from_hex("4d5c2af327cd64a62cf35abd2ba6fab4");

        let aes_key = Aes::key_expansion(&key).unwrap();
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&iv);
        let out = AesGcm::encrypt(&aes_key, &nonce, &pt, &aad);
        assert_eq!(
            hex(&out.ciphertext),
            hex(&expected_ct),
            "AES-128-GCM ciphertext"
        );
        assert_eq!(hex(&out.tag), hex(&expected_tag), "AES-128-GCM tag");
    }

    #[test]
    fn c18_nist_aes128_gcm_test_case_3_decrypt_and_auth_fail() {
        // Same vector — decrypt direction, plus an explicit
        // AuthenticationFailed proof on a tampered tag.
        let key = from_hex("feffe9928665731c6d6a8f9467308308");
        let iv = from_hex("cafebabefacedbaddecaf888");
        let ct = from_hex(
            "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e0\
             35c17e2329aca12e21d514b25466931c7d8f6a5aac84aa05\
             1ba30b396a0aac973d58e091473f5985",
        );
        let aad: Vec<u8> = Vec::new();
        let tag_bytes = from_hex("4d5c2af327cd64a62cf35abd2ba6fab4");
        let expected_pt = from_hex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da\
             2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525\
             b16aedf5aa0de657ba637b391aafd255",
        );

        let aes_key = Aes::key_expansion(&key).unwrap();
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&iv);
        let mut tag = [0u8; 16];
        tag.copy_from_slice(&tag_bytes);

        let pt = AesGcm::decrypt(&aes_key, &nonce, &ct, &aad, &tag)
            .expect("NIST Test Case 3 decrypt must succeed");
        assert_eq!(hex(&pt), hex(&expected_pt), "AES-128-GCM plaintext");

        // Tamper the high bit of the tag — auth must fail.
        let mut bad_tag = tag;
        bad_tag[0] ^= 0x80;
        let res = AesGcm::decrypt(&aes_key, &nonce, &ct, &aad, &bad_tag);
        assert!(
            matches!(res, Err(CryptoError::AuthenticationFailed)),
            "AES-128-GCM with tampered tag must return AuthenticationFailed, got {:?}",
            res
        );
    }

    // -----------------------------------------------------------------------
    // HMAC Tests (RFC 4231 test vectors)
    // -----------------------------------------------------------------------

    #[test]
    fn hmac_sha256_test_vector_1() {
        // RFC 4231 Test Case 1
        let key = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let data = b"Hi There";
        let mac = Hmac::mac(&key, data, HashFunction::Sha256);
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_sha256_test_vector_2() {
        // RFC 4231 Test Case 2 — key = "Jefe"
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let mac = Hmac::mac(key, data, HashFunction::Sha256);
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_sha256_test_vector_3() {
        // RFC 4231 Test Case 3
        let key = from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let data = from_hex("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");
        let mac = Hmac::mac(&key, &data, HashFunction::Sha256);
        assert_eq!(
            hex(&mac),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    #[test]
    fn hmac_sha384_test_vector_1() {
        let key = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let data = b"Hi There";
        let mac = Hmac::mac(&key, data, HashFunction::Sha384);
        assert_eq!(
            hex(&mac),
            "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59cfaea9ea9076ede7f4af152e8b2fa9cb6"
        );
    }

    #[test]
    fn hmac_sha512_test_vector_1() {
        let key = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let data = b"Hi There";
        let mac = Hmac::mac(&key, data, HashFunction::Sha512);
        assert_eq!(
            hex(&mac),
            "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
        );
    }

    #[test]
    fn hmac_incremental() {
        let key = b"key";
        let mut h = Hmac::new(key, HashFunction::Sha256);
        h.update(b"hello ");
        h.update(b"world");
        let mac1 = h.finalize();
        let mac2 = Hmac::mac(key, b"hello world", HashFunction::Sha256);
        assert_eq!(mac1, mac2);
    }

    // -----------------------------------------------------------------------
    // HKDF Tests (RFC 5869 test vectors)
    // -----------------------------------------------------------------------

    #[test]
    fn hkdf_sha256_test_vector_1() {
        // RFC 5869 Test Case 1
        let ikm = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = from_hex("000102030405060708090a0b0c");
        let info = from_hex("f0f1f2f3f4f5f6f7f8f9");
        let okm = Hkdf::derive(HashFunction::Sha256, &salt, &ikm, &info, 42);
        assert_eq!(
            hex(&okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn hkdf_sha256_test_vector_2() {
        // RFC 5869 Test Case 2
        let ikm = from_hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f");
        let salt = from_hex("606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeaf");
        let info = from_hex("b0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
        let okm = Hkdf::derive(HashFunction::Sha256, &salt, &ikm, &info, 82);
        assert_eq!(
            hex(&okm),
            "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71cc30c58179ec3e87c14c01d5c1f3434f1d87"
        );
    }

    #[test]
    fn hkdf_sha256_test_vector_3() {
        // RFC 5869 Test Case 3 — zero-length salt and info
        let ikm = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let okm = Hkdf::derive(HashFunction::Sha256, &[], &ikm, &[], 42);
        assert_eq!(
            hex(&okm),
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"
        );
    }

    #[test]
    fn hkdf_extract_expand_separate() {
        let ikm = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = from_hex("000102030405060708090a0b0c");
        let info = from_hex("f0f1f2f3f4f5f6f7f8f9");
        let prk = Hkdf::extract(HashFunction::Sha256, &salt, &ikm);
        let okm = Hkdf::expand(HashFunction::Sha256, &prk, &info, 42);
        let okm_combined = Hkdf::derive(HashFunction::Sha256, &salt, &ikm, &info, 42);
        assert_eq!(okm, okm_combined);
    }

    // -----------------------------------------------------------------------
    // SecureRandom Tests
    // -----------------------------------------------------------------------

    #[test]
    fn secure_random_deterministic() {
        let mut rng1 = SecureRandom::new_with_seed(42);
        let mut rng2 = SecureRandom::new_with_seed(42);
        let mut buf1 = [0u8; 32];
        let mut buf2 = [0u8; 32];
        rng1.next_bytes(&mut buf1);
        rng2.next_bytes(&mut buf2);
        assert_eq!(buf1, buf2);
    }

    #[test]
    fn secure_random_different_seeds() {
        let mut rng1 = SecureRandom::new_with_seed(42);
        let mut rng2 = SecureRandom::new_with_seed(43);
        assert_ne!(rng1.next_u64(), rng2.next_u64());
    }

    #[test]
    fn secure_random_next_u32() {
        let mut rng = SecureRandom::new_with_seed(1);
        let v1 = rng.next_u32();
        let v2 = rng.next_u32();
        assert_ne!(v1, v2);
    }

    #[test]
    fn secure_random_fill_buffer() {
        let mut rng = SecureRandom::new_with_seed(99);
        let mut buf = [0u8; 100];
        rng.next_bytes(&mut buf);
        // Not all zeros
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn secure_random_sequential_not_equal() {
        let mut rng = SecureRandom::new_with_seed(7);
        let v1 = rng.next_u64();
        let v2 = rng.next_u64();
        let v3 = rng.next_u64();
        assert_ne!(v1, v2);
        assert_ne!(v2, v3);
    }

    // VULN(secrand) / VULN(secrand-collision) regression: SecureRandom output now
    // comes straight from the OS CSPRNG and is INDEPENDENT of the `key` argument
    // (the old identity-hash-keyed DRBG side-table — and its collision/aliasing
    // hazard — is gone). Each draw must be fresh regardless of the key.

    #[test]
    fn secure_random_fill_is_fresh_every_draw() {
        // Successive draws (same key) must differ — no cached/replayed state.
        let mut a1 = [0u8; 32];
        let mut a2 = [0u8; 32];
        secure_random_fill(0x1111_1111, &mut a1);
        secure_random_fill(0x1111_1111, &mut a2);
        assert_ne!(a1, a2, "successive draws must be fresh, not cached");
        // Different key value — also fresh, never aliased to the first key.
        let mut b1 = [0u8; 32];
        secure_random_fill(0x2222_2222, &mut b1);
        assert_ne!(a1, b1, "draws must not be correlated across keys");
        // Output must not be all-zero (no degenerate output).
        assert!(a1.iter().any(|&b| b != 0));
    }

    #[test]
    fn secure_random_fill_ignores_key_collisions() {
        // The collision hazard is removed: two draws with the SAME key value
        // (the worst case an identity-hash collision could produce) are still
        // independent fresh OS draws, not a shared/aliased deterministic stream.
        let key = 0x3333_3333;
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        secure_random_fill(key, &mut first);
        secure_random_fill(key, &mut second);
        assert_ne!(
            first, second,
            "same-key draws must remain independent (no aliased DRBG state)"
        );
    }

    // -----------------------------------------------------------------------
    // PKCS7 Padding Tests
    // -----------------------------------------------------------------------

    #[test]
    fn pkcs7_pad_full_block() {
        let padded = pkcs7_pad(b"1234567890123456", 16);
        // 16 bytes input -> 32 bytes with full block of padding
        assert_eq!(padded.len(), 32);
        assert!(padded[16..].iter().all(|&b| b == 16));
    }

    #[test]
    fn pkcs7_pad_partial() {
        let padded = pkcs7_pad(b"hello", 16);
        assert_eq!(padded.len(), 16);
        assert!(padded[5..].iter().all(|&b| b == 11));
    }

    #[test]
    fn pkcs7_unpad_valid() {
        let mut data = b"hello".to_vec();
        data.extend(std::iter::repeat(11u8).take(11));
        let unpadded = pkcs7_unpad(&data).unwrap();
        assert_eq!(unpadded, b"hello");
    }

    #[test]
    fn pkcs7_unpad_invalid() {
        assert!(pkcs7_unpad(&[]).is_err());
        assert!(pkcs7_unpad(&[0u8; 16]).is_err()); // pad byte = 0 is invalid
    }

    // -----------------------------------------------------------------------
    // CryptoError Display Tests
    // -----------------------------------------------------------------------

    #[test]
    fn crypto_error_display() {
        assert!(format!("{}", CryptoError::InvalidKeyLength(7)).contains("7"));
        assert!(format!("{}", CryptoError::InvalidBlockSize).contains("block"));
        assert!(format!("{}", CryptoError::InvalidPadding).contains("padding"));
        assert!(format!("{}", CryptoError::AuthenticationFailed).contains("authentication"));
        assert!(format!("{}", CryptoError::InvalidNonceLength).contains("nonce"));
        assert!(format!("{}", CryptoError::UnsupportedAlgorithm("foo".into())).contains("foo"));
    }

    // -----------------------------------------------------------------------
    // HashFunction Tests
    // -----------------------------------------------------------------------

    #[test]
    fn hash_function_block_sizes() {
        assert_eq!(HashFunction::Sha256.block_size(), 64);
        assert_eq!(HashFunction::Sha384.block_size(), 128);
        assert_eq!(HashFunction::Sha512.block_size(), 128);
    }

    #[test]
    fn hash_function_output_sizes() {
        assert_eq!(HashFunction::Sha256.output_size(), 32);
        assert_eq!(HashFunction::Sha384.output_size(), 48);
        assert_eq!(HashFunction::Sha512.output_size(), 64);
    }

    #[test]
    fn hash_function_hash_sha256() {
        let h = HashFunction::Sha256.hash(b"abc");
        assert_eq!(hex(&h), hex(&Sha256::digest(b"abc")));
    }

    #[test]
    fn hash_function_hash_sha512() {
        let h = HashFunction::Sha512.hash(b"abc");
        assert_eq!(hex(&h), hex(&Sha512::digest(b"abc").to_vec()));
    }

    // -----------------------------------------------------------------------
    // NIST AES-128 ECB Known Answer Test
    // -----------------------------------------------------------------------

    #[test]
    fn aes128_ecb_nist_vector() {
        // NIST SP 800-38A F.1.1 ECB-AES128.Encrypt
        let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let pt = from_hex("6bc1bee22e409f96e93d7e117393172a");
        let mut block = [0u8; 16];
        block.copy_from_slice(&pt);
        let ct = Aes::encrypt_block(&aes_key, &block);
        assert_eq!(hex(&ct), "3ad77bb40d7a3660a89ecaf32466ef97");
    }

    // -----------------------------------------------------------------------
    // Splitmix64 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn splitmix64_deterministic() {
        let mut s1 = 42u64;
        let mut s2 = 42u64;
        assert_eq!(splitmix64(&mut s1), splitmix64(&mut s2));
    }

    // -----------------------------------------------------------------------
    // ChaCha20 keystream (SecureRandom OS-entropy fallback) — RFC 7539 vector
    // -----------------------------------------------------------------------

    #[test]
    fn chacha20_keystream_rfc7539_zero_key() {
        // RFC 7539 §2.3.2 derived: ChaCha20 keystream for an all-zero 256-bit
        // key, all-zero 96-bit nonce, starting block counter 0. This is the
        // canonical "ChaCha20 of zeros" reference output.
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let mut out = [0u8; 64];
        chacha20_keystream_fill(&key, &nonce, &mut out);
        assert_eq!(
            hex(&out),
            "76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7\
             da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586"
        );
    }

    /// The vectors below were MEASURED, not recalled: HotSpot 25's own SunJCE
    /// `Cipher.getInstance("ChaCha20")` was initialised with the stated key,
    /// `ChaCha20ParameterSpec(nonce, counter)`, and asked to encrypt an
    /// all-zero plaintext — which yields the raw keystream. Encrypting zeros is
    /// the only way to read a stream cipher's keystream through the JCA API,
    /// and it makes the oracle's output directly comparable with
    /// `chacha20_xor` over a zero buffer.
    ///
    /// The zero-key vector above (`chacha20_keystream_rfc7539_zero_key`) was
    /// re-derived from the same oracle in passing and matches this tree's
    /// existing expectation byte for byte, which is independent evidence that
    /// the pre-existing core was already correct — what it lacked was a
    /// caller-supplied counter and an XOR, not a working permutation.
    #[test]
    fn chacha20_xor_counter_one_matches_hotspot() {
        // key = 00..1f, nonce = 00 00 00 00 00 00 00 4a 00 00 00 00 (RFC 8439
        // §2.4.2's key and nonce), block counter 1.
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let nonce = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let mut out = [0u8; 128];
        chacha20_xor(&key, &nonce, 1, &mut out);
        assert_eq!(
            hex(&out),
            "224f51f3401bd9e12fde276fb8631ded8c131f823d2c06e27e4fcaec9ef3cf78\
             8a3b0aa372600a92b57974cded2b9334794cba40c63e34cdea212c4cf07d41b7\
             69a6749f3f630f4122cafe28ec4dc47e26d4346d70b98c73f3e9c53ac40c5945\
             398b6eda1a832c89c167eacd901d7e2bf363740373201aa188fbbce83991c4ed"
        );
    }

    /// The counter is a plain block index, and this is the observation that
    /// proves it without trusting either implementation: the same key and nonce
    /// at counter 0 must produce, as its SECOND 64-byte block, exactly what
    /// counter 1 produces as its FIRST. HotSpot's output has this property;
    /// so must ours. A core that ignored `initial_counter` would still pass
    /// the vector test above if its expectation were taken from itself — this
    /// one it could not pass.
    #[test]
    fn chacha20_xor_counter_is_a_block_index() {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let nonce = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let mut from_zero = [0u8; 128];
        chacha20_xor(&key, &nonce, 0, &mut from_zero);
        let mut from_one = [0u8; 64];
        chacha20_xor(&key, &nonce, 1, &mut from_one);
        assert_eq!(&from_zero[64..], &from_one[..]);
        // …and the counter-0 stream is HotSpot's, so neither block is ours alone.
        assert_eq!(
            hex(&from_zero[..64]),
            "af051e40bba0354981329a806a140eafd258a22a6dcb4bb9f6569cb3efe2deaf\
             837bd87ca20b5ba12081a306af0eb35c41a239d20dfc74c81771560d9c9c1e4b"
        );
    }

    /// A length that is not a multiple of the 64-byte block must stop mid-block
    /// and must not touch the bytes past the end. HotSpot's 70-byte answer is
    /// the 128-byte answer truncated, which is the property being pinned.
    #[test]
    fn chacha20_xor_partial_final_block() {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let nonce = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let mut out = [0u8; 70];
        chacha20_xor(&key, &nonce, 1, &mut out);
        assert_eq!(
            hex(&out),
            "224f51f3401bd9e12fde276fb8631ded8c131f823d2c06e27e4fcaec9ef3cf78\
             8a3b0aa372600a92b57974cded2b9334794cba40c63e34cdea212c4cf07d41b7\
             69a6749f3f63"
        );
    }

    /// XOR, not write: running the same keystream over the same buffer twice
    /// must restore the plaintext. This is the property that makes the function
    /// a cipher rather than a generator, and the one the old
    /// `copy_from_slice` body did not have.
    #[test]
    fn chacha20_xor_is_an_involution() {
        let key = [0x5au8; 32];
        let nonce = [0x3cu8; 12];
        let plaintext: Vec<u8> = (0..200u32).map(|i| (i * 31) as u8).collect();
        let mut buf = plaintext.clone();
        chacha20_xor(&key, &nonce, 7, &mut buf);
        assert_ne!(buf, plaintext, "ciphertext must differ from plaintext");
        chacha20_xor(&key, &nonce, 7, &mut buf);
        assert_eq!(buf, plaintext, "decrypting must restore the plaintext");
    }

    #[test]
    fn chacha20_keystream_spans_multiple_blocks() {
        // A request larger than one 64-byte block must keep advancing the
        // counter: the second block differs from the first (no repetition).
        let key = [7u8; 32];
        let nonce = [3u8; 12];
        let mut out = [0u8; 128];
        chacha20_keystream_fill(&key, &nonce, &mut out);
        assert_ne!(&out[..64], &out[64..], "blocks must not repeat");
    }

    // -----------------------------------------------------------------------
    // AES-256 NIST test vectors
    // -----------------------------------------------------------------------

    #[test]
    fn aes256_nist_vector() {
        let key = from_hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let pt = from_hex("6bc1bee22e409f96e93d7e117393172a");
        let mut block = [0u8; 16];
        block.copy_from_slice(&pt);
        let ct = Aes::encrypt_block(&aes_key, &block);
        assert_eq!(hex(&ct), "f3eed1bdb5d2a03c064b5a7e3db181f8");
    }

    // -----------------------------------------------------------------------
    // GCM with AAD only (no plaintext)
    // -----------------------------------------------------------------------

    #[test]
    fn aes_gcm_aad_only() {
        let key = from_hex("00000000000000000000000000000000");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let nonce = [0u8; 12];
        let output = AesGcm::encrypt(&aes_key, &nonce, b"", b"some aad data");
        assert!(output.ciphertext.is_empty());
        // Tag should still be non-zero
        assert!(output.tag.iter().any(|&b| b != 0));
        let pt = AesGcm::decrypt(&aes_key, &nonce, b"", b"some aad data", &output.tag).unwrap();
        assert!(pt.is_empty());
    }

    // -----------------------------------------------------------------------
    // HMAC with long key (> block size)
    // -----------------------------------------------------------------------

    #[test]
    fn hmac_sha256_long_key() {
        // RFC 4231 Test Case 6: key longer than block size
        let key = vec![0xaa; 131];
        let data = b"Test Using Larger Than Block-Size Key - Hash Key First";
        let mac = Hmac::mac(&key, data, HashFunction::Sha256);
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn hmac_sha256_long_key_and_data() {
        // RFC 4231 Test Case 7
        let key = vec![0xaa; 131];
        let data = b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.";
        let mac = Hmac::mac(&key, data, HashFunction::Sha256);
        assert_eq!(
            hex(&mac),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    // -----------------------------------------------------------------------
    // HKDF with SHA-512
    // -----------------------------------------------------------------------

    #[test]
    fn hkdf_sha512_basic() {
        let ikm = vec![0x0b; 22];
        let salt = from_hex("000102030405060708090a0b0c");
        let info = from_hex("f0f1f2f3f4f5f6f7f8f9");
        let okm = Hkdf::derive(HashFunction::Sha512, &salt, &ikm, &info, 42);
        // Just verify it produces 42 bytes
        assert_eq!(okm.len(), 42);
    }

    // -----------------------------------------------------------------------
    // AES-192 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn aes192_encrypt_decrypt_roundtrip() {
        let key = from_hex("8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let pt = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        let ct = Aes::encrypt_block(&aes_key, &pt);
        let decrypted = Aes::decrypt_block(&aes_key, &ct);
        assert_eq!(decrypted, pt);
    }

    #[test]
    fn aes192_nist_vector() {
        let key = from_hex("8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b");
        let aes_key = Aes::key_expansion(&key).unwrap();
        let pt = from_hex("6bc1bee22e409f96e93d7e117393172a");
        let mut block = [0u8; 16];
        block.copy_from_slice(&pt);
        let ct = Aes::encrypt_block(&aes_key, &block);
        assert_eq!(hex(&ct), "bd334f1d6e45f25ff712a214571fa5cc");
    }

    // =======================================================================
    // BigUint tests
    // =======================================================================

    #[test]
    fn biguint_basic_arithmetic() {
        let a = BigUint::from_u64(12345);
        let b = BigUint::from_u64(6789);
        let sum = a.add(&b);
        assert_eq!(sum.to_bytes_be(), BigUint::from_u64(19134).to_bytes_be());
        let diff = a.sub(&b);
        assert_eq!(diff.to_bytes_be(), BigUint::from_u64(5556).to_bytes_be());
        let prod = a.mul(&b);
        assert_eq!(
            prod.to_bytes_be(),
            BigUint::from_u64(12345 * 6789).to_bytes_be()
        );
    }

    #[test]
    fn biguint_div_rem() {
        let a = BigUint::from_u64(1000000);
        let b = BigUint::from_u64(7);
        let (q, r) = a.div_rem(&b);
        assert_eq!(q.to_bytes_be(), BigUint::from_u64(142857).to_bytes_be());
        assert_eq!(r.to_bytes_be(), BigUint::from_u64(1).to_bytes_be());
    }

    /// Build a `BigUint` from little-endian limbs, so a test can aim at an
    /// exact internal shape rather than hoping a decimal literal lands on one.
    fn biguint_from_limbs(limbs: &[u32]) -> BigUint {
        let mut n = BigUint {
            limbs: limbs.to_vec(),
        };
        n.normalize();
        n
    }

    /// `u = q*v + r` and `0 <= r < v`, the only thing division has to promise.
    fn assert_div_rem_identity(u: &BigUint, v: &BigUint) {
        let (q, r) = u.div_rem(v);
        assert_eq!(
            q.mul(v).add(&r).to_bytes_be(),
            u.to_bytes_be(),
            "q*v + r != u for u={:?} v={:?} (q={:?} r={:?})",
            u.limbs,
            v.limbs,
            q.limbs,
            r.limbs
        );
        assert_eq!(
            r.cmp(v),
            std::cmp::Ordering::Less,
            "remainder {:?} not < divisor {:?}",
            r.limbs,
            v.limbs
        );
    }

    /// Knuth D's quotient-digit estimate must not overflow, on the branch
    /// that can only be reached from inside a division.
    ///
    /// `r_hat` is bounded by `base` only on the `u_high < v_top` branch. On
    /// the other one it starts at `u_mid + v_top`, which reaches ~2^33, and
    /// the old loop computed `base * r_hat` before testing whether `r_hat` was
    /// in range at all. `2^32 * 2^33` does not fit: debug builds panicked,
    /// **release builds wrapped** and skipped a correction that was due.
    ///
    /// Reaching this through `div_rem` means steering a *partial remainder*
    /// several digits in — a 400,000-pair random sweep never did it once, and
    /// neither did three full RSA keygen/encrypt/decrypt cycles. Calling the
    /// estimate directly makes it three arguments.
    ///
    /// Each case is checked against the definition of the estimate rather than
    /// a hard-coded answer: `q_hat` must be the true quotient digit or exactly
    /// one more, which is all D3 promises and all D4/D6 need.
    #[test]
    fn qhat_estimate_is_exact_or_one_high_without_overflowing() {
        const BASE: u128 = 1 << 32;
        // `u_high == v_top` — the branch that overflowed — plus `u_high` above
        // and below it, at the extremes of `u_mid`/`u_low`/`v_top2`.
        let v_tops = [0x8000_0000u64, 0xFFFF_FFFF, 0xC000_0001];
        let extremes = [0u64, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF];
        for v_top in v_tops {
            for v_top2 in extremes {
                for u_mid in extremes {
                    for u_low in extremes {
                        for u_high in [v_top, v_top - 1, v_top.saturating_sub(0x1234_5678), 0] {
                            let q_hat =
                                estimate_quotient_digit(u_high, u_mid, u_low, v_top, v_top2);
                            assert!(q_hat < BASE as u64, "q_hat must fit a limb: {q_hat:#x}");
                            // The true digit, computed in 128-bit with the full
                            // three-limb numerator and two-limb divisor.
                            let numerator =
                                ((u_high as u128) << 64) | ((u_mid as u128) << 32) | u_low as u128;
                            let divisor = ((v_top as u128) << 32) | v_top2 as u128;
                            let exact = numerator / divisor;
                            let exact = exact.min(BASE - 1);
                            assert!(
                                q_hat as u128 == exact || q_hat as u128 == exact + 1,
                                "q_hat {q_hat:#x} is neither the true digit {exact:#x} nor one more \
                                 (u_high={u_high:#x} u_mid={u_mid:#x} u_low={u_low:#x} \
                                 v_top={v_top:#x} v_top2={v_top2:#x})"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The exact operands that overflowed: `u_high == v_top` with `u_mid`
    /// large enough that `u_mid + v_top` crosses 2^32. Before the fix this
    /// panicked with "attempt to multiply with overflow" in a debug build.
    #[test]
    fn qhat_estimate_survives_the_operands_that_panicked() {
        for v_top in [0x8000_0000u64, 0xABCD_EF01, 0xFFFF_FFFF] {
            for u_mid in [0xFFFF_FFFFu64, 0x8000_0000, 0xC000_0000] {
                let q_hat = estimate_quotient_digit(v_top, u_mid, 0xFFFF_FFFF, v_top, 0xFFFF_FFFF);
                assert!(q_hat < 1u64 << 32);
            }
        }
    }

    /// The `div_rem` shapes the estimate feeds, end to end.
    #[test]
    fn biguint_div_rem_qhat_estimate_does_not_overflow() {
        let v = biguint_from_limbs(&[0x0000_0001, 0x8000_0000]);
        for u_mid in [0x8000_0000u32, 0xFFFF_FFFF, 0xC000_0000] {
            for u_low in [0u32, 1, 0xFFFF_FFFF] {
                let u = biguint_from_limbs(&[u_low, u_mid, 0x8000_0000]);
                assert_div_rem_identity(&u, &v);
                // And with a trailing limb, so the loop runs for more than one
                // quotient digit and `u` has been mutated by D4 before the
                // estimate is made again.
                let u = biguint_from_limbs(&[u_low, u_mid, 0x8000_0000, 0x7FFF_FFFF]);
                assert_div_rem_identity(&u, &v);
            }
        }
    }

    /// Sweep divisor/dividend shapes deterministically.
    ///
    /// The defect above reached production as an intermittent failure of
    /// `rsa_cipher_roundtrip_all_paddings`, because whether it fires depends
    /// on the limb values of a randomly generated key — so the coverage that
    /// would have caught it cannot itself depend on random input. This walks a
    /// fixed LCG instead: same pairs on every run, on every machine.
    #[test]
    fn biguint_div_rem_identity_over_many_shapes() {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 32) as u32
        };
        for v_limbs in 2..=6usize {
            for u_limbs in v_limbs..=10usize {
                for _ in 0..40 {
                    let v: Vec<u32> = (0..v_limbs).map(|_| next()).collect();
                    let u: Vec<u32> = (0..u_limbs).map(|_| next()).collect();
                    let v = biguint_from_limbs(&v);
                    let u = biguint_from_limbs(&u);
                    if v.is_zero() {
                        continue;
                    }
                    assert_div_rem_identity(&u, &v);
                }
            }
        }
    }

    /// The extremes of the estimate: an all-ones divisor, a divisor whose top
    /// limb is the smallest value that still normalises to itself, and top
    /// limbs that make `q_hat` land on `base - 1`.
    #[test]
    fn biguint_div_rem_extreme_limb_values() {
        let cases: &[(&[u32], &[u32])] = &[
            (
                &[0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF],
                &[0xFFFF_FFFF, 0xFFFF_FFFF],
            ),
            (&[0, 0, 0xFFFF_FFFF], &[0xFFFF_FFFF, 0x8000_0000]),
            (&[0xFFFF_FFFF, 0, 0x8000_0000], &[0, 0x8000_0000]),
            (&[1, 0xFFFF_FFFF, 0xFFFF_FFFF], &[0xFFFF_FFFF, 0xFFFF_FFFF]),
            (&[0, 0, 0, 1], &[1, 1]),
            (&[0xFFFF_FFFF; 8], &[0x0000_0001, 0x8000_0000]),
        ];
        for (u, v) in cases {
            assert_div_rem_identity(&biguint_from_limbs(u), &biguint_from_limbs(v));
        }
    }

    #[test]
    fn biguint_modpow() {
        // 3^13 mod 50 = 1594323 mod 50 = 23
        let base = BigUint::from_u64(3);
        let exp = BigUint::from_u64(13);
        let m = BigUint::from_u64(50);
        let result = base.modpow(&exp, &m);
        assert_eq!(result.to_bytes_be(), BigUint::from_u64(23).to_bytes_be());
    }

    #[test]
    fn biguint_modinv() {
        // 3^-1 mod 11 = 4 (since 3*4 = 12 ≡ 1 mod 11)
        let a = BigUint::from_u64(3);
        let m = BigUint::from_u64(11);
        let inv = a.modinv(&m).unwrap();
        assert_eq!(inv.to_bytes_be(), BigUint::from_u64(4).to_bytes_be());
    }

    #[test]
    fn biguint_bytes_roundtrip() {
        let bytes = from_hex("deadbeef01020304050607080910111213141516");
        let n = BigUint::from_bytes_be(&bytes);
        let out = n.to_bytes_be();
        assert_eq!(out, bytes);
    }

    // =======================================================================
    // RSA tests
    // =======================================================================

    #[test]
    fn rsa_sign_verify_1024() {
        let (pub_key, priv_key) = Rsa::generate_keypair(1024);
        assert!(pub_key.n.bit_length() == 1024);
        let message = b"Hello RSA!";
        let sig = Rsa::sign_sha256(&priv_key, message);
        assert!(Rsa::verify_sha256(&pub_key, message, &sig));
        // Tamper with message
        assert!(!Rsa::verify_sha256(&pub_key, b"wrong", &sig));
    }

    #[test]
    fn rsa_key_serialization() {
        let (pub_key, priv_key) = Rsa::generate_keypair(1024);
        let der = Rsa::public_key_to_der(&pub_key);
        assert!(!der.is_empty());
        // Parse it back
        let parsed = parse_rsa_public_key(&der).unwrap();
        assert_eq!(parsed.n.to_bytes_be(), pub_key.n.to_bytes_be());
        assert_eq!(parsed.e.to_bytes_be(), pub_key.e.to_bytes_be());

        let priv_der = Rsa::private_key_to_der(&priv_key);
        assert!(!priv_der.is_empty());
    }

    /// A generated key MUST carry its CRT parameters and emit the complete
    /// 9-element PKCS#1 `RSAPrivateKey`, not the legacy 4-element non-CRT form.
    /// Regression guard for http-server-sslengine-identity-singleton-clobber:
    /// a non-CRT DER round-tripped through `RSAKeyFactory$Legacy` becomes a
    /// `sun.security.rsa.RSAPrivateKeyImpl` whose `getEncoded()` is the
    /// incomplete 572-byte PKCS#8 that rustls rejects.
    #[test]
    fn rsa_generated_key_is_crt() {
        let (_pub_key, priv_key) = Rsa::generate_keypair(1024);
        let p = priv_key.p.as_ref().expect("p present");
        let q = priv_key.q.as_ref().expect("q present");
        let dp = priv_key.dp.as_ref().expect("dp present");
        let dq = priv_key.dq.as_ref().expect("dq present");
        let qinv = priv_key.qinv.as_ref().expect("qinv present");
        // p > q (JDK convention).
        assert_eq!(p.cmp(q), std::cmp::Ordering::Greater, "p must exceed q");
        // n == p*q.
        assert_eq!(p.mul(q).to_bytes_be(), priv_key.n.to_bytes_be(), "n == p*q");
        // dp == d mod (p-1), dq == d mod (q-1).
        let p1 = p.sub(&BigUint::one());
        let q1 = q.sub(&BigUint::one());
        assert_eq!(priv_key.d.modulo(&p1).to_bytes_be(), dp.to_bytes_be(), "dP");
        assert_eq!(priv_key.d.modulo(&q1).to_bytes_be(), dq.to_bytes_be(), "dQ");
        // qinv*q ≡ 1 (mod p).
        assert_eq!(
            qinv.mul(q).modulo(p).to_bytes_be(),
            BigUint::one().to_bytes_be(),
            "qInv*q ≡ 1 mod p"
        );
        // The DER carries the CRT tail: a 1024-bit CRT RSAPrivateKey is ~600+
        // bytes; the non-CRT (n,e,d)-only form is ~350. Assert we're on the
        // CRT side of that gap.
        let der = Rsa::private_key_to_der(&priv_key);
        assert!(
            der.len() > 500,
            "generated RSA-1024 key DER must be the full CRT form (got {} bytes)",
            der.len()
        );
    }

    /// A key with no CRT params (an imported bare `(n, d)`) still round-trips
    /// through the legacy 4-element form — no panic, no CRT tail.
    #[test]
    fn rsa_noncrt_key_emits_legacy_form() {
        let key = RsaPrivateKey {
            n: BigUint::from_bytes_be(&[0x00, 0xC1, 0x00, 0x01]),
            d: BigUint::from_bytes_be(&[0x03]),
            e: BigUint::from_bytes_be(&[0x01, 0x00, 0x01]),
            p: None,
            q: None,
            dp: None,
            dq: None,
            qinv: None,
        };
        let der = Rsa::private_key_to_der(&key);
        assert!(!der.is_empty());
    }

    // =======================================================================
    // ECDSA P-256 tests
    // =======================================================================

    #[test]
    fn p256_generator_on_curve() {
        let g = Ecdsa::generator();
        // Verify y^2 = x^3 - 3x + b mod p
        let p = &P256_P;
        let x2 = FieldElement256::mul_mod(&g.x, &g.x, p);
        let x3 = FieldElement256::mul_mod(&x2, &g.x, p);
        let three = FieldElement256 {
            limbs: [3, 0, 0, 0],
        };
        let three_x = FieldElement256::mul_mod(&three, &g.x, p);
        let rhs = FieldElement256::add_mod(&FieldElement256::sub_mod(&x3, &three_x, p), &P256_B, p);
        let y2 = FieldElement256::mul_mod(&g.y, &g.y, p);
        assert_eq!(y2.limbs, rhs.limbs);
    }

    #[test]
    fn ecdsa_sign_verify() {
        let (pub_key, priv_key) = Ecdsa::generate_keypair();
        let msg = b"test message for ECDSA";
        let sig = Ecdsa::sign_sha384(&priv_key, msg);
        assert!(Ecdsa::verify_sha384(&pub_key, msg, &sig));
        assert!(!Ecdsa::verify_sha384(&pub_key, b"wrong", &sig));
    }

    #[test]
    fn ecdsa_key_serialization() {
        let (pub_key, _priv_key) = Ecdsa::generate_keypair();
        let bytes = Ecdsa::public_key_to_bytes(&pub_key);
        assert_eq!(bytes.len(), 65);
        assert_eq!(bytes[0], 0x04);
        let parsed = Ecdsa::public_key_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.point.x.limbs, pub_key.point.x.limbs);
        assert_eq!(parsed.point.y.limbs, pub_key.point.y.limbs);
    }

    // =======================================================================
    // DER encoding/decoding tests
    // =======================================================================

    #[test]
    fn der_integer_encode_decode() {
        let val = vec![0x01, 0x00, 0x01]; // 65537
        let encoded = der_encode_integer(&val);
        assert_eq!(encoded[0], 0x02); // INTEGER tag
        let (decoded, _) = der_read_integer(&encoded).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn der_ecdsa_sig_roundtrip() {
        let r = vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let s = vec![0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];
        let encoded = der_encode_ecdsa_signature(&r, &s);
        let (r2, s2) = der_decode_ecdsa_signature(&encoded).unwrap();
        assert_eq!(r2, r);
        assert_eq!(s2, s);
    }

    // =======================================================================
    // JKS KeyStore tests
    // =======================================================================

    #[test]
    fn jks_magic_detection() {
        let data = vec![
            0xFE, 0xED, 0xFE, 0xED, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
        ];
        let ks = KeyStoreData::load_jks(&data, b"").unwrap();
        assert_eq!(ks.store_type, "JKS");
        assert_eq!(ks.entries.len(), 0);
    }

    #[test]
    fn jks_invalid_magic() {
        let data = vec![0x00, 0x00, 0x00, 0x00];
        assert!(KeyStoreData::load_jks(&data, b"").is_err());
    }

    #[test]
    fn keystore_auto_detect() {
        let jks = vec![
            0xFE, 0xED, 0xFE, 0xED, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
        ];
        let ks = KeyStoreData::load(&jks, b"", "auto").unwrap();
        assert_eq!(ks.store_type, "JKS");
    }

    // =======================================================================
    // X.509 certificate parsing tests
    // =======================================================================

    #[test]
    fn x509_parse_self_signed() {
        // Build a minimal self-signed X.509 cert structure for testing
        // This is a structurally valid DER cert (not cryptographically valid)
        let serial = der_encode_integer(&[0x01]);
        // Sig alg: sha256WithRSAEncryption
        let sig_alg_oid: &[u8] = &[
            0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00,
        ];
        let sig_alg = der_encode_sequence(sig_alg_oid);
        // Issuer: CN=Test
        let cn_oid = &[0x06, 0x03, 0x55, 0x04, 0x03];
        let cn_val = &[0x0c, 0x04, 0x54, 0x65, 0x73, 0x74]; // UTF8String "Test"
        let mut atv = Vec::new();
        atv.extend_from_slice(cn_oid);
        atv.extend_from_slice(cn_val);
        let atv_seq = der_encode_sequence(&atv);
        let rdn_set = {
            let mut s = vec![0x31];
            s.extend_from_slice(&der_encode_length(atv_seq.len()));
            s.extend_from_slice(&atv_seq);
            s
        };
        let issuer = der_encode_sequence(&rdn_set);
        let subject = issuer.clone();
        // Validity: notBefore=240101000000Z, notAfter=341231235959Z
        let nb = &[
            0x17, 0x0d, b'2', b'4', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'0',
            b'Z',
        ];
        let na = &[
            0x17, 0x0d, b'3', b'4', b'1', b'2', b'3', b'1', b'2', b'3', b'5', b'9', b'5', b'9',
            b'Z',
        ];
        let mut validity_inner = Vec::new();
        validity_inner.extend_from_slice(nb);
        validity_inner.extend_from_slice(na);
        let validity = der_encode_sequence(&validity_inner);
        // SPKI (stub RSA)
        let spki =
            der_encode_sequence(&[0x30, 0x03, 0x06, 0x01, 0x00, 0x03, 0x03, 0x00, 0x01, 0x02]);

        // TBSCertificate
        let version = &[0xa0, 0x03, 0x02, 0x01, 0x02]; // v3
        let mut tbs = Vec::new();
        tbs.extend_from_slice(version);
        tbs.extend_from_slice(&serial);
        tbs.extend_from_slice(&sig_alg);
        tbs.extend_from_slice(&issuer);
        tbs.extend_from_slice(&validity);
        tbs.extend_from_slice(&subject);
        tbs.extend_from_slice(&spki);
        let tbs_seq = der_encode_sequence(&tbs);

        // Outer sig alg + signature
        let outer_sig_alg = sig_alg.clone();
        let sig_value = &[0x03, 0x03, 0x00, 0xab, 0xcd]; // BIT STRING

        let mut cert_inner = Vec::new();
        cert_inner.extend_from_slice(&tbs_seq);
        cert_inner.extend_from_slice(&outer_sig_alg);
        cert_inner.extend_from_slice(sig_value);
        let cert_der = der_encode_sequence(&cert_inner);

        let parsed = X509Cert::parse_der(&cert_der).unwrap();
        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.sig_algorithm, "SHA256withRSA");
        assert_eq!(parsed.subject_cn, "Test");
        assert_eq!(parsed.issuer_cn, "Test");
        assert!(parsed.not_before > 0);
        assert!(parsed.not_after > parsed.not_before);
    }

    #[test]
    fn datetime_to_epoch_known_value() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        let ts = super::datetime_to_epoch(2024, 1, 1, 0, 0, 0);
        assert_eq!(ts, 1704067200);
    }

    // =======================================================================
    // Global store tests
    // =======================================================================

    #[test]
    fn keystore_global_store_roundtrip() {
        let id = keystore_next_id();
        let data = KeyStoreData {
            store_type: "JKS".into(),
            entries: HashMap::new(),
        };
        keystore_store(id, data);
        let retrieved = keystore_get(id).unwrap();
        assert_eq!(retrieved.store_type, "JKS");
    }

    // --- Phase 80.4: SecureRandom CSPRNG Tests ---

    #[test]
    fn os_random_bytes_succeeds() {
        // OS entropy must succeed on any platform where this JVM runs.
        let mut buf = [0u8; 32];
        assert!(
            super::os_random_bytes(&mut buf),
            "OS entropy source must be available"
        );
        // Output must not be all zeros (statistically impossible for 32 bytes).
        assert_ne!(buf, [0u8; 32], "OS entropy must produce non-zero output");
    }

    #[test]
    fn secure_random_output_distribution() {
        // Verify that SecureRandom produces non-trivial output.
        // Generate 256 bytes and check that at least 8 distinct byte values appear.
        let mut sr = super::SecureRandom::new();
        let mut buf = [0u8; 256];
        sr.next_bytes(&mut buf);
        let mut seen = std::collections::HashSet::new();
        for &b in &buf {
            seen.insert(b);
        }
        assert!(
            seen.len() >= 8,
            "256 random bytes must contain at least 8 distinct values, got {}",
            seen.len()
        );
    }

    // -----------------------------------------------------------------------
    // Phase F — additional JCA conformance coverage
    // -----------------------------------------------------------------------

    // RF.1: NIST-published digest test vectors for MD5 / SHA-1 / SHA-512.
    // SHA-256 is already covered above; these complete the "four algorithms"
    // success criterion from the roadmap.

    #[test]
    fn rf1_sha1_abc() {
        // FIPS PUB 180-1 Appendix A
        let h = crate::real_sha1(b"abc");
        assert_eq!(hex(&h), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn rf1_sha1_longer_message() {
        // FIPS PUB 180-1 Appendix A
        let h = crate::real_sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(hex(&h), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
    }

    #[test]
    fn rf1_md5_empty_and_known() {
        // RFC 1321 test suite
        assert_eq!(
            hex(&crate::real_md5(b"")),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(
            hex(&crate::real_md5(b"abc")),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            hex(&crate::real_md5(b"message digest")),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
    }

    #[test]
    fn rf1_sha512_abc() {
        // FIPS PUB 180-4 Appendix C
        let h = Sha512::digest(b"abc");
        assert_eq!(
            hex(&h),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    // RF.2: HMAC-SHA256/384/512 RFC 4231 Test Case 2 (key=Jefe). Test Case 1
    // is covered above; Case 2 probes a different key/data ratio and was
    // chosen because the original suite already covers the all-aa key case.

    #[test]
    fn rf2_hmac_sha256_rfc4231_tc2() {
        // RFC 4231 §4.3 Test Case 2
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let mac = Hmac::mac(key, data, HashFunction::Sha256);
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn rf2_hmac_sha512_rfc4231_tc2() {
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let mac = Hmac::mac(key, data, HashFunction::Sha512);
        assert_eq!(
            hex(&mac),
            "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea250554\
             9758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737"
        );
    }

    // RF.3: NIST SP 800-38D GCM test vector — the roadmap asks for AES-256-GCM
    // parity with NIST vectors. Test Case 1 from gcmEncryptExtIV256.rsp
    // (zero key, zero IV, zero plaintext) is the canonical smoke test.
    #[test]
    fn rf3_aes_256_gcm_nist_zero_vector() {
        use aes_gcm::{aead::Aead, Aes256Gcm, Key, KeyInit, Nonce};
        let key = Key::<Aes256Gcm>::from_slice(&[0u8; 32]);
        let cipher = Aes256Gcm::new(key);
        let nonce = Nonce::from_slice(&[0u8; 12]);
        let ct = cipher.encrypt(nonce, b"".as_ref()).unwrap();
        // Expected tag (no plaintext) from NIST vectors.
        assert_eq!(hex(&ct), "530f8afbc74536b9a963b4f1c4cb738b");
    }

    /// Attribution probe for the RSA private-key operation, kept because
    /// `perf/biginteger-modpow-has-no-montgomery-reduction-20260817` was closed
    /// on the strength of it: after Montgomery, the secret-exponent modpow is
    /// no longer the whole cost of a signature, and anyone tempted to make
    /// modpow faster again should re-run this first and check that modpow is
    /// still what they are paying for.
    ///
    /// `cargo test --release -p cratonvm-native-builtins --lib -- --ignored \
    ///     rsa_sign_cost_attribution --nocapture`
    #[test]
    #[ignore = "timing probe, not an assertion; run explicitly with --nocapture"]
    fn rsa_sign_cost_attribution() {
        use std::time::Instant;
        let (_pk, sk) = Rsa::generate_keypair(2048);
        let msg = b"the floor under every certificate test";

        let t = Instant::now();
        for _ in 0..10 {
            let _ = Rsa::sign_sha256(&sk, msg);
        }
        let whole = t.elapsed();

        let m = BigUint::from_bytes_be(&Sha256::digest(msg));
        let t = Instant::now();
        for _ in 0..10 {
            let _ = m.modpow(&sk.d, &sk.n);
        }
        let secret_modpow = t.elapsed();

        let t = Instant::now();
        for _ in 0..10 {
            let _ = rsa_crt_exponentiate(&sk, &m).expect("CRT");
        }
        let crt = t.elapsed();

        let r = rsa_random_coprime(&sk.n).expect("blinding factor");
        let t = Instant::now();
        for _ in 0..10 {
            let _ = r.modinv(&sk.n);
        }
        let modinv = t.elapsed();

        let t = Instant::now();
        for _ in 0..10 {
            let _ = r.modpow(&sk.e, &sk.n);
        }
        let public_modpow = t.elapsed();

        let t = Instant::now();
        for _ in 0..10 {
            let _ = rsa_random_coprime(&sk.n);
        }
        let draw = t.elapsed();

        println!("--- RSA-2048 sign_sha256, x10 ---");
        println!("  whole signature      {whole:>12.2?}");
        println!("  secret-exponent modpow {secret_modpow:>10.2?}   (full-width d)");
        println!("  CRT + fault check    {crt:>12.2?}");
        println!("  r.modinv(n)          {modinv:>12.2?}");
        println!("  r^e mod n            {public_modpow:>12.2?}");
        println!("  draw blinding factor {draw:>12.2?}");
    }

    /// `BigUint::modpow` splits on the parity of the modulus: odd takes the
    /// Montgomery ladder, even keeps dividing. The two arms must be
    /// indistinguishable — this drives them against each other over odd moduli
    /// of every limb width, and both against a `u128` oracle on small operands
    /// where an exact answer can be computed independently.
    ///
    /// The doc this retires
    /// (`perf/biginteger-modpow-has-no-montgomery-reduction-20260817`) names the
    /// failure mode precisely: a subtly wrong modPow yields plausible-looking
    /// wrong signatures that still round-trip. Checking the Montgomery arm only
    /// against itself would not catch that; checking it against the routine it
    /// replaced does.
    #[test]
    fn modpow_montgomery_matches_the_dividing_ladder() {
        fn xorshift(state: &mut u64) -> u64 {
            let mut x = *state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *state = x;
            x
        }

        // Small operands against an exact u128 oracle.
        for m in [3u64, 5, 7, 255, 65537, 4294967295, 4294967291] {
            for base in [0u64, 1, 2, 3, m - 1, m, m + 1, 123456789] {
                for e in [0u64, 1, 2, 3, 17, 65537, 4294967296] {
                    let got = BigUint::from_u64(base)
                        .modpow(&BigUint::from_u64(e), &BigUint::from_u64(m));
                    let mut want: u128 = 1;
                    let mut b = (base % m) as u128;
                    let mut k = e;
                    while k > 0 {
                        if k & 1 == 1 {
                            want = want * b % m as u128;
                        }
                        b = b * b % m as u128;
                        k >>= 1;
                    }
                    assert_eq!(
                        got.to_bytes_be(),
                        BigUint::from_u64(want as u64).to_bytes_be(),
                        "{base}^{e} mod {m}"
                    );
                }
            }
        }

        // Wide operands: Montgomery arm vs the dividing arm it replaced.
        let mut state = 0x51ed_c0de_0817_2026u64;
        for limbs in 1..=8usize {
            for _ in 0..12 {
                let mut m = BigUint {
                    limbs: (0..limbs).map(|_| xorshift(&mut state) as u32).collect(),
                };
                m.limbs[0] |= 1; // odd
                m.limbs[limbs - 1] |= 1 << 31; // full width
                let base = BigUint {
                    limbs: (0..limbs).map(|_| xorshift(&mut state) as u32).collect(),
                };
                for ebits in [1usize, 7, 32, 33, 64, 127, 256] {
                    let ewords = ebits.div_ceil(32);
                    let mut e = BigUint {
                        limbs: (0..ewords).map(|_| xorshift(&mut state) as u32).collect(),
                    };
                    let top = (ebits - 1) % 32;
                    e.limbs[ewords - 1] &= (1u32 << top) | ((1u32 << top) - 1);
                    e.limbs[ewords - 1] |= 1u32 << top;
                    assert_eq!(
                        base.modpow(&e, &m).limbs,
                        base.modpow_dividing(&e, &m).limbs,
                        "montgomery vs dividing, {limbs} limbs, {ebits}-bit exponent"
                    );
                }
                // Degenerate bases and exponents.
                let zero = BigUint::zero();
                assert_eq!(base.modpow(&zero, &m).limbs, vec![1], "x^0 == 1");
                assert_eq!(
                    zero.modpow(&BigUint::from_u64(5), &m).limbs,
                    Vec::<u32>::new()
                );
                assert_eq!(
                    base.modpow(&BigUint::one(), &m).limbs,
                    base.modulo(&m).limbs
                );
                // An even modulus must still take the dividing arm and agree
                // with itself.
                let m_even = m.add(&BigUint::one());
                let e = BigUint::from_u64(65537);
                assert_eq!(
                    base.modpow(&e, &m_even).limbs,
                    base.modpow_dividing(&e, &m_even).limbs,
                    "even modulus falls back"
                );
            }
        }
    }

    /// The CRT private op against the full-width one it replaced.
    ///
    /// `internal/performance/rsa-private-key-op-crt-FIXED-20260817`
    /// set the bar: "a differential test against the non-CRT path across key
    /// sizes, imported-key (`None` CRT params) fallback, and the fault-check
    /// branch". All three are here, and the differential is against
    /// `base.modpow(&d, &n)` — the exact expression the CRT path replaced — so
    /// the new arm is never checked only against itself.
    #[test]
    fn crt_private_op_matches_the_full_width_exponentiation() {
        for bits in [1024usize, 2048] {
            let (_pk, sk) = Rsa::generate_keypair(bits);
            assert!(sk.p.is_some(), "generated key must carry CRT parameters");

            // A spread of bases, including the ones that exercise the
            // `m1 < m2` borrow in Garner and the degenerate residues.
            let mut bases = vec![
                BigUint::zero(),
                BigUint::one(),
                BigUint::from_u64(2),
                BigUint::from_u64(65537),
                sk.n.sub(&BigUint::one()),
            ];
            let mut seed = 0x5eed_c27_0817u64;
            for _ in 0..24 {
                let mut bytes = vec![0u8; bits / 8];
                for b in bytes.iter_mut() {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    *b = seed as u8;
                }
                bases.push(BigUint::from_bytes_be(&bytes).modulo(&sk.n));
            }

            for base in &bases {
                let want = base.modpow(&sk.d, &sk.n);
                let crt = rsa_crt_exponentiate(&sk, base).expect("CRT must engage");
                assert_eq!(
                    crt.to_bytes_be(),
                    want.to_bytes_be(),
                    "{bits}-bit CRT disagrees with full-width d"
                );
                // The dispatcher must pick the same value either way.
                assert_eq!(
                    rsa_private_exponentiate(&sk, base).to_bytes_be(),
                    want.to_bytes_be(),
                    "{bits}-bit dispatcher"
                );
                // Blinded, the answer is still the answer — the blinding factor
                // is random per call, so this also says the unblind is right.
                assert_eq!(
                    rsa_private_op_blinded(&sk, base).to_bytes_be(),
                    want.to_bytes_be(),
                    "{bits}-bit blinded CRT"
                );
            }
        }
    }

    /// A key with no CRT parameters — every `(n, d)` import — must still sign,
    /// via the full-width fallback. Losing the parameters may cost speed; it
    /// may never change an answer.
    #[test]
    fn crt_absent_falls_back_and_still_signs() {
        let (pk, sk) = Rsa::generate_keypair(1024);
        let stripped = RsaPrivateKey {
            n: sk.n.clone(),
            d: sk.d.clone(),
            e: sk.e.clone(),
            p: None,
            q: None,
            dp: None,
            dq: None,
            qinv: None,
        };
        let base = BigUint::from_u64(0xdead_beef);
        assert!(
            rsa_crt_exponentiate(&stripped, &base).is_none(),
            "no parameters => no CRT"
        );
        assert_eq!(
            rsa_private_exponentiate(&stripped, &base).to_bytes_be(),
            base.modpow(&sk.d, &sk.n).to_bytes_be(),
            "fallback must equal the full-width answer"
        );
        let msg = b"imported key, no CRT tail";
        let sig = Rsa::sign_sha256(&stripped, msg);
        assert!(
            Rsa::verify_sha256(&pk, msg, &sig),
            "stripped key still signs"
        );

        // Partial parameters are not usable parameters: any missing member
        // sends it back to the fallback rather than half-computing.
        for drop_idx in 0..5 {
            let mut k = clone_key(&sk);
            match drop_idx {
                0 => k.p = None,
                1 => k.q = None,
                2 => k.dp = None,
                3 => k.dq = None,
                _ => k.qinv = None,
            }
            assert!(
                rsa_crt_exponentiate(&k, &base).is_none(),
                "missing member {drop_idx} must decline CRT"
            );
            assert_eq!(
                rsa_private_exponentiate(&k, &base).to_bytes_be(),
                base.modpow(&sk.d, &sk.n).to_bytes_be(),
                "missing member {drop_idx} still answers correctly"
            );
        }
    }

    /// The Bellcore branch: a key whose CRT parameters are WRONG must be caught
    /// by the fault check and fall back, never return the faulty value.
    ///
    /// This is the whole reason the check exists. A faulty CRT signature lets
    /// `gcd(s - s_correct, n)` recover a factor of `n` from a single sample,
    /// and `parse_rsa_private_key_der` reads all five parameters out of a
    /// PKCS#8 file without ever checking `n == p*q`, so a hostile key file
    /// reaches this code directly.
    #[test]
    fn corrupt_crt_parameters_are_caught_and_do_not_leak_a_factor() {
        let (_pk, sk) = Rsa::generate_keypair(1024);
        let base = BigUint::from_u64(0x1234_5678_9abc);
        let correct = base.modpow(&sk.d, &sk.n);

        // Corrupt each parameter in turn. `dP`/`dQ` are the realistic fault
        // (a flipped exponent bit); `p`/`q`/`qInv` cover a malformed import.
        for name in ["dP", "dQ", "qInv", "p", "q"] {
            let mut bad = clone_key(&sk);
            let one = BigUint::one();
            let two = BigUint::from_u64(2);
            match name {
                "dP" => bad.dp = Some(bad.dp.as_ref().unwrap().add(&one)),
                "dQ" => bad.dq = Some(bad.dq.as_ref().unwrap().add(&one)),
                "qInv" => bad.qinv = Some(bad.qinv.as_ref().unwrap().add(&one)),
                "p" => bad.p = Some(bad.p.as_ref().unwrap().add(&two)),
                _ => bad.q = Some(bad.q.as_ref().unwrap().add(&two)),
            }
            assert!(
                rsa_crt_exponentiate(&bad, &base).is_none(),
                "corrupt {name} must fail the fault check"
            );
            // And the caller-facing entry points still return the RIGHT answer,
            // because declining CRT falls back rather than failing.
            assert_eq!(
                rsa_private_exponentiate(&bad, &base).to_bytes_be(),
                correct.to_bytes_be(),
                "corrupt {name}: fallback answer"
            );
            assert_eq!(
                rsa_private_op_blinded(&bad, &base).to_bytes_be(),
                correct.to_bytes_be(),
                "corrupt {name}: blinded fallback answer"
            );
        }

        // A degenerate p or q declines before any arithmetic runs.
        for degenerate in [BigUint::zero(), BigUint::one()] {
            let mut bad = clone_key(&sk);
            bad.p = Some(degenerate.clone());
            assert!(rsa_crt_exponentiate(&bad, &base).is_none(), "degenerate p");
            let mut bad = clone_key(&sk);
            bad.q = Some(degenerate.clone());
            assert!(rsa_crt_exponentiate(&bad, &base).is_none(), "degenerate q");
        }

        // An unverifiable public exponent gets no CRT at all.
        for tiny in [BigUint::zero(), BigUint::one(), BigUint::from_u64(2)] {
            let mut bad = clone_key(&sk);
            bad.e = tiny;
            assert!(
                rsa_crt_exponentiate(&bad, &base).is_none(),
                "e too small to verify => no CRT"
            );
        }
    }

    fn clone_key(k: &RsaPrivateKey) -> RsaPrivateKey {
        RsaPrivateKey {
            n: k.n.clone(),
            d: k.d.clone(),
            e: k.e.clone(),
            p: k.p.clone(),
            q: k.q.clone(),
            dp: k.dp.clone(),
            dq: k.dq.clone(),
            qinv: k.qinv.clone(),
        }
    }

    /// The handle-based decrypt (CRT) must be indistinguishable from the
    /// `(n, d)` decrypt it accelerates — same plaintexts AND same failure
    /// classes. The two share a body precisely so they cannot drift, and this
    /// is the test that says so.
    #[test]
    fn decrypt_by_id_matches_the_n_d_path_including_its_refusals() {
        let (pk, sk) = Rsa::generate_keypair(2048);
        let id = rsa_key_next_id();
        rsa_key_store(
            id,
            RsaKeyPairData {
                public_key: pk,
                private_key: sk,
            },
        );
        let (n, d) = rsa_key_get_priv(id).expect("private components");
        let (_, e) = rsa_key_get_pub(id).expect("public components");

        for pad in [
            RsaCipherPadding::Pkcs1,
            RsaCipherPadding::OaepSha1,
            RsaCipherPadding::OaepSha256,
        ] {
            // A ciphertext with a leading zero byte makes the "short" row below
            // hand decrypt the same integer; re-roll, as the sibling test does.
            let ct = loop {
                let c = rsa_cipher_encrypt(&n, &e, pad, b"crt payload").expect("encrypt");
                if c[0] != 0 {
                    break c;
                }
            };

            let by_id = rsa_cipher_decrypt_by_id(id, &n, pad, &ct)
                .expect("handle is in the store")
                .expect("decrypt");
            let by_nd = rsa_cipher_decrypt(&n, &d, pad, &ct).expect("decrypt");
            assert_eq!(by_id, b"crt payload".to_vec(), "{pad:?}: plaintext");
            assert_eq!(by_id, by_nd, "{pad:?}: the two paths must agree");

            // Refusals must carry the same class from both entry points.
            let mut corrupt = ct.clone();
            corrupt[200] ^= 0x01;
            let a = rsa_cipher_decrypt_by_id(id, &n, pad, &corrupt)
                .expect("handle")
                .expect_err("corrupt must not decrypt");
            let b = rsa_cipher_decrypt(&n, &d, pad, &corrupt).expect_err("corrupt");
            assert_eq!(a.jca_class(), b.jca_class(), "{pad:?}: corrupt class");
            assert_eq!(a.message(), b.message(), "{pad:?}: corrupt message");

            let mut long = ct.clone();
            long.push(0);
            let a = rsa_cipher_decrypt_by_id(id, &n, pad, &long)
                .expect("handle")
                .expect_err("over-long must be refused");
            let b = rsa_cipher_decrypt(&n, &d, pad, &long).expect_err("over-long");
            assert_eq!(a.jca_class(), b.jca_class(), "{pad:?}: over-long class");
            assert_eq!(a.message(), b.message(), "{pad:?}: over-long message");

            let a = rsa_cipher_decrypt_by_id(id, &n, pad, &ct[1..])
                .expect("handle")
                .expect_err("short must not decrypt");
            let b = rsa_cipher_decrypt(&n, &d, pad, &ct[1..]).expect_err("short");
            assert_eq!(a.jca_class(), b.jca_class(), "{pad:?}: short class");
        }

        // An unknown handle is "I cannot help", not a decryption failure — the
        // caller falls back to (n, d) rather than reporting a bad ciphertext.
        assert!(
            rsa_cipher_decrypt_by_id(u64::MAX, &n, RsaCipherPadding::Pkcs1, b"x").is_none(),
            "unknown handle must decline, not fail"
        );

        // THE INTERLOCK. A handle that names a real but DIFFERENT key must
        // decline rather than decrypt with it. This is the collision the JCA
        // layer's field-slot fallback can produce on a genuine JDK key, and
        // without the modulus check it would silently return a wrong plaintext.
        let (other_pk, other_sk) = Rsa::generate_keypair(2048);
        let other_id = rsa_key_next_id();
        rsa_key_store(
            other_id,
            RsaKeyPairData {
                public_key: other_pk,
                private_key: other_sk,
            },
        );
        let ct =
            rsa_cipher_encrypt(&n, &e, RsaCipherPadding::Pkcs1, b"crt payload").expect("encrypt");
        assert!(
            rsa_cipher_decrypt_by_id(other_id, &n, RsaCipherPadding::Pkcs1, &ct).is_none(),
            "a handle naming a different key must decline"
        );
        // ...while the right handle for that same modulus still works.
        assert_eq!(
            rsa_cipher_decrypt_by_id(id, &n, RsaCipherPadding::Pkcs1, &ct)
                .expect("handle")
                .expect("decrypt"),
            b"crt payload".to_vec()
        );
    }

    // RF.6: RSA key generation produces a functional sign/verify pair.
    #[test]
    fn rf6_rsa_keypair_roundtrip() {
        let (pk, sk) = Rsa::generate_keypair(1024);
        let msg = b"phase-f roadmap verification";
        let sig = Rsa::sign_sha256(&sk, msg);
        assert!(Rsa::verify_sha256(&pk, msg, &sig), "own-key verify");
        // Tampered payload must fail.
        let mut bad = msg.to_vec();
        bad[0] ^= 1;
        assert!(!Rsa::verify_sha256(&pk, &bad, &sig), "tampered payload");
    }

    #[test]
    fn rsa_blinding_matches_plain_modpow() {
        // Base blinding MUST be functionally transparent: the blinded private
        // exponentiation has to produce exactly the same result as the plain
        // `modpow` for every operand. Use the textbook RSA instance
        // (n=3233, e=17, d=2753) so the math is checkable by hand, plus a sweep
        // of operands to catch any reduction/inverse mistake.
        let n = BigUint::from_u64(3233);
        let e = BigUint::from_u64(17);
        let d = BigUint::from_u64(2753);
        for base_v in [2u64, 7, 42, 65, 1000, 3232] {
            let base = BigUint::from_u64(base_v);
            let expected = base.modpow(&d, &n);
            // With public exponent (RSA sign path).
            let with_e = rsa_private_modpow_blinded(&base, &d, &e, &n);
            assert!(
                with_e == expected,
                "blinded(with e) mismatch for base {}",
                base_v
            );
            // Without public exponent (RSA Cipher decrypt path).
            let no_e = rsa_private_modpow_blinded_no_e(&base, &d, &n);
            assert!(
                no_e == expected,
                "blinded(no e) mismatch for base {}",
                base_v
            );
        }
    }

    #[test]
    fn rsa_random_coprime_is_in_range_and_coprime() {
        // The blinding factor must be coprime to n (so it is invertible) and a
        // non-trivial value in [2, n).
        let n = BigUint::from_u64(3233); // 53 * 61
        let r = rsa_random_coprime(&n).expect("OS entropy available in test");
        assert!(
            r.cmp(&BigUint::from_u64(2)) != std::cmp::Ordering::Less,
            "r >= 2"
        );
        assert!(r.cmp(&n) == std::cmp::Ordering::Less, "r < n");
        let (g, _, _, _, _) = BigUint::extended_gcd(&r, &n);
        assert!(g.is_one(), "gcd(r, n) == 1");
    }

    // RSA `Cipher` round-trip: encrypt with the public key, decrypt with the
    // private key, for every supported padding. Exercises the EME-PKCS1-v1_5
    // and EME-OAEP (SHA-1 / SHA-256) encode/decode paths used by the keycloak
    // JWE RSA1_5 / RSA-OAEP / RSA-OAEP-256 transformations. Also covers the
    // base-blinded decrypt path (blinding must not change the plaintext).
    #[test]
    fn rsa_cipher_roundtrip_all_paddings() {
        let (pk, sk) = Rsa::generate_keypair(2048);
        let n = pk.n.to_bytes_be();
        let e = pk.e.to_bytes_be();
        let d = sk.d.to_bytes_be();
        for pad in [
            RsaCipherPadding::Pkcs1,
            RsaCipherPadding::OaepSha1,
            RsaCipherPadding::OaepSha256,
        ] {
            let msg = b"0123456789abcdef"; // 16-byte AES CEK, like JWE
            let ct = rsa_cipher_encrypt(&n, &e, pad, msg).expect("encrypt");
            assert_eq!(ct.len(), 256, "{:?}: ciphertext is modulus-sized", pad);
            let pt = rsa_cipher_decrypt(&n, &d, pad, &ct).expect("decrypt");
            assert_eq!(pt, msg, "{:?}: round-trip", pad);
        }
    }

    /// The exception CLASS every RSA failure mode must become, pinned against
    /// SunJCE on Temurin 25.0.3+9 (`probes/JcaExceptionTypeProbe.expected.txt`).
    ///
    /// Every one of these used to be an unchecked `IllegalStateException` at the
    /// `Cipher` layer, so a caller's `catch (BadPaddingException e)` — the
    /// exception `doFinal` DECLARES — was dead. The class is the only thing a
    /// `catch` selects on, so the class is what this asserts; the round-trip
    /// test above cannot see any of it, because a round trip never fails.
    #[test]
    fn rsa_cipher_failures_carry_the_class_sunjce_raises() {
        // ORDER THE TWO MODULI, and the reason is a measured 1-in-8 flake.
        //
        // The wrong-key row below encrypts under the first key and decrypts
        // under the second, and asserts a PADDING failure. Both moduli are
        // 2048-bit and otherwise unrelated, so roughly half the time the second
        // is the smaller of the two — and then the ciphertext integer (uniform
        // below the FIRST modulus) is sometimes at or above the second, where
        // `RSACore.parseMsg`'s guard rejects it as
        // `BadPaddingException("Message is larger than modulus")` BEFORE any
        // unpadding runs. Same class, different message, and the message is
        // what the VULN(1) oracle repair pins:
        //
        // ```text
        // assertion `left == right` failed: Pkcs1
        //   left: "Message is larger than modulus"
        //  right: "Padding error in decryption"
        // ```
        //
        // Sorting the pair so the ENCRYPTING modulus is the smaller one makes
        // `ct < n <= other_n` hold by construction, so the ciphertext is always
        // a decryptable representative under the wrong key and the failure is
        // always the unpadding one the test is about. Relaxing the assertion
        // instead would retire the oracle regression it exists for.
        let (a_pk, a_sk) = Rsa::generate_keypair(2048);
        let (b_pk, b_sk) = Rsa::generate_keypair(2048);
        let ((pk, sk), (other_pk, other_sk)) = if a_pk.n.cmp(&b_pk.n) == std::cmp::Ordering::Less {
            ((a_pk, a_sk), (b_pk, b_sk))
        } else {
            ((b_pk, b_sk), (a_pk, a_sk))
        };
        let n = pk.n.to_bytes_be();
        let e = pk.e.to_bytes_be();
        let d = sk.d.to_bytes_be();
        let other_n = other_pk.n.to_bytes_be();
        let other_d = other_sk.d.to_bytes_be();

        let bad = "javax/crypto/BadPaddingException";
        let size = "javax/crypto/IllegalBlockSizeException";

        for pad in [
            RsaCipherPadding::Pkcs1,
            RsaCipherPadding::OaepSha1,
            RsaCipherPadding::OaepSha256,
        ] {
            // Re-roll until the ciphertext's leading byte is non-zero.
            // `rsa_cipher_encrypt` emits exactly `k` bytes via
            // `to_bytes_be_padded`, so a ciphertext below 2^(8(k-1)) — about 1
            // in 128 for this modulus — carries a leading zero, and the
            // "shorter than the modulus" row below then hands `decrypt` the
            // SAME integer, which decrypts correctly and fails an assertion
            // that is about length, not about padding. Measured at 3/60 runs
            // before this loop, on both this tree and dev. Encryption is
            // randomized (PKCS#1 type 2 / OAEP seed), so re-rolling is free and
            // terminates immediately.
            let ct = loop {
                let c = rsa_cipher_encrypt(&n, &e, pad, b"payload").expect("encrypt");
                if c[0] != 0 {
                    break c;
                }
            };

            // The WRONG PRIVATE KEY and a CORRUPTED CIPHERTEXT are padding
            // failures. This is the row the regression suite sampled.
            let wrong_key = rsa_cipher_decrypt(&other_n, &other_d, pad, &ct)
                .expect_err("the wrong private key must not decrypt");
            assert_eq!(wrong_key.jca_class(), bad, "{pad:?}: wrong key");
            let mut corrupt = ct.clone();
            corrupt[200] ^= 0x01;
            let flipped = rsa_cipher_decrypt(&n, &d, pad, &corrupt)
                .expect_err("a flipped ciphertext byte must not decrypt");
            assert_eq!(flipped.jca_class(), bad, "{pad:?}: corrupted ciphertext");

            // Every padding failure must ALSO still carry the single opaque
            // message the VULN(1) constant-time repair collapsed them to.
            // Widening the exception surface must not reopen the
            // Bleichenbacher/Manger oracle.
            assert_eq!(wrong_key.message(), RSA_PADDING_ERROR, "{pad:?}");
            assert_eq!(flipped.message(), RSA_PADDING_ERROR, "{pad:?}");

            // A SHORTER-than-modulus ciphertext is a smaller integer that
            // decrypts and fails to unpad, NOT a block-size failure. A LONGER
            // one is the reverse. The asymmetry is SunJCE's, and a
            // `ct.len() != k` check gets it wrong in one direction while
            // getting the class wrong in both.
            let short = rsa_cipher_decrypt(&n, &d, pad, &ct[1..])
                .expect_err("a short ciphertext must not decrypt");
            assert_eq!(short.jca_class(), bad, "{pad:?}: short ciphertext");
            let mut long = ct.clone();
            long.push(0);
            let long_err = rsa_cipher_decrypt(&n, &d, pad, &long)
                .expect_err("an over-long ciphertext must be refused");
            assert_eq!(long_err.jca_class(), size, "{pad:?}: over-long ciphertext");
            assert_eq!(
                long_err.message(),
                "Data must not be longer than 256 bytes",
                "{pad:?}: SunJCE's own wording"
            );

            // Too much plaintext for the padding: 245 for PKCS#1 v1.5, 214 for
            // OAEP-SHA-1, 190 for OAEP-SHA-256 under a 2048-bit modulus.
            let max = pad.max_data_size(256).expect("2048-bit modulus fits");
            assert!(
                rsa_cipher_encrypt(&n, &e, pad, &vec![0u8; max]).is_ok(),
                "{pad:?}: exactly max_data_size must still encrypt"
            );
            let too_long = rsa_cipher_encrypt(&n, &e, pad, &vec![0u8; max + 1])
                .expect_err("one byte over the padding limit must be refused");
            assert_eq!(too_long.jca_class(), size, "{pad:?}: plaintext too long");
            assert_eq!(
                too_long.message(),
                format!("Data must not be longer than {max} bytes"),
                "{pad:?}: SunJCE's own wording"
            );
        }

        // The measured limits, so a change to `hlen` or to the overheads shows
        // up here as a number rather than as a silently different refusal.
        assert_eq!(RsaCipherPadding::Pkcs1.max_data_size(256), Some(245));
        assert_eq!(RsaCipherPadding::OaepSha1.max_data_size(256), Some(214));
        assert_eq!(RsaCipherPadding::OaepSha256.max_data_size(256), Some(190));
        // A modulus too small to hold the padding is a KEY problem, not a data
        // one — SunJCE raises InvalidKeyException from `RSAPadding.getInstance`.
        assert_eq!(RsaCipherPadding::OaepSha256.max_data_size(66), None);
        assert_eq!(RsaCipherPadding::Pkcs1.max_data_size(11), None);

        // ANTI-VACUITY: the three variants must map to three DIFFERENT classes.
        // A `jca_class` that answered `BadPaddingException` for everything
        // would satisfy most of the assertions above and would be the same
        // defect one level down.
        assert_ne!(
            RsaCipherError::BlockSize(String::new()).jca_class(),
            RsaCipherError::Padding(String::new()).jca_class()
        );
        assert_ne!(
            RsaCipherError::Key(String::new()).jca_class(),
            RsaCipherError::Padding(String::new()).jca_class()
        );
        // And none of them may be an unchecked class — the whole point.
        for e in [
            RsaCipherError::BlockSize(String::new()),
            RsaCipherError::Padding(String::new()),
            RsaCipherError::Key(String::new()),
        ] {
            assert!(
                !e.jca_class().starts_with("java/lang/"),
                "{:?} must not be a java.lang (unchecked) exception",
                e
            );
        }
    }

    #[test]
    fn rsa_cipher_padding_from_transformation() {
        assert_eq!(
            RsaCipherPadding::from_transformation("PKCS1Padding"),
            Some(RsaCipherPadding::Pkcs1)
        );
        assert_eq!(
            RsaCipherPadding::from_transformation("OAEPWithSHA-1AndMGF1Padding"),
            Some(RsaCipherPadding::OaepSha1)
        );
        assert_eq!(
            RsaCipherPadding::from_transformation("OAEPWithSHA-256AndMGF1Padding"),
            Some(RsaCipherPadding::OaepSha256)
        );
        assert_eq!(RsaCipherPadding::from_transformation("NoPadding"), None);
    }

    #[test]
    fn rsa_pss_verify_hotspot_vectors() {
        // PS256/PS384/PS512 signatures produced by HotSpot's RSASSA-PSS SPI
        // (salt length == hash length, MGF1 over the same hash) over a fixed
        // 2048-bit key — validates EMSA-PSS-VERIFY against a real implementation.
        let n = from_hex("00e2887dc7a26dee90a6811cb259ee83a027a132771e3811a33768a6ef96a1090e793c4d042bfd04f52e8ae497a40a1b71a6acd8d451f35c7f6804d3c46a30e1f00d03b8542397f87aec655447c33f11998a07bf505dfc1c623148cbc6e2a1a9d88f44ed77ecb5813ae51c2db6043077223da796509e4158f5fa2f97cbebd28ad78dc9a7c5c48ed6131ee3cd897605ed771c7cd55d91dfb14eddc27164840803d67c9cb0ec3d7077d91921a3ea0b44c791b1b06fa1de4ea39cadca9a982704b30b3f07e35d1edd1c56d11907b44e46986b9f2edfce56111a4ab21d441bd884c7442aed05b99502bbc0171ba74ca08abac0dda1376ddcda9dca78124f7ee2284619");
        let e = from_hex("010001");
        let msg = from_hex("74686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f67");
        for (h, sig_hex) in [
            (PssHash::Sha256, "e12b6161e1f1912f87d64e4face0199d9b95b74bac37e2e257dd129e56fc618094031beb07f888577af2749ed933d171111640f824dad6b55fb3175304d6ee44420dcef4e6a0be6c5ae1f750aa8352d7302ac75bb3f6201fe73d5ba667427c467d0e3a93ea9eac799a105f23f6dd04e0bcc240517ef3fb58cfb2ad3fd6d6bd37ca917dfc5b1e72aa42defc90780434745f36e9cb11f7b932a063ea3d821a8fd008580c0103ca35df29409539bd62d1c7cdb42e26f857f5c9abe65f3057306bae225918d13b1780fc87fbe35b338b27b40619a7928edce8b17427b5232315538e755f6bfb051210f8f2af14d0bebe39914be2f44c0c992a4b32e4acaf9689ef95"),
            (PssHash::Sha384, "85a0d54722469e406e11391c33c0c63a5c931bc05c85f5e0e91c0f689dfdccf983e472eceae07e76f46b3643ae55a6b21e5595dfaefd2581d3f802146f785af290dbd08300a872a77883e16dbba5c6f2c43413ed9d6b6ba8406aed6c698f839acec64c6917d833f107b248afd1851074f80b535ac3c8f52dd4251d203d4b332e34d610ca86534305ea936798d1ca4313c1226f10da5f25bf3d99d2b7cb63e6d63990e3713284a3a746155ea8b1bd11c0fa44408aecf9bebb002b3c18d6cbe3fb03562c3d12d9c83127af0b2b48f1336f37dac7e8c904e83b897a2614eef1ba329cbca5183e661622b8f067dd9696568b5b24f9c38e56b704b6dd7d33c1c39bf7"),
            (PssHash::Sha512, "1aa9ecf5d993ccd238f23b07f08ab30af2298bbe1b1a26d85efdef2550a02429b58d14a8a4d6a4f27e9fbfa36f51ddb46bbd5b1fdc3921f2031b0aecc3ea245f76e94a482fe232db7a5b1aa8515e24165bab95dd3fa4ec03d55e98a95201f7f858865d8dc96bc46b863db5f9c1e11d07d238faef3dd419c4fe970d07c7e1c5d8d252b956198e9358e6b37a50417a69cd5353a3dc74765568038769fa32ac0758b2161aa1399ef1c4101a465dfa517f70e1d23d158359fa52e0f6ddc4b3f54ece116f24d6478701e6ebf7e2dcf19e97af6b0542237ddcac5b81cce62b6467502de9421a469f4f43ba048e05589860b3b7e4a9004a7b99ca557e2e85ba2a871f7f"),
        ] {
            let sig = from_hex(sig_hex);
            assert!(rsa_verify_pss(&n, &e, h, &msg, &sig), "{:?}: valid PSS sig", h);
            let mut bad = msg.clone();
            bad[0] ^= 1;
            assert!(!rsa_verify_pss(&n, &e, h, &bad, &sig), "{:?}: tampered msg", h);
        }
    }

    #[test]
    fn rsa_pss_sign_verify_round_trip() {
        let (public, private) = Rsa::generate_keypair(1024);
        let message = b"CratonVM RSASSA-PSS round trip";
        let signature = rsa_sign_pss(&private, PssHash::Sha256, message);
        assert_eq!(signature.len(), 128);
        assert!(rsa_verify_pss(
            &public.n.to_bytes_be(),
            &public.e.to_bytes_be(),
            PssHash::Sha256,
            message,
            &signature,
        ));
    }

    // RF.10: PKIX chain walker — helper unit coverage.

    // Build a minimal X509Cert with the fields `verify_cert_chain` reads.
    // We skip signature bytes (use an always-valid mock algorithm) by
    // routing subject/issuer through cert stubs and then exercising the
    // branches we own.
    fn mock_cert(subject: &str, issuer: &str, not_before: i64, not_after: i64) -> X509Cert {
        X509Cert {
            version: 3,
            serial_number: vec![0x01],
            sig_algorithm: "Unknown".to_string(), // forces verify_signature → false
            issuer_raw: issuer.as_bytes().to_vec(),
            issuer_cn: issuer.to_string(),
            subject_raw: subject.as_bytes().to_vec(),
            subject_cn: subject.to_string(),
            not_before,
            not_after,
            public_key_bytes: vec![],
            public_key_algorithm: "RSA".to_string(),
            signature_bytes: vec![],
            tbs_bytes: vec![],
            encoded: vec![],
        }
    }

    #[test]
    fn rf10_empty_chain_rejected() {
        let err = verify_cert_chain(&[], &[], 0).unwrap_err();
        assert_eq!(err, PkixError::EmptyChain);
    }

    #[test]
    fn rf10_chain_too_long_rejected() {
        // Use 11 mock certs — the limit is 10.
        let chain: Vec<X509Cert> = (0..11)
            .map(|i| mock_cert(&format!("c{i}"), &format!("c{}", i + 1), 0, i64::MAX))
            .collect();
        let err = verify_cert_chain(&chain, &[], 0).unwrap_err();
        assert_eq!(err, PkixError::ChainTooLong);
    }

    #[test]
    fn rf10_untrusted_root_rejected() {
        let leaf = mock_cert("leaf", "intermediate", 0, i64::MAX);
        // No anchors provided — validation must fail.
        let err = verify_cert_chain(&[leaf], &[], 0).unwrap_err();
        match err {
            PkixError::UntrustedRoot { subject } => assert_eq!(subject, "leaf"),
            other => panic!("expected UntrustedRoot, got {other:?}"),
        }
    }

    #[test]
    fn rf10_expired_cert_rejected() {
        // not_before in the past, not_after also in the past → expired.
        let expired = mock_cert("expired", "ca", 1_000, 2_000);
        let err = verify_cert_chain(&[expired], &[], 10_000).unwrap_err();
        match err {
            PkixError::Expired { subject } => assert_eq!(subject, "expired"),
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn rf10_pkix_error_display_messages() {
        assert_eq!(
            format!("{}", PkixError::EmptyChain),
            "empty certificate chain"
        );
        assert!(format!(
            "{}",
            PkixError::Expired {
                subject: "x".into()
            }
        )
        .contains("expired"));
        assert!(format!(
            "{}",
            PkixError::UntrustedRoot {
                subject: "y".into()
            }
        )
        .contains("trust anchor"));
        assert_eq!(
            format!("{}", PkixError::ChainTooLong),
            "chain exceeds maximum depth"
        );
    }

    #[test]
    fn rf10_subject_cn_matches_basic() {
        let c = mock_cert("root", "root", 0, i64::MAX);
        assert!(c.subject_cn_matches("root"));
        assert!(!c.subject_cn_matches("other"));

        let empty = mock_cert("", "", 0, i64::MAX);
        // An empty subject must not match anything — otherwise an unparsed
        // certificate would pretend to be a trust anchor.
        assert!(!empty.subject_cn_matches(""));
    }

    // RF.7: OS CSPRNG produces non-repeating output. `os_random_bytes` is
    // already exercised by `os_random_bytes_returns_distinct_bytes` above;
    // here we confirm that back-to-back draws produce independent streams.
    #[test]
    fn rf7_os_random_back_to_back_differs() {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        assert!(os_random_bytes(&mut a));
        assert!(os_random_bytes(&mut b));
        // Two 64-byte draws from a CSPRNG should effectively never match.
        assert_ne!(a, b, "consecutive CSPRNG draws must differ");
    }

    // B1: a tiny RSA modulus (256-bit → k=32, below tLen+11=62 for SHA-256)
    // must be rejected as a verification failure, NOT underflow the PKCS#1
    // v1.5 padding-length math (panic in debug / exabyte alloc in release).
    // Reachable from `verify_signature`/`checkServerTrusted` with an
    // attacker-supplied issuer key.
    #[test]
    fn b1_small_rsa_modulus_verify_rejects_without_panic() {
        let n = BigUint::from_bytes_be(&[0xFFu8; 32]); // 256-bit modulus
        let e = BigUint::from_bytes_be(&[0x01, 0x00, 0x01]);
        let key = RsaPublicKey { n, e };
        // Signature length must equal k (=32) to reach the encode step.
        let sig = vec![0x01u8; 32];
        assert!(
            !Rsa::verify_sha256(&key, b"anything", &sig),
            "small-modulus RSA verify must fail closed, not panic"
        );
    }

    // B1: signing with a too-small key returns an empty signature (treated as
    // a failure by callers) instead of underflowing the padding math.
    #[test]
    fn b1_small_rsa_modulus_sign_returns_empty() {
        let n = BigUint::from_bytes_be(&[0xFFu8; 32]);
        let d = BigUint::from_bytes_be(&[0x03]);
        let e = BigUint::from_bytes_be(&[0x01, 0x00, 0x01]);
        let key = RsaPrivateKey {
            n,
            d,
            e,
            p: None,
            q: None,
            dp: None,
            dq: None,
            qinv: None,
        };
        assert!(
            Rsa::sign_sha256(&key, b"anything").is_empty(),
            "sign with a sub-minimum modulus must yield an empty signature"
        );
    }

    // B2: a DER length field encoded in 8 bytes near usize::MAX must be
    // rejected (None), not wrap `total_hdr + len` and produce an inverted /
    // out-of-range slice panic.
    #[test]
    fn b2_der_oversized_length_rejected_without_panic() {
        // tag=0x30, long-form length: 0x88 (=> 8 length octets) all 0xFF,
        // then a couple of content bytes. The encoded length is ~usize::MAX.
        let mut data = vec![0x30u8, 0x88];
        data.extend_from_slice(&[0xFFu8; 8]);
        data.extend_from_slice(&[0x01, 0x02]);
        assert!(
            der_read_tag_length(&data).is_none(),
            "oversized DER length must be rejected, not panic"
        );
    }

    // B2: a length wider than a usize (9 length octets) is rejected outright.
    #[test]
    fn b2_der_overwide_length_field_rejected() {
        let mut data = vec![0x30u8, 0x89]; // 9 length octets — too wide
        data.extend_from_slice(&[0xFFu8; 9]);
        data.push(0x00);
        assert!(der_read_tag_length(&data).is_none());
    }

    // B2: a parseable but truncated certificate (length exceeds buffer) is a
    // clean parse error, not a panic.
    #[test]
    fn b2_truncated_der_is_parse_error() {
        // SEQUENCE claiming 100 content bytes but only 3 present.
        let data = vec![0x30u8, 100, 0x01, 0x02, 0x03];
        assert!(der_read_tag_length(&data).is_none());
    }

    // V1: full Name-DER matching — a cert whose subject DER differs must not
    // match even if a CN string would have collided.
    #[test]
    fn v1_subject_der_matches_uses_full_name() {
        let c = mock_cert("root", "root", 0, i64::MAX);
        // mock_cert sets subject_raw = subject.as_bytes().
        assert!(c.subject_der_matches(b"root"));
        assert!(!c.subject_der_matches(b"r00t"));
        // Empty subject Name never matches (an unparsed cert can't be anchor).
        let empty = mock_cert("", "", 0, i64::MAX);
        assert!(!empty.subject_der_matches(b""));
    }

    // -----------------------------------------------------------------------
    // nb-crypto-impl VULN(1) — RSA padding-oracle regression tests.
    //
    // Every PKCS#1 v1.5 and OAEP padding failure must collapse to the SAME
    // opaque error string (`RSA_PADDING_ERROR`); none may leak which structural
    // check failed. Valid padding must still round-trip.
    // -----------------------------------------------------------------------

    #[test]
    fn pkcs1_unpad_roundtrip_ok() {
        let k = 128; // RSA-1024 modulus byte length
        let msg = b"hello pkcs1";
        let em = rsa_pkcs1_type2_pad(msg, k).unwrap();
        assert_eq!(em.len(), k);
        assert_eq!(rsa_pkcs1_type2_unpad(&em).unwrap(), msg.to_vec());
    }

    #[test]
    fn pkcs1_unpad_all_failures_indistinguishable() {
        let k = 128;
        let good = rsa_pkcs1_type2_pad(b"x", k).unwrap();

        // Wrong leading byte.
        let mut a = good.clone();
        a[0] = 0x01;
        // Wrong block type.
        let mut b = good.clone();
        b[1] = 0x01;
        // No 0x00 separator anywhere after the type byte.
        let mut c = good.clone();
        for byte in c.iter_mut().skip(2) {
            *byte = 0xFF;
        }
        // Separator too early -> PS shorter than 8 bytes.
        let mut d = vec![0x00u8, 0x02, 0x01, 0x02, 0x00];
        d.extend(std::iter::repeat(0xAAu8).take(k - d.len()));
        // Too short to hold any valid padding.
        let e = vec![0x00u8, 0x02, 0x00];

        for bad in [&a, &b, &c, &d, &e] {
            let err = rsa_pkcs1_type2_unpad(bad).unwrap_err();
            assert_eq!(err, RSA_PADDING_ERROR, "PKCS1 error string leaked detail");
        }
    }

    #[test]
    fn oaep_unpad_roundtrip_ok() {
        let k = 256; // RSA-2048
        for pad in [RsaCipherPadding::OaepSha1, RsaCipherPadding::OaepSha256] {
            let msg = b"oaep payload";
            let em = rsa_oaep_pad(pad, msg, k).unwrap();
            assert_eq!(em.len(), k);
            assert_eq!(rsa_oaep_unpad(pad, &em).unwrap(), msg.to_vec());
        }
    }

    #[test]
    fn oaep_unpad_all_failures_indistinguishable() {
        let k = 256;
        let pad = RsaCipherPadding::OaepSha256;
        let good = rsa_oaep_pad(pad, b"y", k).unwrap();

        // Corrupt leading Y byte (must be 0x00) -> Manger oracle bait.
        let mut a = good.clone();
        a[0] ^= 0x01;
        // Corrupt the masked DB so the recovered lHash mismatches.
        let mut b = good.clone();
        let last = b.len() - 1;
        b[last] ^= 0xFF;
        // Corrupt the masked seed region (also perturbs DB unmask -> mismatch).
        let mut c = good.clone();
        c[1] ^= 0xFF;
        // Too short to hold `00 || seed || DB`.
        let d = vec![0u8; 2 * pad.hlen() + 1];

        for bad in [&a, &b, &c, &d] {
            let err = rsa_oaep_unpad(pad, bad).unwrap_err();
            assert_eq!(err, RSA_PADDING_ERROR, "OAEP error string leaked detail");
        }
    }

    #[test]
    fn ct_helpers_behave() {
        assert_eq!(ct_eq_u8(0x42, 0x42), 0xFF);
        assert_eq!(ct_eq_u8(0x42, 0x43), 0x00);
        assert_eq!(ct_is_nonzero_u8(0), 0x00);
        assert_eq!(ct_is_nonzero_u8(1), 0xFF);
        assert_eq!(ct_is_nonzero_u8(0xFF), 0xFF);
        assert_eq!(ct_eq_bytes(b"abc", b"abc"), 0xFF);
        assert_eq!(ct_eq_bytes(b"abc", b"abd"), 0x00);
        assert_eq!(ct_eq_bytes(b"abc", b"ab"), 0x00);
    }

    // -----------------------------------------------------------------------
    // Migration off the ambiguous `bool` wrapper.
    //
    // `Rsa::verify_sha256` was the last call site of
    // `native-builtins-crypto`'s `verify_rsa_pkcs1_v15` (the `-> bool` form
    // the crypto-failure contract flags as ambiguous). `rsa_verify` — the
    // backend behind `Signature.verify()` — now goes through the *checked*
    // form so that "the backend refused this key" and "the signature does not
    // match" are two different answers.
    // -----------------------------------------------------------------------

    #[test]
    fn rsa_verify_separates_a_refused_key_from_a_failed_verification() {
        // MUST STILL WORK: a real key round-trips, and a tampered signature is
        // a plain `Some(false)` — the genuine negative, preserved.
        let (public_key, private_key) = Rsa::generate_keypair(1024);
        let good_n = public_key.n.clone();
        let id = rsa_key_next_id();
        rsa_key_store(
            id,
            RsaKeyPairData {
                public_key,
                private_key,
            },
        );
        let msg = b"a message worth signing";
        let sig = rsa_sign(id, msg).expect("a registered key must sign");
        assert_eq!(rsa_verify(id, msg, &sig), Some(true));

        let mut tampered = sig.clone();
        tampered[0] ^= 0xff;
        assert_eq!(
            rsa_verify(id, msg, &tampered),
            Some(false),
            "a real digest mismatch is the answer to the question asked"
        );
        assert_eq!(rsa_verify(id, b"a different message", &sig), Some(false));

        // MUST BE UNANSWERABLE: an even public exponent is rejected by the
        // backend before any RSA operation runs. Collapsed to `false` before
        // this migration, which the caller could not tell from a forgery.
        let (_ignored_pub, refused_priv) = Rsa::generate_keypair(1024);
        let refused_id = rsa_key_next_id();
        rsa_key_store(
            refused_id,
            RsaKeyPairData {
                public_key: RsaPublicKey {
                    n: good_n,
                    e: BigUint::from_u64(4), // even — not a valid RSA exponent
                },
                private_key: refused_priv,
            },
        );
        assert_eq!(
            rsa_verify(refused_id, msg, &sig),
            None,
            "a key the backend refuses must not answer the verification question"
        );

        // And an unregistered handle stays `None`, as before.
        assert_eq!(rsa_verify(u64::MAX, msg, &sig), None);
    }

    /// The `bool` surface that certificate-chain validation still uses must
    /// stay fail-closed: neither a refusal nor a mismatch may become `true`.
    #[test]
    fn the_bool_verify_surface_is_still_fail_closed() {
        let (public_key, private_key) = Rsa::generate_keypair(1024);
        let msg = b"chain-validation payload";
        let sig = Rsa::sign_sha256(&private_key, msg);
        assert!(Rsa::verify_sha256(&public_key, msg, &sig));

        let mut tampered = sig.clone();
        tampered[0] ^= 0xff;
        assert!(!Rsa::verify_sha256(&public_key, msg, &tampered));

        let refused = RsaPublicKey {
            n: public_key.n.clone(),
            e: BigUint::from_u64(4),
        };
        assert!(
            !Rsa::verify_sha256(&refused, msg, &sig),
            "an Err must never surface as true"
        );
        assert!(Rsa::try_verify_sha256(&refused, msg, &sig).is_err());
    }
}
