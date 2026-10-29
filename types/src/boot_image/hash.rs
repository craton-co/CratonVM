//! A small, dependency-free 64-bit hash for corruption detection and for the
//! key's file name. Not cryptographic: it guards against torn writes, bit rot
//! and a stale file, not against an adversary who can write the cache
//! directory (who could equally replace the binary).

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;

#[inline]
fn round(acc: u64, w: u64) -> u64 {
    (acc ^ w.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

/// Hash `data`, continuing from `seed` (so a hash can be chained over several
/// slices: `hash64(hash64(s, a), b)`). Four independent lanes over 32-byte
/// blocks keep it at memory speed for the optional payload verification.
pub fn hash64(seed: u64, data: &[u8]) -> u64 {
    let mut lanes = [
        seed ^ P1,
        seed.wrapping_add(P2),
        seed.rotate_left(17) ^ P3,
        seed.wrapping_sub(P1),
    ];
    let mut blocks = data.chunks_exact(32);
    for b in &mut blocks {
        for (i, lane) in lanes.iter_mut().enumerate() {
            let w = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
            *lane = round(*lane, w);
        }
    }
    let mut h = lanes[0].rotate_left(1)
        ^ lanes[1].rotate_left(7)
        ^ lanes[2].rotate_left(12)
        ^ lanes[3].rotate_left(18);
    h = h.wrapping_add((data.len() as u64).wrapping_mul(P3));
    let rest = blocks.remainder();
    let mut words = rest.chunks_exact(8);
    for w in &mut words {
        h = round(h, u64::from_le_bytes(w.try_into().unwrap()));
    }
    let tail = words.remainder();
    if !tail.is_empty() {
        let mut buf = [0u8; 8];
        buf[..tail.len()].copy_from_slice(tail);
        h = round(h ^ tail.len() as u64, u64::from_le_bytes(buf));
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^= h >> 32;
    h
}
