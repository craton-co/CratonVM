// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

    /// AES-GCM encrypt with a 128-bit IV, J0 derived by GHASH over the IV
    /// (NIST SP 800-38D 7.1), as SunJCE does for any IV that is not 96 bits.
    /// JSSE's stateless session tickets (`SessionTicketExtension`) use exactly
    /// this: a 16-byte IV and a 128-bit tag.
    pub fn encrypt_iv16(key: &AesKey, iv: &[u8; 16], plaintext: &[u8], aad: &[u8]) -> AesGcmOutput {
        use aes::cipher::consts::U16;
        use aes_gcm::{aead::AeadInPlace, AesGcm as AesGcmAead};
        let nonce_arr = aes_gcm::Nonce::<U16>::from_slice(iv);
        let mut buf = plaintext.to_vec();
        let tag_arr = match &key.cipher {
            AesCipher::Aes128(c) => {
                let gcm: AesGcmAead<aes::Aes128, U16> = c.clone().into();
                gcm.encrypt_in_place_detached(nonce_arr, aad, &mut buf)
            }
            AesCipher::Aes192(c) => {
                let gcm: AesGcmAead<aes::Aes192, U16> = c.clone().into();
                gcm.encrypt_in_place_detached(nonce_arr, aad, &mut buf)
            }
            AesCipher::Aes256(c) => {
                let gcm: AesGcmAead<aes::Aes256, U16> = c.clone().into();
                gcm.encrypt_in_place_detached(nonce_arr, aad, &mut buf)
            }
        }
        .expect("AES-GCM (128-bit IV) detached encrypt: buffer never grows");
        let mut tag = [0u8; 16];
        tag.copy_from_slice(tag_arr.as_slice());
        AesGcmOutput {
            ciphertext: buf,
            tag,
        }
    }

    /// The decrypt half of [`AesGcm::encrypt_iv16`].
    pub fn decrypt_iv16(
        key: &AesKey,
        iv: &[u8; 16],
        ciphertext: &[u8],
        aad: &[u8],
        tag: &[u8; 16],
    ) -> Result<Vec<u8>, CryptoError> {
        use aes::cipher::consts::U16;
        use aes_gcm::{aead::AeadInPlace, AesGcm as AesGcmAead};
        let nonce_arr = aes_gcm::Nonce::<U16>::from_slice(iv);
        let tag_arr = aes_gcm::Tag::<U16>::from_slice(tag);
        let mut buf = ciphertext.to_vec();
        let res = match &key.cipher {
            AesCipher::Aes128(c) => {
                let gcm: AesGcmAead<aes::Aes128, U16> = c.clone().into();
                gcm.decrypt_in_place_detached(nonce_arr, aad, &mut buf, tag_arr)
            }
            AesCipher::Aes192(c) => {
                let gcm: AesGcmAead<aes::Aes192, U16> = c.clone().into();
                gcm.decrypt_in_place_detached(nonce_arr, aad, &mut buf, tag_arr)
            }
            AesCipher::Aes256(c) => {
                let gcm: AesGcmAead<aes::Aes256, U16> = c.clone().into();
                gcm.decrypt_in_place_detached(nonce_arr, aad, &mut buf, tag_arr)
            }
        };
        res.map_err(|_| CryptoError::AuthenticationFailed)?;
        Ok(buf)
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
pub fn secure_random_fill(_key: i32, buf: &mut [u8]) {
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
pub fn chacha20_xor(key: &[u8; 32], nonce: &[u8; 12], initial_counter: u32, buf: &mut [u8]) {
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

pub fn register_crypto_impl_natives(r: &mut NativeMethodRegistry) {
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
    // default CLI build, and `--fast-boot` / `--real-jdk` are served by
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
        native_secure_random_next_bytes, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/security/SecureRandom",
        "generateSeed",
        "(I)[B",
        native_secure_random_generate_seed, cratonvm_native_api::NativeKind::Bridge,
    );
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
            limbs: mont.out_of_mont(&acc),
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
    // http-server-sslengine-identity-singleton-clobber-FIXED.md.
    // Convention: `p > q`, matching JDK's `RSAKeyPairGenerator`.
    pub p: Option<BigUint>,
    pub q: Option<BigUint>,
    pub dp: Option<BigUint>,   // d mod (p-1)
    pub dq: Option<BigUint>,   // d mod (q-1)
    pub qinv: Option<BigUint>, // q^{-1} mod p
}

impl Drop for RsaPrivateKey {
    fn drop(&mut self) {
        // Zeroize private key material.
        //
        // `fill(0)` rather than the equivalent `for limb in .. { *limb = 0 }`
        // only because `cargo clippy -- -D warnings` began refusing the loop
        // form (`manual_slice_fill`, stable 1.98) and was red on `dev` for
        // every push. Semantics are identical, and so is what the optimiser is
        // allowed to do with either: neither form is a GUARANTEED zeroization —
        // that needs a volatile write or the `zeroize` crate — so this is a
        // lint fix, not a strengthening of the property above.
        self.d.limbs.fill(0);
        self.n.limbs.fill(0);
        for crt in [
            &mut self.p,
            &mut self.q,
            &mut self.dp,
            &mut self.dq,
            &mut self.qinv,
        ] {
            if let Some(v) = crt {
                v.limbs.fill(0);
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
        let hash = Self::digest_for_pkcs1v15(digest, message);
        let k = (key.n.bit_length() + 7) / 8;
        let Some(em) = Self::pkcs1v15_encode_digest(digest, &hash, k) else {
            return Vec::new();
        };
        let m = BigUint::from_bytes_be(&em);
        rsa_private_op_blinded(key, &m).to_bytes_be_padded(k)
    }

    /// The digest bytes for a PKCS#1 v1.5 signature, for every algorithm the
    /// `SunRsaSign` provider advertises.
    ///
    /// MD2 and MD5 come from this crate's own `real_md2` / `real_md5` — the
    /// implementations `W7-63-jca-advertise-vs-serve.md` §3 #1 added and
    /// adjudicated against HotSpot on ten messages including the three MD2
    /// padding boundaries — and the SHA-3 family from the `sha3` dependency
    /// that same section notes was "already a dependency". None of this is new
    /// cryptography; the nine names failed at `sign()` for want of an arm.
    pub fn digest_for_pkcs1v15(
        digest: cratonvm_native_builtins_crypto::signature::DigestAlgorithm,
        message: &[u8],
    ) -> Vec<u8> {
        use cratonvm_native_builtins_crypto::signature::DigestAlgorithm as D;
        use sha3::Digest as _;
        match digest {
            D::Md2 => crate::real_md2(message),
            D::Md5 => crate::real_md5(message),
            D::Sha1 => {
                use sha1::Digest;
                let mut h = sha1::Sha1::new();
                h.update(message);
                h.finalize().to_vec()
            }
            D::Sha224 => sha2::Sha224::digest(message).to_vec(),
            D::Sha256 => Sha256::digest(message).to_vec(),
            D::Sha384 => Sha384::digest(message).to_vec(),
            D::Sha512 => Sha512::digest(message).to_vec(),
            D::Sha512_224 => sha2::Sha512_224::digest(message).to_vec(),
            D::Sha512_256 => sha2::Sha512_256::digest(message).to_vec(),
            D::Sha3_224 => sha3::Sha3_224::digest(message).to_vec(),
            D::Sha3_256 => sha3::Sha3_256::digest(message).to_vec(),
            D::Sha3_384 => sha3::Sha3_384::digest(message).to_vec(),
            D::Sha3_512 => sha3::Sha3_512::digest(message).to_vec(),
        }
    }

    fn pkcs1v15_encode_digest(
        digest: cratonvm_native_builtins_crypto::signature::DigestAlgorithm,
        hash: &[u8],
        k: usize,
    ) -> Option<Vec<u8>> {
        // GENERATED, not transcribed. This carried its own copy of RFC 8017
        // §9.2 note 1's four hex blobs, which is one transcription per digest
        // and a second copy of knowledge the verify side also held (through
        // the `rsa` crate's `Pkcs1v15Sign::new::<D>()`). Both now come from
        // `DigestAlgorithm::pkcs1v15_digest_info_prefix`, whose own test
        // asserts it reproduces the published blobs byte for byte — so sign
        // and verify cannot drift, and adding a digest is an OID and a length.
        let digest_info_prefix = digest.pkcs1v15_digest_info_prefix();
        let t_len = digest_info_prefix.len() + hash.len();
        // Need: 00 01 || PS(>=8 bytes of FF) || 00 || T  => k >= t_len + 11.
        // `ps_len = k - t_len - 3` must be >= 8, equivalently k >= t_len + 11.
        let ps_len = k.checked_sub(t_len + 3).filter(|&ps| ps >= 8)?;
        let mut em = Vec::with_capacity(k);
        em.push(0x00);
        em.push(0x01);
        em.extend(std::iter::repeat(0xff).take(ps_len));
        em.push(0x00);
        em.extend_from_slice(&digest_info_prefix);
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
    /// http-server-sslengine-identity-singleton-clobber-FIXED.md.
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
mod named_curve_param_tests;

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
mod named_curve_openssl_tests;

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
    // Hash HERE and use the prehashed entry point, because this crate is the
    // one that can compute all thirteen digests — `native-builtins-crypto`
    // depends on nothing of ours and carries only the four it can hash itself.
    // The padding, which is the half that must not be duplicated, stays over
    // there; `digest_for_pkcs1v15` is the only thing this side adds.
    let hash = Rsa::digest_for_pkcs1v15(digest, message);
    match cratonvm_native_builtins_crypto::signature::verify_rsa_pkcs1_v15_prehashed(
        &n, &e, digest, &hash, signature,
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
/// Keys were therefore `(vm_identity, identity_hash_code)`.
///
/// gc-common w17-b (`common-w16b-jca-signature-and-rsa-realkey-tables-are-never-pruned`):
/// that pair was never pruned, not even at VM teardown -- one row per real RSA
/// key ever bridged -- and a dead key's row was found by the next key object
/// with its identity hash, which then signed with the dead key's material. The
/// key is now the key object's lib.rs lock key
/// ([`rsa_realkey_key_for`], `gc_stable_weak_lock_key`): VM-scoped, unique
/// across VMs, never re-minted, re-addressed after every moving collection, and
/// freed at the first sweep that finds the key object dead (or at its VM's
/// teardown); the sweep then calls [`forget_rsa_realkey_keys`].
///
/// GC: no `ObjectRef` is stored (key = a lock key, value = a `u64`
/// `crypto_impl` handle), so this table needs no collector scan/remap
/// companion.
static RSA_REALKEY_MAP: parking_lot::RwLock<Option<HashMap<RsaRealKeyKey, u64>>> =
    parking_lot::RwLock::new(None);

/// The real RSA key object's lock key (`crate::gc_stable_weak_lock_key`).
type RsaRealKeyKey = usize;

/// The `RSA_REALKEY_MAP` key of the real RSA key object `key`. Mint it while
/// `key` is current (pinned, or with no allocation since it was read); the key
/// itself then survives any later move.
pub fn rsa_realkey_key_for(ctx: &dyn NativeContext, key: ObjectRef) -> RsaRealKeyKey {
    crate::gc_stable_weak_lock_key(ctx, key)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

/// Register `key_id` for the real RSA key object `key` of the calling VM.
/// `key` must be current (see [`rsa_realkey_key_for`]).
pub fn rsa_realkey_map_set(ctx: &dyn NativeContext, key: ObjectRef, key_id: u64) {
    rsa_realkey_map_insert(rsa_realkey_key_for(ctx, key), key_id);
}

/// Register `key_id` under a key [`rsa_realkey_key_for`] minted earlier.
///
/// gc-common w18-e: the key object also becomes an OWNER of the handle
/// ([`key_handle_own`]), so the `RSA_KEY_STORE` row goes once every object
/// carrying it is dead. A row this replaces gives up its old handle.
pub fn rsa_realkey_map_insert(key: RsaRealKeyKey, key_id: u64) {
    let previous = {
        let mut guard = RSA_REALKEY_MAP.write();
        guard.get_or_insert_with(HashMap::new).insert(key, key_id)
    };
    key_handle_own(key, KeyStoreKind::Rsa, key_id);
    if let Some(prev) = previous.filter(|&p| p != key_id) {
        key_handle_disown(key, KeyStoreKind::Rsa, prev);
    }
}

/// Look up the `crypto_impl` key handle registered for the key object `key`
/// of the calling VM. `key` must be current. An empty map answers without
/// minting a lock key for `key` (most lookups are of synthetic keys, which are
/// never registered).
pub fn rsa_realkey_map_get(ctx: &dyn NativeContext, key: ObjectRef) -> Option<u64> {
    if RSA_REALKEY_MAP.read().as_ref().is_none_or(|m| m.is_empty()) {
        return None;
    }
    let k = rsa_realkey_key_for(ctx, key);
    let guard = RSA_REALKEY_MAP.read();
    guard.as_ref().and_then(|m| m.get(&k).copied())
}

/// Drop the `RSA_REALKEY_MAP` rows filed under `keys`, which the lock-key
/// registry has just freed (the key object died, or its VM is being torn
/// down) and will never mint again. Called by `lib.rs::sweep_lock_keys` and
/// `lib.rs::forget_vm_lock_keys` with the registry lock released.
///
/// The `crypto_impl` key handle a row names is NOT released here: the public
/// and the private key object of one generated pair share a handle
/// (`jca::key_factory::real_rsa_key_from_components` is called for both with
/// the pair's `key_id`), so the handle outlives either object. Since
/// gc-common w18-e each object is an owner of the handle, and
/// [`forget_key_handle_owner_keys`] (the same hooks, the same freed keys)
/// frees the handle with its LAST owner.
pub fn forget_rsa_realkey_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let mut guard = RSA_REALKEY_MAP.write();
    let Some(m) = guard.as_mut() else {
        return;
    };
    if m.is_empty() {
        return;
    }
    for k in keys {
        m.remove(k);
    }
}

/// Whether `RSA_REALKEY_MAP` holds a row under `key` (gc-common w17-b tests).
#[cfg(test)]
pub fn rsa_realkey_map_contains_for_test(key: RsaRealKeyKey) -> bool {
    RSA_REALKEY_MAP
        .read()
        .as_ref()
        .is_some_and(|m| m.contains_key(&key))
}

// ---------------------------------------------------------------------------
// gc-common w18-e (`common-w17b-crypto-impl-key-stores-are-never-freed`):
// key-store handle OWNERS.
//
// `RSA_KEY_STORE`, `ECDSA_KEY_STORE` and `ED25519_KEY_STORE` hold full key
// pairs (private halves included) under process-counter handles, and nothing
// used to remove a row: every `KeyStore.getKey` (a fresh handle per call),
// every import and every generated pair stayed in process memory for the life
// of the process, across VM teardown too.
//
// A handle's lifetime is now tied to the Java objects that can still name it.
// Each such object is an OWNER, registered under its weak lock key
// (`crate::gc_stable_weak_lock_key`):
//
// * a key object carrying the handle -- a real RSA key through its
//   `RSA_REALKEY_MAP` row ([`rsa_realkey_map_insert`]), a synthetic key
//   through its slot 3 (`jca::key_factory::alloc_{public,private}_key`). The
//   two halves of one pair share a handle, so the handle goes with the LAST
//   of them;
// * a `Signature` that copied the handle at `init*`
//   (`jca::signature::set_sig_keyid`). On a real-JDK `Signature` nothing else
//   keeps the key object alive between `init` and `sign`, and HotSpot keeps
//   the key through the SPI, so `init(key); key = null; gc; sign()` must work.
//
// A `Cipher`'s `rsa_key_id` copy is NOT an owner: a handle missing from the
// store makes `rsa_cipher_decrypt_by_id` answer `None`, and the caller falls
// back to the `(n, d)` components it captured at `init` (same result, no CRT).
//
// When the lock-key registry frees an owner's key (the object died, or its VM
// is torn down), `lib.rs::sweep_lock_keys` / `forget_vm_lock_keys` call
// [`forget_key_handle_owner_keys`], which drops the owner's rows and removes
// every handle whose count reached zero from its store.
//
// Tracking starts with the PRODUCER: every site that stores a handle for a
// Java key object takes a [`KeyHandleHold`] right after the `*_key_store`
// insert and keeps it until its key objects are registered. That makes the
// handle tracked (only a tracked handle can gain owners, see
// [`key_handle_own`]); it survives an allocation in between that collects an
// earlier owner (the real public key of a pair whose private half then falls
// back to a synthetic key); and a producer that fails before any owner exists
// frees the handle when the hold drops.
//
// A handle no producer held (a unit test's, or one minted by code not
// converted) is untracked, gains no owners and is never freed: the old
// behaviour. A number that merely LOOKS like a handle (a `Signature` copying
// whatever slot 3 held) therefore cannot shorten another key's life.
//
// LOCK ORDER: `key_handle_owners()` is taken first and a store lock under it
// (to remove a handle whose count reached zero in the same critical section,
// so no owner can be added between the verdict and the removal). No path
// takes a store lock and then this one.
// ---------------------------------------------------------------------------

/// Which `crypto_impl` key store a handle names. The three stores number their
/// handles from independent counters, so a bare `u64` is ambiguous.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KeyStoreKind {
    Rsa,
    Ecdsa,
    Ed25519,
}

#[derive(Default)]
struct KeyHandleOwners {
    /// Owner lock key -> the handles it keeps alive (no duplicates).
    by_owner: rustc_hash::FxHashMap<usize, Vec<(KeyStoreKind, u64)>>,
    /// Handle -> the owner rows and live [`KeyHandleHold`]s naming it.
    counts: rustc_hash::FxHashMap<(KeyStoreKind, u64), u32>,
}

/// Leaf under the lock order above: never held across a VM call.
fn key_handle_owners() -> &'static parking_lot::Mutex<KeyHandleOwners> {
    static T: std::sync::OnceLock<parking_lot::Mutex<KeyHandleOwners>> = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(KeyHandleOwners::default()))
}

#[cfg(test)]
fn key_store_contains(kind: KeyStoreKind, id: u64) -> bool {
    match kind {
        KeyStoreKind::Rsa => RSA_KEY_STORE
            .read()
            .as_ref()
            .is_some_and(|m| m.contains_key(&id)),
        KeyStoreKind::Ecdsa => ECDSA_KEY_STORE
            .read()
            .as_ref()
            .is_some_and(|m| m.contains_key(&id)),
        KeyStoreKind::Ed25519 => ED25519_KEY_STORE
            .read()
            .as_ref()
            .is_some_and(|m| m.contains_key(&id)),
    }
}

fn key_store_remove(kind: KeyStoreKind, id: u64) {
    match kind {
        KeyStoreKind::Rsa => {
            if let Some(m) = RSA_KEY_STORE.write().as_mut() {
                m.remove(&id);
            }
        }
        KeyStoreKind::Ecdsa => {
            if let Some(m) = ECDSA_KEY_STORE.write().as_mut() {
                m.remove(&id);
            }
        }
        KeyStoreKind::Ed25519 => {
            if let Some(m) = ED25519_KEY_STORE.write().as_mut() {
                m.remove(&id);
            }
        }
    }
}

/// One count off `handle`; at zero the count row and the store row go. Called
/// with the owners lock held (see the lock order above).
fn key_handle_release_locked(o: &mut KeyHandleOwners, handle: (KeyStoreKind, u64)) {
    let Some(n) = o.counts.get_mut(&handle) else {
        return;
    };
    *n = n.saturating_sub(1);
    if *n == 0 {
        o.counts.remove(&handle);
        key_store_remove(handle.0, handle.1);
    }
}

/// The object whose weak lock key is `owner` carries handle `id` of `kind`:
/// keep the handle while the object lives.
///
/// Only a TRACKED handle -- one its producer took a [`KeyHandleHold`] on, and
/// that some owner or hold still keeps -- can gain an owner. An untracked
/// handle (a unit test's, one minted by code not converted, or a number read
/// off a slot that names no handle at all) is left exactly as before: never
/// counted, so never freed by an owner that happened to name it. A no-op
/// too for handle 0 and for a handle `owner` already owns.
pub fn key_handle_own(owner: usize, kind: KeyStoreKind, id: u64) {
    if id == 0 {
        return;
    }
    let mut guard = key_handle_owners().lock();
    let KeyHandleOwners { by_owner, counts } = &mut *guard;
    let Some(count) = counts.get_mut(&(kind, id)) else {
        return;
    };
    let owned = by_owner.entry(owner).or_default();
    if owned.contains(&(kind, id)) {
        return;
    }
    owned.push((kind, id));
    *count += 1;
}

/// [`key_handle_own`] for a key object `obj` that is current (pinned, or with
/// no allocation since it was read).
pub fn key_handle_own_obj(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    kind: KeyStoreKind,
    id: u64,
) {
    if id == 0 {
        return;
    }
    let owner = crate::gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"));
    key_handle_own(owner, kind, id);
}

/// `owner` no longer names handle `id` of `kind` (its row was replaced).
pub fn key_handle_disown(owner: usize, kind: KeyStoreKind, id: u64) {
    let mut guard = key_handle_owners().lock();
    let o: &mut KeyHandleOwners = &mut guard;
    let emptied = {
        let Some(owned) = o.by_owner.get_mut(&owner) else {
            return;
        };
        let Some(i) = owned.iter().position(|h| *h == (kind, id)) else {
            return;
        };
        owned.swap_remove(i);
        owned.is_empty()
    };
    if emptied {
        o.by_owner.remove(&owner);
    }
    key_handle_release_locked(o, (kind, id));
}

/// `owner` (a `Signature`) now names exactly `handle` -- or nothing -- in place
/// of whatever it named before (a re-`init` with another key).
pub fn key_handle_replace_owned(owner: usize, handle: Option<(KeyStoreKind, u64)>) {
    let handle = handle.filter(|&(_, id)| id != 0);
    let old: Vec<(KeyStoreKind, u64)> = {
        let mut guard = key_handle_owners().lock();
        let o: &mut KeyHandleOwners = &mut guard;
        let unchanged = match (o.by_owner.get(&owner), handle) {
            (Some(owned), Some(h)) => owned.len() == 1 && owned[0] == h,
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        o.by_owner.remove(&owner).unwrap_or_default()
    };
    // Own the new handle before releasing the old ones, so a re-`init` with
    // the same key never passes through a zero count.
    if let Some((kind, id)) = handle {
        key_handle_own(owner, kind, id);
    }
    if old.is_empty() {
        return;
    }
    let mut guard = key_handle_owners().lock();
    let o: &mut KeyHandleOwners = &mut guard;
    for h in old {
        key_handle_release_locked(o, h);
    }
}

/// Drop the owner rows filed under `keys`, which the lock-key registry has
/// just freed (the object died, or its VM is being torn down) and will never
/// mint again, and free every handle whose last owner that was. Called by
/// `lib.rs::sweep_lock_keys` and `lib.rs::forget_vm_lock_keys` with the
/// registry lock released.
pub fn forget_key_handle_owner_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let mut guard = key_handle_owners().lock();
    let o: &mut KeyHandleOwners = &mut guard;
    if o.by_owner.is_empty() {
        return;
    }
    for k in keys {
        if let Some(owned) = o.by_owner.remove(k) {
            for h in owned {
                key_handle_release_locked(o, h);
            }
        }
    }
}

/// A producer's claim on a handle it just stored, from the store insert until
/// the key objects that carry it are registered as owners (see the section
/// note). Dropping it frees the handle if no owner took it.
#[must_use = "a hold released at once frees the handle it was meant to keep"]
pub struct KeyHandleHold {
    kind: KeyStoreKind,
    id: u64,
}

/// Take a [`KeyHandleHold`] on handle `id` of `kind`. Take it right after
/// the `*_key_store` insert.
pub fn hold_key_handle(kind: KeyStoreKind, id: u64) -> KeyHandleHold {
    if id != 0 {
        *key_handle_owners()
            .lock()
            .counts
            .entry((kind, id))
            .or_insert(0) += 1;
    }
    KeyHandleHold { kind, id }
}

impl Drop for KeyHandleHold {
    fn drop(&mut self) {
        if self.id == 0 {
            return;
        }
        let mut guard = key_handle_owners().lock();
        key_handle_release_locked(&mut guard, (self.kind, self.id));
    }
}

/// Owner rows naming handle `id` of `kind`, plus live holds (tests).
#[cfg(test)]
pub fn key_handle_count_for_test(kind: KeyStoreKind, id: u64) -> u32 {
    key_handle_owners()
        .lock()
        .counts
        .get(&(kind, id))
        .copied()
        .unwrap_or(0)
}

/// Whether handle `id` of `kind` is still in its store (tests).
#[cfg(test)]
pub fn key_store_contains_for_test(kind: KeyStoreKind, id: u64) -> bool {
    key_store_contains(kind, id)
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

// gc-common w18-e: the C18 Signature payload store (`SIG_DATA_STORE`, keyed by
// a bare identity hash, and `SIG_DATA_STORE_RAW_PTR`, keyed by a raw pointer)
// and its six `sig_data_*` functions are deleted. They had no caller anywhere
// in the workspace (`jca::signature` keeps the payload in `sig_payload_table`,
// keyed by the receiver's lock key), and their own doc comment asked for the
// block to be deleted rather than wired up again: both keys were VM-unscoped
// and neither table was ever pruned.

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
mod tests;

/// gc-common w18-e (`common-w17b-crypto-impl-key-stores-are-never-freed`).
/// Key-store handles owned by key objects and `Signature`s. The mock's
/// identity hash is the address truncated to `i32`; fake addresses end in 0
/// and are never dereferenced (only lock keys are minted for them). Every test
/// mints its own handles from the real counters and has its own VM
/// identities, forgotten by a guard: the stores and the lock-key registry are
/// process-wide.
#[cfg(test)]
mod w18e_key_store_owner_tests;
