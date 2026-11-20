// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Block-compression kernels for BouncyCastle's own pure-Java digests.
//!
//! # Why these exist
//!
//! BouncyCastle's hash-based post-quantum code (LMS/HSS in
//! `org.bouncycastle.pqc.crypto.lms`) does not hash through
//! `java.security.MessageDigest`. `LMS DigestUtil.createDigest` constructs
//! `org.bouncycastle.crypto.digests.SHA256Digest` directly, so CratonVM's
//! native JCA SHA-256 — which is real and fast — is on a path this workload
//! never takes. HotSpot has no intrinsic for BouncyCastle's class either; its
//! speed there is plain C2-compiled Java.
//!
//! That leaves the compression function running as ~2 500 bytecodes per
//! 64-byte block on both VMs, and measurement (see the PQC throughput
//! investigation) put CratonVM at ~37x HotSpot on exactly that kernel with the
//! JIT fully engaged and `hot_but_stuck_in_interpreter=0` — nothing was failing
//! to compile, the compiled code was simply far slower than C2's. Replacing the
//! single `processBlock` leaf removes the whole round schedule from the
//! bytecode path in one step.
//!
//! # What is deliberately NOT here
//!
//! Only the *compression function* is native. Buffering, padding, length
//! encoding, `reset`, `copy`/`Memoable` state and the digest output all remain
//! real BouncyCastle bytecode, so anything that observes intermediate digest
//! state (and `SavableDigest.getEncodedState` does) sees exactly what it would
//! have seen. The kernel below is a pure function of `(state, block)` with no
//! VM or heap dependency — the marshalling lives at the `native-builtins`
//! registration site.

/// SHA-256 round constants (FIPS 180-4 §4.2.2), identical to BouncyCastle's
/// `SHA256Digest.K`.
const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// BouncyCastle `SHA256Digest.processBlock()`, in full.
///
/// `state` is `H1..H8`; `x` is the digest's own `X[64]` scratch array, whose
/// first sixteen words are the big-endian-decoded message block that
/// `GeneralDigest.processWord` already deposited there.
///
/// On return this leaves **exactly** the state BouncyCastle's bytecode would:
/// `state` advanced by one compression, `x[16..64]` holding the expanded
/// message schedule, and `x[0..16]` zeroed. The schedule tail is written back
/// rather than dropped because `copy()` / `reset(Memoable)` copy all 64 words
/// of `X`, and reproducing observable state exactly is cheaper than an argument
/// about which words a caller can reach.
///
/// Pure: no allocation, no heap access, no failure mode. `wrapping_add` is the
/// Java `int` arithmetic the source performs; every other operator is exact.
pub fn sha256_process_block(state: &mut [u32; 8], x: &mut [u32; 64]) {
    let _scrub = crate::scrub::ScrubOnDrop;
    // Message schedule — BouncyCastle's `for (int t = 16; t <= 63; t++)` loop,
    // expanding in place into the same array.
    for t in 16..64 {
        let t2 = x[t - 2];
        let t15 = x[t - 15];
        // Theta1(v) = ror17 ^ ror19 ^ (v >>> 10); Theta0(v) = ror7 ^ ror18 ^ (v >>> 3).
        let theta1 = t2.rotate_right(17) ^ t2.rotate_right(19) ^ (t2 >> 10);
        let theta0 = t15.rotate_right(7) ^ t15.rotate_right(18) ^ (t15 >> 3);
        x[t] = theta1
            .wrapping_add(x[t - 7])
            .wrapping_add(theta0)
            .wrapping_add(x[t - 16]);
    }

    let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
    let (mut e, mut f, mut g, mut h) = (state[4], state[5], state[6], state[7]);

    for t in 0..64 {
        // Sum1(e) = ror6 ^ ror11 ^ ror25; Ch(e,f,g) = (e&f) ^ (~e & g).
        let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(sum1)
            .wrapping_add(ch)
            .wrapping_add(K256[t])
            .wrapping_add(x[t]);
        // Sum0(a) = ror2 ^ ror13 ^ ror22; Maj(a,b,c) = (a&b) ^ (a&c) ^ (b&c).
        let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = sum0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);

    // BouncyCastle's trailing `for (int i = 0; i < 16; i++) X[i] = 0;`.
    for slot in x[..16].iter_mut() {
        *slot = 0;
    }
}

/// Convenience wrapper for callers that hold only the sixteen message words
/// and do not own an `X[64]`. Used by this module's own vectors.

/// BouncyCastle `MD5Digest.processBlock()`, in full.
///
/// `state` is `H1..H4`; `x` is the digest's own `X[16]` scratch array, whose
/// sixteen words are the little-endian-decoded message block that
/// `MD5Digest.processWord` already deposited there.
///
/// On return `state` is advanced by one RFC 1321 compression and `x[0..16]` is
/// zeroed, matching BouncyCastle's trailing loop.
pub fn md5_process_block(state: &mut [u32; 4], x: &mut [u32; 16]) {
    let _scrub = crate::scrub::ScrubOnDrop;
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
            .wrapping_add(crate::jdk_intrinsics::MD5_K[i])
            .wrapping_add(x[g])
            .rotate_left(crate::jdk_intrinsics::MD5_S[i]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(t);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    x.fill(0);
}


/// Hardware-accelerated (SHA-NI when available) SHA-256 block compression over
/// sixteen big-endian-decoded 32-bit message words.
pub fn sha256_compress(state: &mut [u32; 8], w: &[u32; 16]) {
    let _scrub = crate::scrub::ScrubOnDrop;
    let mut block = [0u8; 64];
    for (dst, &word) in block.chunks_exact_mut(4).zip(w.iter()) {
        dst.copy_from_slice(&word.to_be_bytes());
    }
    sha2::compress256(
        state,
        core::slice::from_ref(sha2::digest::generic_array::GenericArray::from_slice(&block)),
    );
}

/// SHA-512/384 block compression over sixteen 64-bit message words.
pub fn sha512_compress(state: &mut [u64; 8], w: &[u64; 16]) {
    let _scrub = crate::scrub::ScrubOnDrop;
    let mut block = [0u8; 128];
    for (dst, &word) in block.chunks_exact_mut(8).zip(w.iter()) {
        dst.copy_from_slice(&word.to_be_bytes());
    }
    sha2::compress512(
        state,
        core::slice::from_ref(sha2::digest::generic_array::GenericArray::from_slice(&block)),
    );
}

#[cfg(test)]
mod tests;
