// SPDX-License-Identifier: Apache-2.0
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
pub fn sha256_compress(state: &mut [u32; 8], w: &[u32; 16]) {
    let mut x = [0u32; 64];
    x[..16].copy_from_slice(w);
    sha256_process_block(state, &mut x);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 initial chaining value (FIPS 180-4 §5.3.3) — BouncyCastle's
    /// `SHA256Digest.reset()`.
    const IV: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    /// Pad `msg` per FIPS 180-4 and compress every block — the same framing
    /// `GeneralDigest.finish` + `processLength` produce.
    fn digest(msg: &[u8]) -> [u8; 32] {
        let mut padded = msg.to_vec();
        let bitlen = (msg.len() as u64) * 8;
        padded.push(0x80);
        while padded.len() % 64 != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&bitlen.to_be_bytes());

        let mut state = IV;
        for block in padded.chunks_exact(64) {
            let mut w = [0u32; 16];
            for (i, word) in block.chunks_exact(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            sha256_compress(&mut state, &w);
        }
        let mut out = [0u8; 32];
        for (i, s) in state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&s.to_be_bytes());
        }
        out
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// FIPS 180-4 published vector. A compression function that is subtly wrong
    /// (a mis-transcribed rotate, a `>>` where the spec says `>>>`) still
    /// produces stable, self-consistent output, so a round trip against itself
    /// proves nothing — only a published vector does.
    #[test]
    fn sha256_matches_the_published_abc_vector() {
        assert_eq!(
            hex(&digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_matches_the_published_empty_vector() {
        assert_eq!(
            hex(&digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// Two-block message: exercises the message schedule carrying across a
    /// block boundary, which the one-block vectors above cannot reach.
    #[test]
    fn sha256_matches_the_published_two_block_vector() {
        assert_eq!(
            hex(&digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// A long message (1 000 000 'a's is the classic NIST case, shortened here
    /// to keep the test fast while still spanning many blocks).
    #[test]
    fn sha256_matches_a_multi_block_vector() {
        let msg = vec![b'a'; 1000];
        assert_eq!(
            hex(&digest(&msg)),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }
}
