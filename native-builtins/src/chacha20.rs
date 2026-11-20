// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! ChaCha20 (RFC 8439 §2.3-2.4), Poly1305 (§2.5) and the ChaCha20-Poly1305
//! AEAD construction (§2.8).
//!
//! # Why this exists
//!
//! `javax.crypto.Cipher` advertised `ChaCha20` and `ChaCha20-Poly1305` and had
//! no implementation of either. `cipher_do_final_impl` discarded the cipher
//! name, `parse_transformation` defaulted a mode-less transformation to `ECB`,
//! and a 32-byte ChaCha20 key is a **valid AES-256 key** — so the AES key
//! schedule was built without error and the ECB arm ran. The nonce was
//! discarded, output was deterministic per (key, block), and for
//! `ChaCha20-Poly1305` there was no AEAD tag at all: decrypting attacker-
//! modified ciphertext returned "plaintext" with no authentication failure and
//! no exception anywhere on the path.
//!
//! Nothing here is a fast implementation. It is a correct one, written to be
//! read against the RFC, with the RFC's own test vectors beside it.
//!
//! # Constant-time properties, stated
//!
//! * The tag comparison in [`chacha20_poly1305_decrypt`] is constant time over
//!   the 16 tag bytes — a variable-time compare is a tag-forgery oracle.
//! * Poly1305's arithmetic is branch-free over the message contents (the limb
//!   carries are unconditional adds and shifts, not conditionals), so the MAC
//!   does not leak the accumulator through timing.
//! * ChaCha20 itself is naturally constant time: adds, XORs and rotations only,
//!   with no data-dependent indexing.

/// The ChaCha20 quarter-round, RFC 8439 §2.1.
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

/// The initial ChaCha20 state, RFC 8439 §2.3: four constant words, eight key
/// words, one counter word, three nonce words — all little-endian.
fn chacha20_state(key: &[u8; 32], nonce: &[u8; 12], counter: u32) -> [u32; 16] {
    let mut s = [0u32; 16];
    // "expand 32-byte k"
    s[0] = 0x6170_7865;
    s[1] = 0x3320_646e;
    s[2] = 0x7962_2d32;
    s[3] = 0x6b20_6574;
    for i in 0..8 {
        s[4 + i] = u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    s[12] = counter;
    for i in 0..3 {
        s[13 + i] = u32::from_le_bytes([
            nonce[4 * i],
            nonce[4 * i + 1],
            nonce[4 * i + 2],
            nonce[4 * i + 3],
        ]);
    }
    s
}

/// One 64-byte ChaCha20 keystream block, RFC 8439 §2.3.1: twenty rounds
/// (ten column/diagonal double-rounds) added word-wise to the initial state.
pub fn chacha20_block(key: &[u8; 32], nonce: &[u8; 12], counter: u32) -> [u8; 64] {
    let initial = chacha20_state(key, nonce, counter);
    let mut x = initial;
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
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&x[i].wrapping_add(initial[i]).to_le_bytes());
    }
    out
}

/// XOR `data` with the ChaCha20 keystream starting at `counter`, RFC 8439 §2.4.
///
/// Encryption and decryption are the same operation, which is why this returns
/// one function rather than a pair: a caller that "decrypts" is XORing the same
/// keystream back off.
///
/// The counter WRAPS rather than saturating, matching the 32-bit counter word
/// the state carries. A caller feeding more than 256 GiB under one nonce has
/// already lost the security argument; wrapping is the RFC's own behaviour and
/// silently producing a different keystream would be worse than reproducing it.
pub fn chacha20_apply(key: &[u8; 32], nonce: &[u8; 12], counter: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut block_counter = counter;
    for chunk in data.chunks(64) {
        let ks = chacha20_block(key, nonce, block_counter);
        for (i, b) in chunk.iter().enumerate() {
            out.push(b ^ ks[i]);
        }
        block_counter = block_counter.wrapping_add(1);
    }
    out
}

/// Poly1305 one-time authenticator, RFC 8439 §2.5.
///
/// The accumulator is 130 bits held as five 26-bit limbs, which is the
/// reference "donna" representation: every partial product fits a `u64`, so
/// the whole MAC needs no bignum and no allocation.
struct Poly1305 {
    /// The clamped `r`, in 26-bit limbs.
    r: [u32; 5],
    /// `r[1..5] * 5`, precomputed — the reduction of a limb that overflows
    /// past 2^130 is a multiply by 5, because 2^130 ≡ 5 (mod 2^130 - 5).
    r5: [u32; 4],
    /// The accumulator, in 26-bit limbs.
    h: [u32; 5],
    /// `s`, the second half of the key, added once at the end.
    pad: [u32; 4],
}

const POLY1305_LIMB_MASK: u32 = 0x03ff_ffff;

impl Poly1305 {
    /// `key` is the 32-byte one-time key: `r` then `s`.
    fn new(key: &[u8; 32]) -> Self {
        let t0 = u32::from_le_bytes([key[0], key[1], key[2], key[3]]);
        let t1 = u32::from_le_bytes([key[4], key[5], key[6], key[7]]);
        let t2 = u32::from_le_bytes([key[8], key[9], key[10], key[11]]);
        let t3 = u32::from_le_bytes([key[12], key[13], key[14], key[15]]);
        // The clamp of RFC 8439 §2.5: clear the top four bits of bytes 3, 7,
        // 11 and 15 and the bottom two bits of bytes 4, 8 and 12. Expressed
        // here on the 26-bit limbs, which folds both into one mask each.
        let r = [
            t0 & 0x03ff_ffff,
            ((t0 >> 26) | (t1 << 6)) & 0x03ff_ff03,
            ((t1 >> 20) | (t2 << 12)) & 0x03ff_c0ff,
            ((t2 >> 14) | (t3 << 18)) & 0x03f0_3fff,
            (t3 >> 8) & 0x000f_ffff,
        ];
        Self {
            r,
            r5: [r[1] * 5, r[2] * 5, r[3] * 5, r[4] * 5],
            h: [0; 5],
            pad: [
                u32::from_le_bytes([key[16], key[17], key[18], key[19]]),
                u32::from_le_bytes([key[20], key[21], key[22], key[23]]),
                u32::from_le_bytes([key[24], key[25], key[26], key[27]]),
                u32::from_le_bytes([key[28], key[29], key[30], key[31]]),
            ],
        }
    }

    /// Absorb one block. `hibit` is `1 << 24` for a full 16-byte block (the
    /// implicit high bit RFC 8439 appends) and is folded into the padded copy
    /// for a short final block instead.
    fn block(&mut self, block: &[u8; 16], hibit: u32) {
        let t0 = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        let t1 = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
        let t2 = u32::from_le_bytes([block[8], block[9], block[10], block[11]]);
        let t3 = u32::from_le_bytes([block[12], block[13], block[14], block[15]]);

        self.h[0] += t0 & POLY1305_LIMB_MASK;
        self.h[1] += ((t0 >> 26) | (t1 << 6)) & POLY1305_LIMB_MASK;
        self.h[2] += ((t1 >> 20) | (t2 << 12)) & POLY1305_LIMB_MASK;
        self.h[3] += ((t2 >> 14) | (t3 << 18)) & POLY1305_LIMB_MASK;
        self.h[4] += (t3 >> 8) | hibit;

        // h *= r (mod 2^130 - 5). Each d[i] is a sum of five products of
        // 26-bit and 26-bit values, so it fits in a u64 with room to spare.
        let h: [u64; 5] = [
            self.h[0] as u64,
            self.h[1] as u64,
            self.h[2] as u64,
            self.h[3] as u64,
            self.h[4] as u64,
        ];
        let r: [u64; 5] = [
            self.r[0] as u64,
            self.r[1] as u64,
            self.r[2] as u64,
            self.r[3] as u64,
            self.r[4] as u64,
        ];
        let s: [u64; 4] = [
            self.r5[0] as u64,
            self.r5[1] as u64,
            self.r5[2] as u64,
            self.r5[3] as u64,
        ];
        let mut d = [0u64; 5];
        d[0] = h[0] * r[0] + h[1] * s[3] + h[2] * s[2] + h[3] * s[1] + h[4] * s[0];
        d[1] = h[0] * r[1] + h[1] * r[0] + h[2] * s[3] + h[3] * s[2] + h[4] * s[1];
        d[2] = h[0] * r[2] + h[1] * r[1] + h[2] * r[0] + h[3] * s[3] + h[4] * s[2];
        d[3] = h[0] * r[3] + h[1] * r[2] + h[2] * r[1] + h[3] * r[0] + h[4] * s[3];
        d[4] = h[0] * r[4] + h[1] * r[3] + h[2] * r[2] + h[3] * r[1] + h[4] * r[0];

        // Carry propagation, then fold the overflow past limb 4 back in at
        // limb 0 multiplied by 5.
        let mut c: u64;
        c = d[0] >> 26;
        self.h[0] = (d[0] as u32) & POLY1305_LIMB_MASK;
        let d1 = d[1] + c;
        c = d1 >> 26;
        self.h[1] = (d1 as u32) & POLY1305_LIMB_MASK;
        let d2 = d[2] + c;
        c = d2 >> 26;
        self.h[2] = (d2 as u32) & POLY1305_LIMB_MASK;
        let d3 = d[3] + c;
        c = d3 >> 26;
        self.h[3] = (d3 as u32) & POLY1305_LIMB_MASK;
        let d4 = d[4] + c;
        c = d4 >> 26;
        self.h[4] = (d4 as u32) & POLY1305_LIMB_MASK;
        self.h[0] += (c as u32) * 5;
        let c2 = self.h[0] >> 26;
        self.h[0] &= POLY1305_LIMB_MASK;
        self.h[1] += c2;
    }

    /// Absorb a whole message, padding each 16-byte group as RFC 8439 requires.
    fn update(&mut self, data: &[u8]) {
        let mut chunks = data.chunks_exact(16);
        for chunk in &mut chunks {
            let mut b = [0u8; 16];
            b.copy_from_slice(chunk);
            self.block(&b, 1 << 24);
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            // A short final block gets the 0x01 byte appended and the rest
            // zeroed, so its implicit high bit lands inside the block instead
            // of at bit 128.
            let mut b = [0u8; 16];
            b[..rem.len()].copy_from_slice(rem);
            b[rem.len()] = 1;
            self.block(&b, 0);
        }
    }

    /// Final reduction and `+ s`, RFC 8439 §2.5.
    fn finish(mut self) -> [u8; 16] {
        // Fully carry h.
        let mut c = self.h[1] >> 26;
        self.h[1] &= POLY1305_LIMB_MASK;
        self.h[2] += c;
        c = self.h[2] >> 26;
        self.h[2] &= POLY1305_LIMB_MASK;
        self.h[3] += c;
        c = self.h[3] >> 26;
        self.h[3] &= POLY1305_LIMB_MASK;
        self.h[4] += c;
        c = self.h[4] >> 26;
        self.h[4] &= POLY1305_LIMB_MASK;
        self.h[0] += c * 5;
        c = self.h[0] >> 26;
        self.h[0] &= POLY1305_LIMB_MASK;
        self.h[1] += c;

        // g = h + 5, i.e. h - (2^130 - 5). If that did not borrow, g is the
        // reduced value and h was >= 2^130 - 5.
        let mut g = [0u32; 5];
        let mut cg = 5u32;
        for i in 0..5 {
            let t = self.h[i] + cg;
            g[i] = t & POLY1305_LIMB_MASK;
            cg = t >> 26;
        }
        // `cg` is 1 exactly when the addition carried out of limb 4 — h is
        // fully carried above, so limb 4 is a true 26-bit value and h + 5
        // reaches 2^130 only when h >= 2^130 - 5. That is precisely when `g`
        // is the reduced representative.
        //
        // The select is branch-free on purpose: `cg` is derived from the
        // accumulator, which is secret, and a branch here would leak whether
        // the MAC landed in the top five values of the field.
        let select = 0u32.wrapping_sub(cg);
        for i in 0..5 {
            self.h[i] = (self.h[i] & !select) | (g[i] & select);
        }

        // Serialise the 130-bit accumulator into four 32-bit words, then add s.
        let h0 = self.h[0] | (self.h[1] << 26);
        let h1 = (self.h[1] >> 6) | (self.h[2] << 20);
        let h2 = (self.h[2] >> 12) | (self.h[3] << 14);
        let h3 = (self.h[3] >> 18) | (self.h[4] << 8);

        let mut f = h0 as u64 + self.pad[0] as u64;
        let o0 = f as u32;
        f = h1 as u64 + self.pad[1] as u64 + (f >> 32);
        let o1 = f as u32;
        f = h2 as u64 + self.pad[2] as u64 + (f >> 32);
        let o2 = f as u32;
        f = h3 as u64 + self.pad[3] as u64 + (f >> 32);
        let o3 = f as u32;

        let mut tag = [0u8; 16];
        tag[0..4].copy_from_slice(&o0.to_le_bytes());
        tag[4..8].copy_from_slice(&o1.to_le_bytes());
        tag[8..12].copy_from_slice(&o2.to_le_bytes());
        tag[12..16].copy_from_slice(&o3.to_le_bytes());
        tag
    }
}

/// Poly1305 over `data` with the 32-byte one-time `key`, RFC 8439 §2.5.
pub fn poly1305(key: &[u8; 32], data: &[u8]) -> [u8; 16] {
    let mut st = Poly1305::new(key);
    st.update(data);
    st.finish()
}

/// The AEAD's one-time Poly1305 key: the first 32 bytes of the ChaCha20
/// keystream at counter 0, RFC 8439 §2.6. The message itself starts at
/// counter 1, which is why this block can never collide with ciphertext.
fn poly1305_key_gen(key: &[u8; 32], nonce: &[u8; 12]) -> [u8; 32] {
    let block = chacha20_block(key, nonce, 0);
    let mut out = [0u8; 32];
    out.copy_from_slice(&block[..32]);
    out
}

/// The AEAD MAC input, RFC 8439 §2.8: AAD, zero-padded to 16; ciphertext,
/// zero-padded to 16; then the two lengths as little-endian u64s.
///
/// The padding and the explicit lengths are what stop an attacker moving bytes
/// between the AAD and the ciphertext without changing the tag. Omitting either
/// is a real forgery, not a formatting detail.
fn aead_mac_data(aad: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(aad.len() + ciphertext.len() + 32);
    m.extend_from_slice(aad);
    m.extend(std::iter::repeat(0u8).take((16 - (aad.len() % 16)) % 16));
    m.extend_from_slice(ciphertext);
    m.extend(std::iter::repeat(0u8).take((16 - (ciphertext.len() % 16)) % 16));
    m.extend_from_slice(&(aad.len() as u64).to_le_bytes());
    m.extend_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    m
}

/// ChaCha20-Poly1305 AEAD encryption, RFC 8439 §2.8.2.
///
/// Returns `(ciphertext, tag)`. The caller appends the tag — `javax.crypto`'s
/// `doFinal` contract is that the tag trails the ciphertext in one array.
pub fn chacha20_poly1305_encrypt(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> (Vec<u8>, [u8; 16]) {
    let otk = poly1305_key_gen(key, nonce);
    let ciphertext = chacha20_apply(key, nonce, 1, plaintext);
    let tag = poly1305(&otk, &aead_mac_data(aad, &ciphertext));
    (ciphertext, tag)
}

/// ChaCha20-Poly1305 AEAD decryption, RFC 8439 §2.8.
///
/// `Err(())` is a tag mismatch and the caller MUST surface it as an
/// authentication failure — never as a plaintext. Returning the decrypted bytes
/// alongside a "failed" flag would invite exactly the misuse this whole change
/// exists to remove, so a failure carries no plaintext at all.
pub fn chacha20_poly1305_decrypt(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
    tag: &[u8; 16],
) -> Result<Vec<u8>, ()> {
    let otk = poly1305_key_gen(key, nonce);
    let expected = poly1305(&otk, &aead_mac_data(aad, ciphertext));
    // Constant-time compare: a `==` on byte arrays may return early on the
    // first differing byte, which is a tag-forgery oracle.
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= expected[i] ^ tag[i];
    }
    if diff != 0 {
        return Err(());
    }
    Ok(chacha20_apply(key, nonce, 1, ciphertext))
}

#[cfg(test)]
mod tests;
