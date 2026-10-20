// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Kernels behind the reviewed `Intrinsic`s over the JDK's OWN crypto engines
//! (SBR wave 5, lane `cryptoeng`).
//!
//! # What these are
//!
//! HotSpot replaces a fixed set of `@IntrinsicCandidate` methods in the SUN
//! and SunJCE providers with hand-written stubs: the digest compression
//! functions (`SHA.implCompress0`, `SHA2.implCompress0`, `SHA5.implCompress0`,
//! `SHA3.implCompress0`, `MD5.implCompress0`, and the multi-block loop
//! `DigestBase.implCompressMultiBlock0`), the AES block functions
//! (`AESCrypt.implEncryptBlock` / `implDecryptBlock`), the AES modes
//! (`CounterMode.implCrypt`, `CipherBlockChaining.implEncrypt` / `implDecrypt`,
//! `ElectronicCodeBook.implECBEncrypt` / `implECBDecrypt`), GCM's
//! `GHASH.processBlocks`, `ChaCha20Cipher.implChaCha20Block` and
//! `Poly1305.processMultipleBlocks`. Everything else in those providers —
//! buffering, padding, length encoding, tag handling, the `Cipher` / `Mac` /
//! `MessageDigest` state machines — stays the JDK's bytecode.
//!
//! Each kernel here is a pure function over the SAME state the Java method
//! reads and writes (the `state` words of a digest, the `K` round-key array
//! `AESCrypt.makeSessionKey` built, the `long[]` GHASH state, the 26-bit
//! Poly1305 limbs), so the Java code before and after the call sees exactly
//! what its own bytecode would have left. No VM or heap dependency: the
//! `native-builtins` facade (`crypto_intrinsics.rs`) marshals the fields and
//! arrays and registers the natives.
//!
//! # Exactness
//!
//! * Digests: FIPS 180-4 / RFC 1321 / FIPS 202 compression functions — the
//!   Java bodies compute exactly these, word for word.
//! * AES: the round keys are taken from the Java `K` array AS GIVEN (never
//!   re-derived from a key), so the result is the Java T-table computation for
//!   ANY `K`, including one that was not produced by `makeSessionKey`. The
//!   AES-NI path and the portable T-table port compute the same function
//!   (`aesenc` = SubBytes, ShiftRows, MixColumns, AddRoundKey = one Java
//!   `T1..T4` round; `aesdec` = one Java `T5..T8` round of the equivalent
//!   inverse cipher, whose keys `makeSessionKey` stores InvMixColumn'd).
//! * GHASH: GF(2^128) with the GCM bit order; `GHASH.blockMult` is the
//!   textbook right-shift algorithm, the `ghash` crate computes the same
//!   product with carry-less multiplication. A continued state is folded into
//!   the first block (`(S ^ X1)·H`), which is the Java recurrence.
//! * Poly1305: a line-by-line port of `IntegerPolynomial1305`'s `encode`,
//!   `setSum`, `setProduct` (`mult` + `carryReduce`) and `reduce`, including
//!   the `numAdds` bookkeeping, so the LIMBS (not just their value mod p) are
//!   the ones the bytecode would leave.
//! * ChaCha20: RFC 8439 §2.3 with the JDK's quirk that words 0..3 are the
//!   constants, not `initState[0..3]`.

use sha2::digest::generic_array::GenericArray;

// ---------------------------------------------------------------------------
// Digests
// ---------------------------------------------------------------------------

/// SHA-1 compression over `data` (a whole number of 64-byte blocks).
/// `SHA.implCompress0`'s body, block by block.
pub fn sha1_blocks(state: &mut [u32; 5], data: &[u8]) {
    for block in data.chunks_exact(64) {
        sha1::compress(
            state,
            core::slice::from_ref(GenericArray::from_slice(block)),
        );
    }
}

/// SHA-224/256 compression. `SHA2.implCompress0`'s body.
pub fn sha256_blocks(state: &mut [u32; 8], data: &[u8]) {
    for block in data.chunks_exact(64) {
        sha2::compress256(
            state,
            core::slice::from_ref(GenericArray::from_slice(block)),
        );
    }
}

/// SHA-384/512(/224,/256) compression. `SHA5.implCompress0`'s body.
pub fn sha512_blocks(state: &mut [u64; 8], data: &[u8]) {
    for block in data.chunks_exact(128) {
        sha2::compress512(
            state,
            core::slice::from_ref(GenericArray::from_slice(block)),
        );
    }
}

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// MD5 compression (RFC 1321). `MD5.implCompress0`'s body.
pub fn md5_blocks(state: &mut [u32; 4], data: &[u8]) {
    for block in data.chunks_exact(64) {
        let mut x = [0u32; 16];
        for (i, w) in x.iter_mut().enumerate() {
            *w = u32::from_le_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        let [mut a, mut b, mut c, mut d] = *state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let t = a
                .wrapping_add(f)
                .wrapping_add(MD5_K[i])
                .wrapping_add(x[g])
                .rotate_left(MD5_S[i]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(t);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }
}

/// SHA-3 absorb: `SHA3.implCompress0`'s body for every `block_size`-byte
/// block of `data` — XOR `block_size / 8` little-endian lanes into the state,
/// then Keccak-f[1600].
pub fn sha3_blocks(state: &mut [u64; 25], data: &[u8], block_size: usize) {
    if block_size == 0 {
        return;
    }
    let lanes = (block_size / 8).min(25);
    for block in data.chunks_exact(block_size) {
        for (i, lane) in state.iter_mut().enumerate().take(lanes) {
            let mut w = [0u8; 8];
            w.copy_from_slice(&block[8 * i..8 * i + 8]);
            *lane ^= u64::from_le_bytes(w);
        }
        keccak::f1600(state);
    }
}

/// SHA-3 absorb for the JDK 17/21 layout, where `SHA3.state` is a
/// `byte[200]` and `lanes` a `long[25]` scratch: `implCompress0` XORs
/// `block_size` bytes into `state`, and `keccak()` converts to lanes,
/// permutes, and writes both back. Same function as [`sha3_blocks`] over the
/// little-endian view of the bytes.
pub fn sha3_blocks_bytes(
    state: &mut [u8; 200],
    lanes: &mut [u64; 25],
    data: &[u8],
    block_size: usize,
) {
    if block_size == 0 || block_size > 200 {
        return;
    }
    for block in data.chunks_exact(block_size) {
        for (s, b) in state.iter_mut().zip(block.iter()) {
            *s ^= *b;
        }
        for (i, lane) in lanes.iter_mut().enumerate() {
            let mut w = [0u8; 8];
            w.copy_from_slice(&state[8 * i..8 * i + 8]);
            *lane = u64::from_le_bytes(w);
        }
        keccak::f1600(lanes);
        for (i, lane) in lanes.iter().enumerate() {
            state[8 * i..8 * i + 8].copy_from_slice(&lane.to_le_bytes());
        }
    }
}

// ---------------------------------------------------------------------------
// AES over the Java `K` array
// ---------------------------------------------------------------------------

/// Round keys in APPLICATION order, taken verbatim from `AESCrypt.K`:
/// `[initial][middle 0..m][final]`, each four big-endian words.
///
/// Built by [`AesRoundKeys::for_encrypt`] / [`AesRoundKeys::for_decrypt`]
/// from exactly the indices the Java method reads.
#[derive(Clone)]
pub struct AesRoundKeys {
    /// The words, in the order the Java body XORs them in.
    words: Vec<u32>,
    /// The same keys as bytes, for AES-NI.
    bytes: Vec<[u8; 16]>,
    decrypt: bool,
}

impl AesRoundKeys {
    /// Keys for `implEncryptBlock`: `K[0..4]`, then one middle round per
    /// `while (keyOffset < limit)` iteration (`keyOffset` starts at 4), then
    /// the last round at `K[keyOffset..keyOffset+4]`.
    ///
    /// `Err(index)` is the first `K` index the Java body would read out of
    /// range (its `ArrayIndexOutOfBoundsException`).
    pub fn for_encrypt(k: &[i32], limit: i32) -> Result<Self, usize> {
        let middle: i64 = if limit > 4 {
            (i64::from(limit) - 4 + 3) / 4
        } else {
            0
        };
        let needed = 8 + 4 * middle;
        if (k.len() as i64) < needed {
            return Err(k.len());
        }
        let needed = needed as usize;
        let words: Vec<u32> = k[..needed].iter().map(|w| *w as u32).collect();
        Ok(Self::from_words(words, false))
    }

    /// Keys for `implDecryptBlock`: `K[4..8]`, then `9 + 2*ROUNDS_12 +
    /// 2*(ROUNDS_12 && ROUNDS_14)` middle rounds from `K[8..]`, then the last
    /// round at `K[0..4]` — exactly the Java body's reads.
    pub fn for_decrypt(k: &[i32], rounds_12: bool, rounds_14: bool) -> Result<Self, usize> {
        let middle = 9 + if rounds_12 { 2 } else { 0 } + if rounds_12 && rounds_14 { 2 } else { 0 };
        let needed = 8 + 4 * middle;
        if k.len() < needed {
            // Java reads K[4..] first, then K[0..4].
            return Err(k.len().max(4));
        }
        let mut words: Vec<u32> = Vec::with_capacity(needed);
        words.extend(k[4..needed].iter().map(|w| *w as u32));
        words.extend(k[0..4].iter().map(|w| *w as u32));
        Ok(Self::from_words(words, true))
    }

    fn from_words(words: Vec<u32>, decrypt: bool) -> Self {
        let bytes = words
            .chunks_exact(4)
            .map(|c| {
                let mut b = [0u8; 16];
                for (i, w) in c.iter().enumerate() {
                    b[4 * i..4 * i + 4].copy_from_slice(&w.to_be_bytes());
                }
                b
            })
            .collect();
        AesRoundKeys {
            words,
            bytes,
            decrypt,
        }
    }

    /// Whether these keys decrypt (built by [`Self::for_decrypt`]).
    pub fn is_decrypt(&self) -> bool {
        self.decrypt
    }

    /// Apply the cipher (encrypt or decrypt, as built) to every 16-byte
    /// block of `data` in place. Blocks are independent (ECB).
    pub fn apply_blocks(&self, data: &mut [u8]) {
        #[cfg(target_arch = "x86_64")]
        {
            if aesni::available() {
                // SAFETY: `available()` checked the `aes` and `sse2` features
                // this function is compiled for; every load/store in it is an
                // unaligned 16-byte access inside `data` / `self.bytes`.
                unsafe { aesni::apply(&self.bytes, self.decrypt, data) };
                return;
            }
        }
        for block in data.chunks_exact_mut(16) {
            let mut b = [0u8; 16];
            b.copy_from_slice(block);
            let o = if self.decrypt {
                portable::decrypt_block(&self.words, &b)
            } else {
                portable::encrypt_block(&self.words, &b)
            };
            block.copy_from_slice(&o);
        }
    }

    /// One block.
    pub fn apply_block(&self, block: &mut [u8; 16]) {
        self.apply_blocks(&mut block[..]);
    }

    /// The portable (T-table) computation of one block — the reference the
    /// AES-NI path is tested against.
    pub fn apply_block_portable(&self, block: &[u8; 16]) -> [u8; 16] {
        if self.decrypt {
            portable::decrypt_block(&self.words, block)
        } else {
            portable::encrypt_block(&self.words, block)
        }
    }
}

/// Big-endian increment with wrap: `CounterMode.increment`.
pub fn ctr_increment(counter: &mut [u8]) {
    for b in counter.iter_mut().rev() {
        *b = b.wrapping_add(1);
        if *b != 0 {
            break;
        }
    }
}

/// The key stream `CounterMode.implCrypt` consumes for `len` bytes, advancing
/// `counter` / `enc_counter` / `used` exactly as its byte loop does
/// (`blockSize` 16). `used` must be `>= 0`.
pub fn ctr_keystream(
    keys: &AesRoundKeys,
    counter: &mut [u8; 16],
    enc_counter: &mut [u8; 16],
    used: &mut i32,
    len: usize,
) -> Vec<u8> {
    let mut ks = Vec::with_capacity(len);
    let mut rem = len;
    if *used < 16 && rem > 0 {
        let start = *used as usize;
        let n = (16 - start).min(rem);
        ks.extend_from_slice(&enc_counter[start..start + n]);
        *used += n as i32;
        rem -= n;
    }
    let full = rem / 16;
    if full > 0 {
        let base = ks.len();
        ks.resize(base + full * 16, 0);
        for i in 0..full {
            ks[base + 16 * i..base + 16 * i + 16].copy_from_slice(counter);
            ctr_increment(counter);
        }
        keys.apply_blocks(&mut ks[base..]);
        enc_counter.copy_from_slice(&ks[ks.len() - 16..]);
        *used = 16;
        rem -= full * 16;
    }
    if rem > 0 {
        let mut b = *counter;
        keys.apply_block(&mut b);
        ctr_increment(counter);
        *enc_counter = b;
        ks.extend_from_slice(&b[..rem]);
        *used = rem as i32;
    }
    ks
}

/// `true` when writing `[out, out+len)` in order never overwrites a byte of
/// `[inp, inp+len)` before the loop has read it — i.e. a bulk
/// read-then-write is indistinguishable from the Java loop.
pub fn bulk_equals_sequential(inp: usize, out: usize, len: usize) -> bool {
    out <= inp || out >= inp + len
}

/// `CipherBlockChaining.implEncrypt` over one buffer: `n` blocks read at
/// `inp`, written at `out`, with the chaining value `r` and the scratch `k`
/// the Java body leaves behind.
pub fn cbc_encrypt(
    keys: &AesRoundKeys,
    r: &mut [u8; 16],
    k: &mut [u8; 16],
    buf: &mut [u8],
    inp: usize,
    out: usize,
    n: usize,
) {
    for j in 0..n {
        for i in 0..16 {
            k[i] = buf[inp + 16 * j + i] ^ r[i];
        }
        let mut c = *k;
        keys.apply_block(&mut c);
        buf[out + 16 * j..out + 16 * j + 16].copy_from_slice(&c);
        r.copy_from_slice(&buf[out + 16 * j..out + 16 * j + 16]);
    }
}

/// `CipherBlockChaining.implDecrypt` over one buffer (see [`cbc_encrypt`]).
/// Disjoint regions are decrypted in bulk; an overlapping in-buffer call runs
/// the Java order block by block (it reads the chaining value back from the
/// cipher array AFTER writing the plain text, which is what the bytecode does).
pub fn cbc_decrypt(
    keys: &AesRoundKeys,
    r: &mut [u8; 16],
    k: &mut [u8; 16],
    buf: &mut [u8],
    inp: usize,
    out: usize,
    n: usize,
) {
    if n == 0 {
        return;
    }
    let len = 16 * n;
    if out >= inp + len || inp >= out + len {
        let mut d = buf[inp..inp + len].to_vec();
        keys.apply_blocks(&mut d);
        for j in 0..n {
            let prev: [u8; 16] = if j == 0 {
                *r
            } else {
                let mut p = [0u8; 16];
                p.copy_from_slice(&buf[inp + 16 * (j - 1)..inp + 16 * j]);
                p
            };
            for i in 0..16 {
                buf[out + 16 * j + i] = d[16 * j + i] ^ prev[i];
            }
        }
        k.copy_from_slice(&d[len - 16..]);
        r.copy_from_slice(&buf[inp + len - 16..inp + len]);
        return;
    }
    for j in 0..n {
        let mut c = [0u8; 16];
        c.copy_from_slice(&buf[inp + 16 * j..inp + 16 * j + 16]);
        keys.apply_block(&mut c);
        *k = c;
        for i in 0..16 {
            buf[out + 16 * j + i] = k[i] ^ r[i];
        }
        r.copy_from_slice(&buf[inp + 16 * j..inp + 16 * j + 16]);
    }
}

/// `ElectronicCodeBook.implECBEncrypt` / `implECBDecrypt` over one buffer.
pub fn ecb(keys: &AesRoundKeys, buf: &mut [u8], inp: usize, out: usize, n: usize) {
    let len = 16 * n;
    if n == 0 {
        return;
    }
    // Each Java block reads its 16 input bytes before writing its 16 output
    // bytes, so a bulk pass equals the loop unless an earlier block's output
    // lands on a later block's input (`inp < out < inp + len`).
    if bulk_equals_sequential(inp, out, len) {
        keys.apply_blocks(&mut buf[inp..inp + len]);
        if out != inp {
            buf.copy_within(inp..inp + len, out);
        }
        return;
    }
    for j in 0..n {
        let mut b = [0u8; 16];
        b.copy_from_slice(&buf[inp + 16 * j..inp + 16 * j + 16]);
        keys.apply_block(&mut b);
        buf[out + 16 * j..out + 16 * j + 16].copy_from_slice(&b);
    }
}

#[cfg(target_arch = "x86_64")]
mod aesni {
    use core::arch::x86_64::*;

    pub fn available() -> bool {
        std::is_x86_feature_detected!("aes") && std::is_x86_feature_detected!("sse2")
    }

    /// ECB over `data` with the given round keys (`keys.len() >= 2`).
    #[target_feature(enable = "aes,sse2")]
    pub unsafe fn apply(keys: &[[u8; 16]], decrypt: bool, data: &mut [u8]) {
        let nk = keys.len();
        debug_assert!(nk >= 2);
        let mut rk: Vec<__m128i> = Vec::with_capacity(nk);
        for k in keys {
            rk.push(_mm_loadu_si128(k.as_ptr() as *const __m128i));
        }
        let first = rk[0];
        let last = rk[nk - 1];
        let middle = &rk[1..nk - 1];
        let mut chunks = data.chunks_exact_mut(128);
        for chunk in &mut chunks {
            let p = chunk.as_mut_ptr() as *mut __m128i;
            let mut s = [_mm_setzero_si128(); 8];
            for (i, v) in s.iter_mut().enumerate() {
                *v = _mm_xor_si128(_mm_loadu_si128(p.add(i)), first);
            }
            if decrypt {
                for k in middle {
                    for v in s.iter_mut() {
                        *v = _mm_aesdec_si128(*v, *k);
                    }
                }
                for v in s.iter_mut() {
                    *v = _mm_aesdeclast_si128(*v, last);
                }
            } else {
                for k in middle {
                    for v in s.iter_mut() {
                        *v = _mm_aesenc_si128(*v, *k);
                    }
                }
                for v in s.iter_mut() {
                    *v = _mm_aesenclast_si128(*v, last);
                }
            }
            for (i, v) in s.iter().enumerate() {
                _mm_storeu_si128(p.add(i), *v);
            }
        }
        for block in chunks.into_remainder().chunks_exact_mut(16) {
            let p = block.as_mut_ptr() as *mut __m128i;
            let mut v = _mm_xor_si128(_mm_loadu_si128(p), first);
            if decrypt {
                for k in middle {
                    v = _mm_aesdec_si128(v, *k);
                }
                v = _mm_aesdeclast_si128(v, last);
            } else {
                for k in middle {
                    v = _mm_aesenc_si128(v, *k);
                }
                v = _mm_aesenclast_si128(v, last);
            }
            _mm_storeu_si128(p, v);
        }
    }
}

/// A line-by-line port of `AESCrypt`'s T-table bodies, parameterised by the
/// round-key words in application order. The fallback where AES-NI is
/// absent, and the reference the AES-NI path is tested against.
mod portable {
    const fn xtime(x: u8) -> u8 {
        (x << 1) ^ if x & 0x80 != 0 { 0x1b } else { 0 }
    }

    const fn gmul(mut a: u8, mut b: u8) -> u8 {
        let mut p = 0u8;
        while b != 0 {
            if b & 1 != 0 {
                p ^= a;
            }
            a = xtime(a);
            b >>= 1;
        }
        p
    }

    pub(super) const S: [u8; 256] = [
        0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab,
        0x76, 0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4,
        0x72, 0xc0, 0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71,
        0xd8, 0x31, 0x15, 0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2,
        0xeb, 0x27, 0xb2, 0x75, 0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6,
        0xb3, 0x29, 0xe3, 0x2f, 0x84, 0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb,
        0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf, 0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45,
        0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8, 0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5,
        0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2, 0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44,
        0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73, 0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a,
        0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb, 0xe0, 0x32, 0x3a, 0x0a, 0x49,
        0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79, 0xe7, 0xc8, 0x37, 0x6d,
        0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08, 0xba, 0x78, 0x25,
        0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a, 0x70, 0x3e,
        0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e, 0xe1,
        0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
        0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb,
        0x16,
    ];

    const fn inverse_sbox() -> [u8; 256] {
        let mut si = [0u8; 256];
        let mut i = 0;
        while i < 256 {
            si[S[i] as usize] = i as u8;
            i += 1;
        }
        si
    }

    pub(super) const SI: [u8; 256] = inverse_sbox();

    /// `T1` (encrypt) = `[2s, s, s, 3s]`, `T5` (decrypt) = `[e·si, 9·si,
    /// d·si, b·si]`; `T2..T4` / `T6..T8` are byte rotations of those.
    const fn tables() -> ([u32; 256], [u32; 256]) {
        let mut te = [0u32; 256];
        let mut td = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let s = S[i];
            te[i] = ((xtime(s) as u32) << 24)
                | ((s as u32) << 16)
                | ((s as u32) << 8)
                | ((xtime(s) ^ s) as u32);
            let si = SI[i];
            td[i] = ((gmul(si, 0x0e) as u32) << 24)
                | ((gmul(si, 0x09) as u32) << 16)
                | ((gmul(si, 0x0d) as u32) << 8)
                | (gmul(si, 0x0b) as u32);
            i += 1;
        }
        (te, td)
    }

    const TABLES: ([u32; 256], [u32; 256]) = tables();
    const TE: [u32; 256] = TABLES.0;
    const TD: [u32; 256] = TABLES.1;

    #[inline]
    fn t1(x: u32) -> u32 {
        TE[(x >> 24) as usize]
    }
    #[inline]
    fn t2(x: u32) -> u32 {
        TE[((x >> 16) & 0xff) as usize].rotate_right(8)
    }
    #[inline]
    fn t3(x: u32) -> u32 {
        TE[((x >> 8) & 0xff) as usize].rotate_right(16)
    }
    #[inline]
    fn t4(x: u32) -> u32 {
        TE[(x & 0xff) as usize].rotate_right(24)
    }
    #[inline]
    fn t5(x: u32) -> u32 {
        TD[(x >> 24) as usize]
    }
    #[inline]
    fn t6(x: u32) -> u32 {
        TD[((x >> 16) & 0xff) as usize].rotate_right(8)
    }
    #[inline]
    fn t7(x: u32) -> u32 {
        TD[((x >> 8) & 0xff) as usize].rotate_right(16)
    }
    #[inline]
    fn t8(x: u32) -> u32 {
        TD[(x & 0xff) as usize].rotate_right(24)
    }

    fn load(b: &[u8; 16], i: usize) -> u32 {
        u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
    }

    /// `implEncryptBlock` with `w` = `K[0 .. 8 + 4m]`.
    pub(super) fn encrypt_block(w: &[u32], input: &[u8; 16]) -> [u8; 16] {
        let mut ko = 0;
        let mut t0 = load(input, 0) ^ w[ko];
        let mut t1v = load(input, 1) ^ w[ko + 1];
        let mut t2v = load(input, 2) ^ w[ko + 2];
        let mut t3v = load(input, 3) ^ w[ko + 3];
        ko += 4;
        while ko + 4 < w.len() {
            let a0 = t1(t0) ^ t2(t1v) ^ t3(t2v) ^ t4(t3v) ^ w[ko];
            let a1 = t1(t1v) ^ t2(t2v) ^ t3(t3v) ^ t4(t0) ^ w[ko + 1];
            let a2 = t1(t2v) ^ t2(t3v) ^ t3(t0) ^ t4(t1v) ^ w[ko + 2];
            t3v = t1(t3v) ^ t2(t0) ^ t3(t1v) ^ t4(t2v) ^ w[ko + 3];
            t0 = a0;
            t1v = a1;
            t2v = a2;
            ko += 4;
        }
        let s = |x: u32| S[x as usize] as u32;
        let mut out = [0u8; 16];
        let cols = [
            [t0, t1v, t2v, t3v],
            [t1v, t2v, t3v, t0],
            [t2v, t3v, t0, t1v],
            [t3v, t0, t1v, t2v],
        ];
        for (c, [a, b, cc, d]) in cols.iter().enumerate() {
            let tt = w[ko + c];
            out[4 * c] = (s(a >> 24) ^ (tt >> 24)) as u8;
            out[4 * c + 1] = (s((b >> 16) & 0xff) ^ (tt >> 16)) as u8;
            out[4 * c + 2] = (s((cc >> 8) & 0xff) ^ (tt >> 8)) as u8;
            out[4 * c + 3] = (s(d & 0xff) ^ tt) as u8;
        }
        out
    }

    /// `implDecryptBlock` with `w` = `K[4 .. 8 + 4m] ++ K[0..4]`.
    pub(super) fn decrypt_block(w: &[u32], input: &[u8; 16]) -> [u8; 16] {
        let mut ko = 0;
        let mut t0 = load(input, 0) ^ w[ko];
        let mut t1v = load(input, 1) ^ w[ko + 1];
        let mut t2v = load(input, 2) ^ w[ko + 2];
        let mut t3v = load(input, 3) ^ w[ko + 3];
        ko += 4;
        while ko + 4 < w.len() {
            let a0 = t5(t0) ^ t6(t3v) ^ t7(t2v) ^ t8(t1v) ^ w[ko];
            let a1 = t5(t1v) ^ t6(t0) ^ t7(t3v) ^ t8(t2v) ^ w[ko + 1];
            let a2 = t5(t2v) ^ t6(t1v) ^ t7(t0) ^ t8(t3v) ^ w[ko + 2];
            t3v = t5(t3v) ^ t6(t2v) ^ t7(t1v) ^ t8(t0) ^ w[ko + 3];
            t0 = a0;
            t1v = a1;
            t2v = a2;
            ko += 4;
        }
        let s = |x: u32| SI[x as usize] as u32;
        let mut out = [0u8; 16];
        let cols = [
            [t0, t3v, t2v, t1v],
            [t1v, t0, t3v, t2v],
            [t2v, t1v, t0, t3v],
            [t3v, t2v, t1v, t0],
        ];
        for (c, [a, b, cc, d]) in cols.iter().enumerate() {
            let tt = w[ko + c];
            out[4 * c] = (s(a >> 24) ^ (tt >> 24)) as u8;
            out[4 * c + 1] = (s((b >> 16) & 0xff) ^ (tt >> 16)) as u8;
            out[4 * c + 2] = (s((cc >> 8) & 0xff) ^ (tt >> 8)) as u8;
            out[4 * c + 3] = (s(d & 0xff) ^ tt) as u8;
        }
        out
    }

    /// `AESCrypt.makeSessionKey`, ported for tests: `(Ke, Kd, rounds)`.
    #[cfg(test)]
    pub(super) fn make_session_key(key: &[u8]) -> (Vec<i32>, Vec<i32>, usize) {
        const RCON: [u8; 30] = [
            0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36, 0x6c, 0xd8, 0xab, 0x4d,
            0x9a, 0x2f, 0x5e, 0xbc, 0x63, 0xc6, 0x97, 0x35, 0x6a, 0xd4, 0xb3, 0x7d, 0xfa, 0xef,
            0xc5, 0x91,
        ];
        let bc = 4usize;
        let rounds = (key.len() >> 2) + 6;
        let rkc = (rounds + 1) * bc;
        let kc = key.len() / 4;
        let mut ke = vec![0u32; rkc];
        let mut kd = vec![0u32; rkc];
        let mut tk: Vec<u32> = (0..kc)
            .map(|i| {
                u32::from_be_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]])
            })
            .collect();
        let mut t = 0;
        let mut j = 0;
        while j < kc && t < rkc {
            ke[t] = tk[j];
            kd[(rounds - t / bc) * bc + t % bc] = tk[j];
            j += 1;
            t += 1;
        }
        let sb = |x: u32| S[(x & 0xff) as usize] as u32;
        let mut rcp = 0;
        while t < rkc {
            let tt = tk[kc - 1];
            tk[0] ^= (sb(tt >> 16) << 24)
                ^ (sb(tt >> 8) << 16)
                ^ (sb(tt) << 8)
                ^ sb(tt >> 24)
                ^ ((RCON[rcp] as u32) << 24);
            rcp += 1;
            if kc != 8 {
                for i in 1..kc {
                    tk[i] ^= tk[i - 1];
                }
            } else {
                for i in 1..kc / 2 {
                    tk[i] ^= tk[i - 1];
                }
                let tt = tk[kc / 2 - 1];
                tk[kc / 2] ^=
                    sb(tt) ^ (sb(tt >> 8) << 8) ^ (sb(tt >> 16) << 16) ^ (sb(tt >> 24) << 24);
                for i in kc / 2 + 1..kc {
                    tk[i] ^= tk[i - 1];
                }
            }
            j = 0;
            while j < kc && t < rkc {
                ke[t] = tk[j];
                kd[(rounds - t / bc) * bc + t % bc] = tk[j];
                j += 1;
                t += 1;
            }
        }
        // InvMixColumn on rounds 1..rounds-1 of Kd.
        for r in 1..rounds {
            for jj in 0..bc {
                let idx = r * bc + jj;
                let b = kd[idx].to_be_bytes();
                let m = |a: u8, x: u8, y: u8, z: u8| {
                    gmul(a, 0x0e) ^ gmul(x, 0x0b) ^ gmul(y, 0x0d) ^ gmul(z, 0x09)
                };
                kd[idx] = u32::from_be_bytes([
                    m(b[0], b[1], b[2], b[3]),
                    m(b[1], b[2], b[3], b[0]),
                    m(b[2], b[3], b[0], b[1]),
                    m(b[3], b[0], b[1], b[2]),
                ]);
            }
        }
        let n = kd.len();
        let tail = [kd[n - 4], kd[n - 3], kd[n - 2], kd[n - 1]];
        kd.copy_within(0..n - 4, 4);
        kd[..4].copy_from_slice(&tail);
        (
            ke.into_iter().map(|w| w as i32).collect(),
            kd.into_iter().map(|w| w as i32).collect(),
            rounds,
        )
    }
}

// ---------------------------------------------------------------------------
// GHASH
// ---------------------------------------------------------------------------

/// `GHASH.processBlocks`: fold every 16-byte block of `data` into the
/// GCM state `st` (two big-endian longs, as `GHASH.state` holds them) with
/// the subkey `h` (`subkeyHtbl[0..2]`).
pub fn ghash_blocks(st: &mut [u64; 2], h: [u64; 2], data: &[u8]) {
    use ghash::universal_hash::{KeyInit, UniversalHash};
    let n = data.len() / 16;
    if n == 0 {
        return;
    }
    let mut hk = [0u8; 16];
    hk[..8].copy_from_slice(&h[0].to_be_bytes());
    hk[8..].copy_from_slice(&h[1].to_be_bytes());
    let mut g = ghash::GHash::new(&hk.into());
    let mut first = [0u8; 16];
    first.copy_from_slice(&data[..16]);
    let s0 = st[0].to_be_bytes();
    let s1 = st[1].to_be_bytes();
    for i in 0..8 {
        first[i] ^= s0[i];
        first[8 + i] ^= s1[i];
    }
    g.update(&[first.into()]);
    if n > 1 {
        g.update_padded(&data[16..16 * n]);
    }
    let tag = g.finalize();
    let mut a = [0u8; 8];
    a.copy_from_slice(&tag[..8]);
    st[0] = u64::from_be_bytes(a);
    a.copy_from_slice(&tag[8..16]);
    st[1] = u64::from_be_bytes(a);
}

/// `GHASH.blockMult` + `processBlock`, ported line by line: the reference
/// [`ghash_blocks`] is tested against.
pub fn ghash_blocks_reference(st: &mut [u64; 2], h: [u64; 2], data: &[u8]) {
    for block in data.chunks_exact(16) {
        let mut a = [0u8; 8];
        a.copy_from_slice(&block[..8]);
        st[0] ^= u64::from_be_bytes(a);
        a.copy_from_slice(&block[8..]);
        st[1] ^= u64::from_be_bytes(a);
        let (mut z0, mut z1) = (0i64, 0i64);
        let (mut v0, mut v1) = (h[0] as i64, h[1] as i64);
        let mut x = st[0] as i64;
        for _ in 0..64 {
            let mask = x >> 63;
            z0 ^= v0 & mask;
            z1 ^= v1 & mask;
            let mask = (v1 << 63) >> 63;
            let carry = v0 & 1;
            v0 = ((v0 as u64) >> 1) as i64;
            v1 = (((v1 as u64) >> 1) as i64) | (carry << 63);
            v0 ^= (0xe100000000000000u64 as i64) & mask;
            x <<= 1;
        }
        x = st[1] as i64;
        for _ in 64..127 {
            let mask = x >> 63;
            z0 ^= v0 & mask;
            z1 ^= v1 & mask;
            let mask = (v1 << 63) >> 63;
            let carry = v0 & 1;
            v0 = ((v0 as u64) >> 1) as i64;
            v1 = (((v1 as u64) >> 1) as i64) | (carry << 63);
            v0 ^= (0xe100000000000000u64 as i64) & mask;
            x <<= 1;
        }
        let mask = x >> 63;
        z0 ^= v0 & mask;
        z1 ^= v1 & mask;
        st[0] = z0 as u64;
        st[1] = z1 as u64;
    }
}

// ---------------------------------------------------------------------------
// Poly1305 (IntegerPolynomial1305 limbs)
// ---------------------------------------------------------------------------

const P1305_BITS: u32 = 26;
const P1305_MASK: i64 = 0x3FF_FFFF;
/// `IntegerPolynomial.maxAddsAdd` for 26-bit limbs (`32 - bitsPerLimb`).
const P1305_MAX_ADDS_ADD: i32 = 32 - 26;
/// `IntegerPolynomial1305`'s `maxAddsMul`.
const P1305_MAX_ADDS_MUL: i32 = 1;

fn p1305_carry(l: &mut [i64; 5]) {
    for i in 0..4 {
        let c = l[i] >> P1305_BITS;
        l[i] = l[i].wrapping_sub(c << P1305_BITS);
        l[i + 1] = l[i + 1].wrapping_add(c);
    }
}

/// `IntegerPolynomial1305.reduce`.
fn p1305_reduce(l: &mut [i64; 5]) {
    let carry3 = l[3] >> P1305_BITS;
    l[3] = l[3].wrapping_sub(carry3 << P1305_BITS);
    let new4 = carry3.wrapping_add(l[4]);
    let carry4 = new4 >> P1305_BITS;
    l[4] = new4.wrapping_sub(carry4 << P1305_BITS);
    l[0] = l[0].wrapping_add(carry4.wrapping_mul(5));
    p1305_carry(l);
}

/// `IntegerPolynomial1305.mult` + `carryReduce`, into `a`.
fn p1305_mult(a: &mut [i64; 5], b: &[i64; 5]) {
    let m = |x: i64, y: i64| x.wrapping_mul(y);
    let c0 = m(a[0], b[0]);
    let c1 = m(a[0], b[1]).wrapping_add(m(a[1], b[0]));
    let c2 = m(a[0], b[2])
        .wrapping_add(m(a[1], b[1]))
        .wrapping_add(m(a[2], b[0]));
    let c3 = m(a[0], b[3])
        .wrapping_add(m(a[1], b[2]))
        .wrapping_add(m(a[2], b[1]))
        .wrapping_add(m(a[3], b[0]));
    let c4 = m(a[0], b[4])
        .wrapping_add(m(a[1], b[3]))
        .wrapping_add(m(a[2], b[2]))
        .wrapping_add(m(a[3], b[1]))
        .wrapping_add(m(a[4], b[0]));
    let c5 = m(a[1], b[4])
        .wrapping_add(m(a[2], b[3]))
        .wrapping_add(m(a[3], b[2]))
        .wrapping_add(m(a[4], b[1]));
    let c6 = m(a[2], b[4])
        .wrapping_add(m(a[3], b[3]))
        .wrapping_add(m(a[4], b[2]));
    let c7 = m(a[3], b[4]).wrapping_add(m(a[4], b[3]));
    let c8 = m(a[4], b[4]);
    // carryReduce
    let mut r = [0i64; 5];
    r[2] = c2.wrapping_add(c7.wrapping_mul(5));
    let c3 = c3.wrapping_add(c8.wrapping_mul(5));
    let carry3 = c3 >> P1305_BITS;
    r[3] = c3.wrapping_sub(carry3 << P1305_BITS);
    let c4 = c4.wrapping_add(carry3);
    let carry4 = c4 >> P1305_BITS;
    r[4] = c4.wrapping_sub(carry4 << P1305_BITS);
    let c5 = c5.wrapping_add(carry4);
    r[0] = c0.wrapping_add(c5.wrapping_mul(5));
    r[1] = c1.wrapping_add(c6.wrapping_mul(5));
    p1305_carry(&mut r);
    *a = r;
}

/// The Poly1305 accumulator and key as `processMultipleBlocks` sees them.
pub struct Poly1305Limbs {
    /// `a.limbs` (the accumulator).
    pub a: [i64; 5],
    /// `a.numAdds`.
    pub a_num_adds: i32,
    /// `r.limbs`.
    pub r: [i64; 5],
    /// `r.numAdds`.
    pub r_num_adds: i32,
    /// Set when the Java code would have reduced `r` in place.
    pub r_changed: bool,
}

/// `Poly1305.processMultipleBlocks` for every whole 16-byte block of
/// `data`: per block `n.setValue(block, 16, 0x01)`, `a.setSum(n)`,
/// `a.setProduct(r)`, with `IntegerPolynomial`'s reduce-if-needed rules.
pub fn poly1305_blocks(p: &mut Poly1305Limbs, data: &[u8]) {
    for block in data.chunks_exact(16) {
        let mut w = [0u8; 8];
        w.copy_from_slice(&block[..8]);
        let low = i64::from_le_bytes(w);
        w.copy_from_slice(&block[8..]);
        let high = i64::from_le_bytes(w);
        let lu = low as u64;
        let hu = high as u64;
        let n = [
            low & P1305_MASK,
            ((lu >> 26) as i64) & P1305_MASK,
            ((lu >> 52) as i64).wrapping_add((high & 0x3FFF) << 12),
            ((hu >> 14) as i64) & P1305_MASK,
            ((hu >> 40) as i64).wrapping_add(1i64 << 24),
        ];
        // setSum(n): n.numAdds == 0 after setValue.
        if p.a_num_adds > P1305_MAX_ADDS_ADD {
            p1305_reduce(&mut p.a);
            p.a_num_adds = 0;
        }
        for i in 0..5 {
            p.a[i] = p.a[i].wrapping_add(n[i]);
        }
        p.a_num_adds = p.a_num_adds.max(0) + 1;
        // setProduct(r)
        if p.a_num_adds > P1305_MAX_ADDS_MUL {
            p1305_reduce(&mut p.a);
            p.a_num_adds = 0;
        }
        if p.r_num_adds > P1305_MAX_ADDS_MUL {
            p1305_reduce(&mut p.r);
            p.r_num_adds = 0;
            p.r_changed = true;
        }
        let r = p.r;
        p1305_mult(&mut p.a, &r);
        p.a_num_adds = 0;
    }
}

// ---------------------------------------------------------------------------
// ChaCha20
// ---------------------------------------------------------------------------

const CHACHA_CONST: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

#[inline(always)]
fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// `ChaCha20Cipher.implChaCha20Block` for `out.len() / 64` consecutive
/// blocks: block `j` uses counter word `init[12] + j` (32-bit wrap). Words
/// 0..3 are the ChaCha constants regardless of `init[0..3]`, as in the JDK.
pub fn chacha20_blocks(init: &[u32; 16], out: &mut [u8]) {
    for (j, block) in out.chunks_exact_mut(64).enumerate() {
        let mut st = *init;
        st[..4].copy_from_slice(&CHACHA_CONST);
        st[12] = init[12].wrapping_add(j as u32);
        let start = st;
        let mut w = st;
        for _ in 0..10 {
            qr(&mut w, 0, 4, 8, 12);
            qr(&mut w, 1, 5, 9, 13);
            qr(&mut w, 2, 6, 10, 14);
            qr(&mut w, 3, 7, 11, 15);
            qr(&mut w, 0, 5, 10, 15);
            qr(&mut w, 1, 6, 11, 12);
            qr(&mut w, 2, 7, 8, 13);
            qr(&mut w, 3, 4, 9, 14);
        }
        for i in 0..16 {
            block[4 * i..4 * i + 4].copy_from_slice(&w[i].wrapping_add(start[i]).to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 ^ (self.0 >> 29)
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    fn pad_md(msg: &[u8], block: usize, len_bytes: usize, big_endian: bool) -> Vec<u8> {
        let mut m = msg.to_vec();
        m.push(0x80);
        while m.len() % block != block - len_bytes {
            m.push(0);
        }
        let bits = (msg.len() as u128) * 8;
        let lb = if big_endian {
            bits.to_be_bytes()
        } else {
            bits.to_le_bytes()
        };
        if big_endian {
            m.extend_from_slice(&lb[16 - len_bytes..]);
        } else {
            m.extend_from_slice(&lb[..len_bytes]);
        }
        m
    }

    #[test]
    fn sha256_matches_the_crate_digest() {
        use sha2::Digest;
        let mut rng = Lcg(1);
        for len in [0usize, 3, 55, 56, 64, 119, 1000] {
            let msg = rng.bytes(len);
            let mut st = [
                0x6a09e667u32,
                0xbb67ae85,
                0x3c6ef372,
                0xa54ff53a,
                0x510e527f,
                0x9b05688c,
                0x1f83d9ab,
                0x5be0cd19,
            ];
            sha256_blocks(&mut st, &pad_md(&msg, 64, 8, true));
            let got: Vec<u8> = st.iter().flat_map(|w| w.to_be_bytes()).collect();
            assert_eq!(got, sha2::Sha256::digest(&msg).to_vec(), "len {len}");
        }
    }

    #[test]
    fn sha512_and_sha1_match_the_crate_digests() {
        use sha2::Digest;
        let mut rng = Lcg(2);
        for len in [0usize, 111, 112, 200, 1000] {
            let msg = rng.bytes(len);
            let mut st = [
                0x6a09e667f3bcc908u64,
                0xbb67ae8584caa73b,
                0x3c6ef372fe94f82b,
                0xa54ff53a5f1d36f1,
                0x510e527fade682d1,
                0x9b05688c2b3e6c1f,
                0x1f83d9abfb41bd6b,
                0x5be0cd19137e2179,
            ];
            sha512_blocks(&mut st, &pad_md(&msg, 128, 16, true));
            let got: Vec<u8> = st.iter().flat_map(|w| w.to_be_bytes()).collect();
            assert_eq!(got, sha2::Sha512::digest(&msg).to_vec());
            let mut s1 = [
                0x67452301u32,
                0xefcdab89,
                0x98badcfe,
                0x10325476,
                0xc3d2e1f0,
            ];
            sha1_blocks(&mut s1, &pad_md(&msg, 64, 8, true));
            let got: Vec<u8> = s1.iter().flat_map(|w| w.to_be_bytes()).collect();
            assert_eq!(got, sha1::Sha1::digest(&msg).to_vec());
        }
    }

    #[test]
    fn md5_matches_rfc1321() {
        let cases: [(&[u8], &str); 3] = [
            (b"", "d41d8cd98f00b204e9800998ecf8427e"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (msg, hex) in cases {
            let mut st = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
            md5_blocks(&mut st, &pad_md(msg, 64, 8, false));
            let got: String = st
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(got, hex);
        }
    }

    #[test]
    fn sha3_256_matches_the_crate_digest() {
        use sha3::Digest;
        let mut rng = Lcg(3);
        for len in [0usize, 135, 136, 500] {
            let msg = rng.bytes(len);
            // SHA3-256: rate 136, suffix 0x06.
            let mut m = msg.clone();
            m.push(0x06);
            while m.len() % 136 != 0 {
                m.push(0);
            }
            let last = m.len() - 1;
            m[last] |= 0x80;
            let mut st = [0u64; 25];
            sha3_blocks(&mut st, &m, 136);
            let got: Vec<u8> = st[..4].iter().flat_map(|w| w.to_le_bytes()).collect();
            assert_eq!(got, sha3::Sha3_256::digest(&msg).to_vec());
        }
    }

    fn aes_crate_encrypt(key: &[u8], b: &[u8; 16]) -> [u8; 16] {
        use aes::cipher::{BlockEncrypt, KeyInit};
        let mut blk = aes::Block::clone_from_slice(b);
        match key.len() {
            16 => aes::Aes128::new_from_slice(key)
                .unwrap()
                .encrypt_block(&mut blk),
            24 => aes::Aes192::new_from_slice(key)
                .unwrap()
                .encrypt_block(&mut blk),
            _ => aes::Aes256::new_from_slice(key)
                .unwrap()
                .encrypt_block(&mut blk),
        }
        let mut o = [0u8; 16];
        o.copy_from_slice(&blk);
        o
    }

    #[test]
    fn aes_java_schedule_matches_fips197_both_paths() {
        let mut rng = Lcg(4);
        for klen in [16usize, 24, 32] {
            for _ in 0..20 {
                let key = rng.bytes(klen);
                let (ke, kd, rounds) = portable::make_session_key(&key);
                let enc = AesRoundKeys::for_encrypt(&ke, (rounds * 4) as i32).unwrap();
                let dec = AesRoundKeys::for_decrypt(&kd, rounds >= 12, rounds == 14).unwrap();
                let mut pt = [0u8; 16];
                pt.copy_from_slice(&rng.bytes(16));
                let want = aes_crate_encrypt(&key, &pt);
                assert_eq!(enc.apply_block_portable(&pt), want);
                let mut b = pt;
                enc.apply_block(&mut b);
                assert_eq!(b, want);
                assert_eq!(dec.apply_block_portable(&want), pt);
                let mut c = want;
                dec.apply_block(&mut c);
                assert_eq!(c, pt);
                // bulk path (8-wide) equals block path
                let mut many = rng.bytes(16 * 19);
                let mut one = many.clone();
                enc.apply_blocks(&mut many);
                for blk in one.chunks_exact_mut(16) {
                    let mut x = [0u8; 16];
                    x.copy_from_slice(blk);
                    blk.copy_from_slice(&enc.apply_block_portable(&x));
                }
                assert_eq!(many, one);
            }
        }
    }

    #[test]
    fn aes_arbitrary_k_arrays_agree_across_paths() {
        let mut rng = Lcg(5);
        for limit in [4i32, 8, 40, 48, 56, 41] {
            let k: Vec<i32> = (0..80).map(|_| rng.next() as i32).collect();
            let enc = AesRoundKeys::for_encrypt(&k, limit).unwrap();
            for flags in [(false, false), (true, false), (true, true), (false, true)] {
                let dec = AesRoundKeys::for_decrypt(&k, flags.0, flags.1).unwrap();
                let mut x = [0u8; 16];
                x.copy_from_slice(&rng.bytes(16));
                let mut a = x;
                dec.apply_block(&mut a);
                assert_eq!(a, dec.apply_block_portable(&x));
            }
            let mut x = [0u8; 16];
            x.copy_from_slice(&rng.bytes(16));
            let mut a = x;
            enc.apply_block(&mut a);
            assert_eq!(a, enc.apply_block_portable(&x));
        }
        assert_eq!(AesRoundKeys::for_encrypt(&[0; 43], 40).err(), Some(43));
        assert_eq!(
            AesRoundKeys::for_decrypt(&[0; 3], false, false).err(),
            Some(4)
        );
    }

    #[test]
    fn ctr_keystream_matches_the_byte_loop() {
        let mut rng = Lcg(6);
        let key = rng.bytes(16);
        let (ke, _, _) = portable::make_session_key(&key);
        let enc = AesRoundKeys::for_encrypt(&ke, 40).unwrap();
        for len in [0usize, 1, 15, 16, 17, 100, 257] {
            for used0 in [0i32, 5, 16] {
                let mut c1 = [0xffu8; 16];
                c1[0] = 7;
                let mut e1 = [0u8; 16];
                e1.copy_from_slice(&rng.bytes(16));
                let (mut c2, mut e2, mut u1, mut u2) = (c1, e1, used0, used0);
                let fast = ctr_keystream(&enc, &mut c1, &mut e1, &mut u1, len);
                let mut slow = Vec::new();
                for _ in 0..len {
                    if u2 >= 16 {
                        e2 = enc.apply_block_portable(&c2);
                        ctr_increment(&mut c2);
                        u2 = 0;
                    }
                    slow.push(e2[u2 as usize]);
                    u2 += 1;
                }
                assert_eq!(fast, slow);
                assert_eq!((c1, e1, u1), (c2, e2, u2), "len {len} used {used0}");
            }
        }
    }

    #[test]
    fn ghash_matches_the_java_bit_loop() {
        let mut rng = Lcg(7);
        for n in [0usize, 1, 2, 5, 33] {
            let h = [rng.next(), rng.next()];
            let s = [rng.next(), rng.next()];
            let data = rng.bytes(16 * n);
            let mut a = s;
            let mut b = s;
            ghash_blocks(&mut a, h, &data);
            ghash_blocks_reference(&mut b, h, &data);
            assert_eq!(a, b, "n {n}");
        }
    }

    #[test]
    fn chacha20_block_matches_rfc8439_2_3_2() {
        let key: Vec<u8> = (0u8..32).collect();
        let nonce = [0u8, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let mut init = [0u32; 16];
        for i in 0..8 {
            init[4 + i] =
                u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
        }
        init[12] = 1;
        for i in 0..3 {
            init[13 + i] = u32::from_le_bytes([
                nonce[4 * i],
                nonce[4 * i + 1],
                nonce[4 * i + 2],
                nonce[4 * i + 3],
            ]);
        }
        let mut out = [0u8; 128];
        chacha20_blocks(&init, &mut out);
        let want_head = [
            0x10u8, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f,
        ];
        assert_eq!(&out[..12], &want_head);
        let mut second = [0u8; 64];
        let mut init2 = init;
        init2[12] = 2;
        chacha20_blocks(&init2, &mut second);
        assert_eq!(&out[64..], &second[..]);
    }

    /// Value of (non-negative, carried) 26-bit limbs mod 2^130-5.
    fn limbs_value(l: &[i64; 5]) -> rsa::BigUint {
        use rsa::BigUint;
        let p = (BigUint::from(1u8) << 130usize) - BigUint::from(5u8);
        let mut v = BigUint::from(0u8);
        for (i, x) in l.iter().enumerate() {
            assert!(*x >= 0, "limb {i} negative: {x}");
            v += BigUint::from(*x as u64) << (26 * i);
        }
        v % p
    }

    #[test]
    fn poly1305_limbs_compute_horner_mod_p() {
        use rsa::BigUint;
        let mut rng = Lcg(8);
        let p = (BigUint::from(1u8) << 130usize) - BigUint::from(5u8);
        // r clamped, 26-bit limbs as IntegerPolynomial1305.getElement leaves them.
        let mut rb = rng.bytes(16);
        for i in [3usize, 7, 11, 15] {
            rb[i] &= 15;
        }
        for i in [4usize, 8, 12] {
            rb[i] &= 252;
        }
        let r_int = BigUint::from_bytes_le(&rb);
        let mut r16 = [0u8; 16];
        r16.copy_from_slice(&rb);
        let rv = u128::from_le_bytes(r16);
        let mut r = [0i64; 5];
        for (i, x) in r.iter_mut().enumerate() {
            *x = ((rv >> (26 * i)) & 0x3ff_ffff) as i64;
        }
        let data = rng.bytes(16 * 9);
        let mut st = Poly1305Limbs {
            a: [0; 5],
            a_num_adds: 0,
            r,
            r_num_adds: 0,
            r_changed: false,
        };
        poly1305_blocks(&mut st, &data);
        let mut acc = BigUint::from(0u8);
        for blk in data.chunks_exact(16) {
            let mut b = blk.to_vec();
            b.push(1);
            acc = ((acc + BigUint::from_bytes_le(&b)) * &r_int) % &p;
        }
        assert_eq!(limbs_value(&st.a), acc);
        assert_eq!(st.a_num_adds, 0);
        assert!(!st.r_changed);
    }
}
