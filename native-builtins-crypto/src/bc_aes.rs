// SPDX-License-Identifier: MIT AND Apache-2.0
//
// Portions of this file are derived from the Bouncy Castle Cryptography Library
// (`org.bouncycastle.crypto.engines.AESEngine`): the AES S-box / inverse S-box,
// the T/Tinv tables, and the round structure are mechanically transcribed from
// BouncyCastle Java source. Those portions are
//   Copyright (c) 2000-2024 The Legion of the Bouncy Castle Inc.
// and are used under the Bouncy Castle Licence (an MIT-style permissive licence),
// NOT Apache-2.0. The Craton-authored glue/integration code is
//   Copyright 2024-2026 Craton Software Company (Apache-2.0).
// See ../THIRD-PARTY-NOTICES.md for the full Bouncy Castle Licence text.

//! Native fast-path for BouncyCastle's `org.bouncycastle.crypto.engines.AESEngine`
//! single-block transform. A faithful, table-for-table port of BC's
//! `encryptBlock` / `decryptBlock` (the T-table equivalent inverse cipher), so
//! it is bit-identical to the interpreted bytecode by construction — it consumes
//! BC's already-expanded `WorkingKey` (`int[][] KW`) verbatim and applies the
//! exact same `T0`/`Tinv0`/`S`/`Si` tables and round structure.
//!
//! Motivation: with `org/bouncycastle/*` JIT-banned, `AESEngine.encryptBlock`
//! runs interpreted and dominates the AES Monte-Carlo stress test
//! (`AESTest` → `BlockCipherMonteCarloTest`), the documented AES non-finish.
//!
//! Tables and round code are mechanically transcribed from `AESEngine.java`
//! (BouncyCastle, under the Bouncy Castle Licence — MIT-style; see
//! ../THIRD-PARTY-NOTICES.md). `shift(r, n)` in BC == `r.rotate_right(n)`
//! (it computes `(r >>> n) | (r << (32 - n))`). Validated by a FIPS-197 KAT
//! below and end-to-end by BC's own `AESTest` vectors.

use crate::failure::{CryptoFailure, CryptoResult};

pub(crate) const AES_S: [u8; 256] = [
    99, 124, 119, 123, 242, 107, 111, 197, 48, 1, 103, 43, 254, 215, 171, 118, 202, 130, 201, 125,
    250, 89, 71, 240, 173, 212, 162, 175, 156, 164, 114, 192, 183, 253, 147, 38, 54, 63, 247, 204,
    52, 165, 229, 241, 113, 216, 49, 21, 4, 199, 35, 195, 24, 150, 5, 154, 7, 18, 128, 226, 235,
    39, 178, 117, 9, 131, 44, 26, 27, 110, 90, 160, 82, 59, 214, 179, 41, 227, 47, 132, 83, 209, 0,
    237, 32, 252, 177, 91, 106, 203, 190, 57, 74, 76, 88, 207, 208, 239, 170, 251, 67, 77, 51, 133,
    69, 249, 2, 127, 80, 60, 159, 168, 81, 163, 64, 143, 146, 157, 56, 245, 188, 182, 218, 33, 16,
    255, 243, 210, 205, 12, 19, 236, 95, 151, 68, 23, 196, 167, 126, 61, 100, 93, 25, 115, 96, 129,
    79, 220, 34, 42, 144, 136, 70, 238, 184, 20, 222, 94, 11, 219, 224, 50, 58, 10, 73, 6, 36, 92,
    194, 211, 172, 98, 145, 149, 228, 121, 231, 200, 55, 109, 141, 213, 78, 169, 108, 86, 244, 234,
    101, 122, 174, 8, 186, 120, 37, 46, 28, 166, 180, 198, 232, 221, 116, 31, 75, 189, 139, 138,
    112, 62, 181, 102, 72, 3, 246, 14, 97, 53, 87, 185, 134, 193, 29, 158, 225, 248, 152, 17, 105,
    217, 142, 148, 155, 30, 135, 233, 206, 85, 40, 223, 140, 161, 137, 13, 191, 230, 66, 104, 65,
    153, 45, 15, 176, 84, 187, 22,
];
pub(crate) const AES_SI: [u8; 256] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130,
    155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61,
    238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109,
    139, 209, 37, 114, 248, 246, 100, 134, 104, 152, 22, 212, 164, 92, 204, 93, 101, 182, 146, 108,
    112, 72, 80, 253, 237, 185, 218, 94, 21, 70, 87, 167, 141, 157, 132, 144, 216, 171, 0, 140,
    188, 211, 10, 247, 228, 88, 5, 184, 179, 69, 6, 208, 44, 30, 143, 202, 63, 15, 2, 193, 175,
    189, 3, 1, 19, 138, 107, 58, 145, 17, 65, 79, 103, 220, 234, 151, 242, 207, 206, 240, 180, 230,
    115, 150, 172, 116, 34, 231, 173, 53, 133, 226, 249, 55, 232, 28, 117, 223, 110, 71, 241, 26,
    113, 29, 41, 197, 137, 111, 183, 98, 14, 170, 24, 190, 27, 252, 86, 62, 75, 198, 210, 121, 32,
    154, 219, 192, 254, 120, 205, 90, 244, 31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39,
    128, 236, 95, 96, 81, 127, 169, 25, 181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160,
    224, 59, 77, 174, 42, 245, 176, 200, 235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119,
    214, 38, 225, 105, 20, 99, 85, 33, 12, 125,
];
pub(crate) const AES_T0: [u32; 256] = [
    0xa56363c6, 0x847c7cf8, 0x997777ee, 0x8d7b7bf6, 0x0df2f2ff, 0xbd6b6bd6, 0xb16f6fde, 0x54c5c591,
    0x50303060, 0x03010102, 0xa96767ce, 0x7d2b2b56, 0x19fefee7, 0x62d7d7b5, 0xe6abab4d, 0x9a7676ec,
    0x45caca8f, 0x9d82821f, 0x40c9c989, 0x877d7dfa, 0x15fafaef, 0xeb5959b2, 0xc947478e, 0x0bf0f0fb,
    0xecadad41, 0x67d4d4b3, 0xfda2a25f, 0xeaafaf45, 0xbf9c9c23, 0xf7a4a453, 0x967272e4, 0x5bc0c09b,
    0xc2b7b775, 0x1cfdfde1, 0xae93933d, 0x6a26264c, 0x5a36366c, 0x413f3f7e, 0x02f7f7f5, 0x4fcccc83,
    0x5c343468, 0xf4a5a551, 0x34e5e5d1, 0x08f1f1f9, 0x937171e2, 0x73d8d8ab, 0x53313162, 0x3f15152a,
    0x0c040408, 0x52c7c795, 0x65232346, 0x5ec3c39d, 0x28181830, 0xa1969637, 0x0f05050a, 0xb59a9a2f,
    0x0907070e, 0x36121224, 0x9b80801b, 0x3de2e2df, 0x26ebebcd, 0x6927274e, 0xcdb2b27f, 0x9f7575ea,
    0x1b090912, 0x9e83831d, 0x742c2c58, 0x2e1a1a34, 0x2d1b1b36, 0xb26e6edc, 0xee5a5ab4, 0xfba0a05b,
    0xf65252a4, 0x4d3b3b76, 0x61d6d6b7, 0xceb3b37d, 0x7b292952, 0x3ee3e3dd, 0x712f2f5e, 0x97848413,
    0xf55353a6, 0x68d1d1b9, 0x00000000, 0x2cededc1, 0x60202040, 0x1ffcfce3, 0xc8b1b179, 0xed5b5bb6,
    0xbe6a6ad4, 0x46cbcb8d, 0xd9bebe67, 0x4b393972, 0xde4a4a94, 0xd44c4c98, 0xe85858b0, 0x4acfcf85,
    0x6bd0d0bb, 0x2aefefc5, 0xe5aaaa4f, 0x16fbfbed, 0xc5434386, 0xd74d4d9a, 0x55333366, 0x94858511,
    0xcf45458a, 0x10f9f9e9, 0x06020204, 0x817f7ffe, 0xf05050a0, 0x443c3c78, 0xba9f9f25, 0xe3a8a84b,
    0xf35151a2, 0xfea3a35d, 0xc0404080, 0x8a8f8f05, 0xad92923f, 0xbc9d9d21, 0x48383870, 0x04f5f5f1,
    0xdfbcbc63, 0xc1b6b677, 0x75dadaaf, 0x63212142, 0x30101020, 0x1affffe5, 0x0ef3f3fd, 0x6dd2d2bf,
    0x4ccdcd81, 0x140c0c18, 0x35131326, 0x2fececc3, 0xe15f5fbe, 0xa2979735, 0xcc444488, 0x3917172e,
    0x57c4c493, 0xf2a7a755, 0x827e7efc, 0x473d3d7a, 0xac6464c8, 0xe75d5dba, 0x2b191932, 0x957373e6,
    0xa06060c0, 0x98818119, 0xd14f4f9e, 0x7fdcdca3, 0x66222244, 0x7e2a2a54, 0xab90903b, 0x8388880b,
    0xca46468c, 0x29eeeec7, 0xd3b8b86b, 0x3c141428, 0x79dedea7, 0xe25e5ebc, 0x1d0b0b16, 0x76dbdbad,
    0x3be0e0db, 0x56323264, 0x4e3a3a74, 0x1e0a0a14, 0xdb494992, 0x0a06060c, 0x6c242448, 0xe45c5cb8,
    0x5dc2c29f, 0x6ed3d3bd, 0xefacac43, 0xa66262c4, 0xa8919139, 0xa4959531, 0x37e4e4d3, 0x8b7979f2,
    0x32e7e7d5, 0x43c8c88b, 0x5937376e, 0xb76d6dda, 0x8c8d8d01, 0x64d5d5b1, 0xd24e4e9c, 0xe0a9a949,
    0xb46c6cd8, 0xfa5656ac, 0x07f4f4f3, 0x25eaeacf, 0xaf6565ca, 0x8e7a7af4, 0xe9aeae47, 0x18080810,
    0xd5baba6f, 0x887878f0, 0x6f25254a, 0x722e2e5c, 0x241c1c38, 0xf1a6a657, 0xc7b4b473, 0x51c6c697,
    0x23e8e8cb, 0x7cdddda1, 0x9c7474e8, 0x211f1f3e, 0xdd4b4b96, 0xdcbdbd61, 0x868b8b0d, 0x858a8a0f,
    0x907070e0, 0x423e3e7c, 0xc4b5b571, 0xaa6666cc, 0xd8484890, 0x05030306, 0x01f6f6f7, 0x120e0e1c,
    0xa36161c2, 0x5f35356a, 0xf95757ae, 0xd0b9b969, 0x91868617, 0x58c1c199, 0x271d1d3a, 0xb99e9e27,
    0x38e1e1d9, 0x13f8f8eb, 0xb398982b, 0x33111122, 0xbb6969d2, 0x70d9d9a9, 0x898e8e07, 0xa7949433,
    0xb69b9b2d, 0x221e1e3c, 0x92878715, 0x20e9e9c9, 0x49cece87, 0xff5555aa, 0x78282850, 0x7adfdfa5,
    0x8f8c8c03, 0xf8a1a159, 0x80898909, 0x170d0d1a, 0xdabfbf65, 0x31e6e6d7, 0xc6424284, 0xb86868d0,
    0xc3414182, 0xb0999929, 0x772d2d5a, 0x110f0f1e, 0xcbb0b07b, 0xfc5454a8, 0xd6bbbb6d, 0x3a16162c,
];
pub(crate) const AES_TINV0: [u32; 256] = [
    0x50a7f451, 0x5365417e, 0xc3a4171a, 0x965e273a, 0xcb6bab3b, 0xf1459d1f, 0xab58faac, 0x9303e34b,
    0x55fa3020, 0xf66d76ad, 0x9176cc88, 0x254c02f5, 0xfcd7e54f, 0xd7cb2ac5, 0x80443526, 0x8fa362b5,
    0x495ab1de, 0x671bba25, 0x980eea45, 0xe1c0fe5d, 0x02752fc3, 0x12f04c81, 0xa397468d, 0xc6f9d36b,
    0xe75f8f03, 0x959c9215, 0xeb7a6dbf, 0xda595295, 0x2d83bed4, 0xd3217458, 0x2969e049, 0x44c8c98e,
    0x6a89c275, 0x78798ef4, 0x6b3e5899, 0xdd71b927, 0xb64fe1be, 0x17ad88f0, 0x66ac20c9, 0xb43ace7d,
    0x184adf63, 0x82311ae5, 0x60335197, 0x457f5362, 0xe07764b1, 0x84ae6bbb, 0x1ca081fe, 0x942b08f9,
    0x58684870, 0x19fd458f, 0x876cde94, 0xb7f87b52, 0x23d373ab, 0xe2024b72, 0x578f1fe3, 0x2aab5566,
    0x0728ebb2, 0x03c2b52f, 0x9a7bc586, 0xa50837d3, 0xf2872830, 0xb2a5bf23, 0xba6a0302, 0x5c8216ed,
    0x2b1ccf8a, 0x92b479a7, 0xf0f207f3, 0xa1e2694e, 0xcdf4da65, 0xd5be0506, 0x1f6234d1, 0x8afea6c4,
    0x9d532e34, 0xa055f3a2, 0x32e18a05, 0x75ebf6a4, 0x39ec830b, 0xaaef6040, 0x069f715e, 0x51106ebd,
    0xf98a213e, 0x3d06dd96, 0xae053edd, 0x46bde64d, 0xb58d5491, 0x055dc471, 0x6fd40604, 0xff155060,
    0x24fb9819, 0x97e9bdd6, 0xcc434089, 0x779ed967, 0xbd42e8b0, 0x888b8907, 0x385b19e7, 0xdbeec879,
    0x470a7ca1, 0xe90f427c, 0xc91e84f8, 0x00000000, 0x83868009, 0x48ed2b32, 0xac70111e, 0x4e725a6c,
    0xfbff0efd, 0x5638850f, 0x1ed5ae3d, 0x27392d36, 0x64d90f0a, 0x21a65c68, 0xd1545b9b, 0x3a2e3624,
    0xb1670a0c, 0x0fe75793, 0xd296eeb4, 0x9e919b1b, 0x4fc5c080, 0xa220dc61, 0x694b775a, 0x161a121c,
    0x0aba93e2, 0xe52aa0c0, 0x43e0223c, 0x1d171b12, 0x0b0d090e, 0xadc78bf2, 0xb9a8b62d, 0xc8a91e14,
    0x8519f157, 0x4c0775af, 0xbbdd99ee, 0xfd607fa3, 0x9f2601f7, 0xbcf5725c, 0xc53b6644, 0x347efb5b,
    0x7629438b, 0xdcc623cb, 0x68fcedb6, 0x63f1e4b8, 0xcadc31d7, 0x10856342, 0x40229713, 0x2011c684,
    0x7d244a85, 0xf83dbbd2, 0x1132f9ae, 0x6da129c7, 0x4b2f9e1d, 0xf330b2dc, 0xec52860d, 0xd0e3c177,
    0x6c16b32b, 0x99b970a9, 0xfa489411, 0x2264e947, 0xc48cfca8, 0x1a3ff0a0, 0xd82c7d56, 0xef903322,
    0xc74e4987, 0xc1d138d9, 0xfea2ca8c, 0x360bd498, 0xcf81f5a6, 0x28de7aa5, 0x268eb7da, 0xa4bfad3f,
    0xe49d3a2c, 0x0d927850, 0x9bcc5f6a, 0x62467e54, 0xc2138df6, 0xe8b8d890, 0x5ef7392e, 0xf5afc382,
    0xbe805d9f, 0x7c93d069, 0xa92dd56f, 0xb31225cf, 0x3b99acc8, 0xa77d1810, 0x6e639ce8, 0x7bbb3bdb,
    0x097826cd, 0xf418596e, 0x01b79aec, 0xa89a4f83, 0x656e95e6, 0x7ee6ffaa, 0x08cfbc21, 0xe6e815ef,
    0xd99be7ba, 0xce366f4a, 0xd4099fea, 0xd67cb029, 0xafb2a431, 0x31233f2a, 0x3094a5c6, 0xc066a235,
    0x37bc4e74, 0xa6ca82fc, 0xb0d090e0, 0x15d8a733, 0x4a9804f1, 0xf7daec41, 0x0e50cd7f, 0x2ff69117,
    0x8dd64d76, 0x4db0ef43, 0x544daacc, 0xdf0496e4, 0xe3b5d19e, 0x1b886a4c, 0xb81f2cc1, 0x7f516546,
    0x04ea5e9d, 0x5d358c01, 0x737487fa, 0x2e410bfb, 0x5a1d67b3, 0x52d2db92, 0x335610e9, 0x1347d66d,
    0x8c61d79a, 0x7a0ca137, 0x8e14f859, 0x893c13eb, 0xee27a9ce, 0x35c961b7, 0xede51ce1, 0x3cb1477a,
    0x59dfd29c, 0x3f73f255, 0x79ce1418, 0xbf37c773, 0xeacdf753, 0x5baafd5f, 0x146f3ddf, 0x86db4478,
    0x81f3afca, 0x3ec468b9, 0x2c342438, 0x5f40a3c2, 0x72c31d16, 0x0c25e2bc, 0x8b493c28, 0x41950dff,
    0x7101a839, 0xdeb30c08, 0x9ce4b4d8, 0x90c15664, 0x6184cb7b, 0x70b632d5, 0x745c6c48, 0x4257b8d0,
];

#[inline(always)]
fn le_to_u32(b: &[u8], off: usize) -> u32 {
    (b[off] as u32)
        | ((b[off + 1] as u32) << 8)
        | ((b[off + 2] as u32) << 16)
        | ((b[off + 3] as u32) << 24)
}

#[inline(always)]
fn u32_to_le(v: u32, b: &mut [u8], off: usize) {
    b[off] = v as u8;
    b[off + 1] = (v >> 8) as u8;
    b[off + 2] = (v >> 16) as u8;
    b[off + 3] = (v >> 24) as u8;
}

// BC `shift(r, n)` == rotate-right by n.
#[inline(always)]
fn sh(r: u32, n: u32) -> u32 {
    r.rotate_right(n)
}

#[inline(always)]
fn t0(i: u32) -> u32 {
    AES_T0[(i & 255) as usize]
}
#[inline(always)]
fn ti(i: u32) -> u32 {
    AES_TINV0[(i & 255) as usize]
}
#[inline(always)]
fn sbox(i: u32) -> u32 {
    AES_S[(i & 255) as usize] as u32
}
#[inline(always)]
fn sibox(i: u32) -> u32 {
    AES_SI[(i & 255) as usize] as u32
}

/// The three legal expanded-schedule lengths: `ROUNDS + 1` for AES-128/192/256
/// (10/12/14 rounds). [`generate_working_key`] produces exactly one of these.
const VALID_SCHEDULE_LENS: [usize; 3] = [11, 13, 15];

/// Preconditions shared by [`encrypt_block`] and [`decrypt_block`].
///
/// The schedule length is checked **exactly**, not merely for a lower bound.
/// `rounds = kw.len() - 1` underflows for an empty schedule, and the round
/// structure additionally reads `kw[rounds]` after the main loop, so lengths
/// like 2 or 4 index out of bounds even though they are "non-empty". A block
/// shorter than 16 bytes would panic in `le_to_u32`/`u32_to_le`.
///
/// The critical property is what this function does **not** offer: there is no
/// "carry on with a degraded schedule" branch. A block cipher that quietly
/// wrote a zero block, or encrypted under a truncated key, would hand the
/// caller ciphertext-shaped bytes that are not ciphertext — the single worst
/// outcome available here.
fn check_block_args(kw: &[[u32; 4]], inb: &[u8], outb: &[u8]) -> CryptoResult<()> {
    if !VALID_SCHEDULE_LENS.contains(&kw.len()) {
        return Err(CryptoFailure::not_initialised(format!(
            "AES engine not initialised: key schedule has {} round key(s), expected 11, 13 or 15",
            kw.len()
        )));
    }
    if inb.len() < 16 || outb.len() < 16 {
        return Err(CryptoFailure::illegal_argument(format!(
            "AES block must be 16 bytes (input {}, output {})",
            inb.len(),
            outb.len()
        )));
    }
    Ok(())
}

/// Fail-loud [`encrypt_block`]: validates the key schedule and block lengths
/// and returns the JDK exception to raise instead of panicking.
///
/// Every in-tree caller of the infallible form already pre-checks
/// `kw.len() >= 2` (`native-builtins/src/phases_late/bouncycastle.rs:6081`,
/// `:6199`, `:6565`); this is the same guard expressed as a value the facade
/// can turn into a catchable `IllegalStateException`, and made **exact** —
/// their lower bound still admits `kw.len() == 2`, which indexes out of bounds
/// on the final round.
pub fn try_encrypt_block(kw: &[[u32; 4]], inb: &[u8], outb: &mut [u8]) -> CryptoResult<()> {
    check_block_args(kw, inb, outb)?;
    encrypt_block(kw, inb, outb);
    Ok(())
}

/// Fail-loud [`decrypt_block`]. See [`try_encrypt_block`].
pub fn try_decrypt_block(kw: &[[u32; 4]], inb: &[u8], outb: &mut [u8]) -> CryptoResult<()> {
    check_block_args(kw, inb, outb)?;
    decrypt_block(kw, inb, outb);
    Ok(())
}

/// The guard applied by the **infallible** pair, which cannot return a failure.
///
/// A `-> ()` block cipher has no safe way to decline: writing zeros, or leaving
/// the output buffer at whatever it held, both hand the caller sixteen bytes it
/// will treat as ciphertext (or plaintext). Only an abort is a refusal.
///
/// This is not a new failure — a malformed schedule already aborted, via an
/// `kw.len() - 1` underflow or an out-of-bounds `kw[rounds]` index. What
/// changes is that the abort now happens **before** any table lookup, is
/// deterministic rather than dependent on which index happens to run off the
/// end first, and names the defect. All six in-tree call sites validate the
/// schedule first, but only with a `kw.len() >= 2` *lower bound*
/// (`native-builtins/src/phases_late/bouncycastle.rs:6081`, `:6199`, `:6565`,
/// `:7578`, `:8886`, `:9091`), which still admits a 2- or 4-entry schedule that
/// the round structure indexes past the end of.
#[inline]
fn demand_block_args(kw: &[[u32; 4]], inb: &[u8], outb: &[u8], kernel: &str) {
    if let Err(e) = check_block_args(kw, inb, outb) {
        panic!("{kernel}: {e}");
    }
}

/// Port of `AESEngine.encryptBlock`. `kw` is the expanded key schedule
/// (`KW[round][col]`); `inb`/`outb` are 16-byte blocks. `ROUNDS == kw.len()-1`.
///
/// # Panics
///
/// On a key schedule whose length is not 11/13/15, or a short block. This is a
/// *loud* failure — it can never be mistaken for ciphertext — but it is not a
/// catchable Java exception. Prefer [`try_encrypt_block`], which reports the
/// same conditions as a failure the facade can throw. The signature is kept
/// as-is for the existing `native-builtins` registrations, all of which
/// validate the schedule first (though only with a `>= 2` lower bound — see
/// [`check_block_args`]). The abort is now raised explicitly and up front by
/// [`demand_block_args`], rather than emerging from whichever index runs off
/// the end first.
pub fn encrypt_block(kw: &[[u32; 4]], inb: &[u8], outb: &mut [u8]) {
    demand_block_args(kw, inb, outb, "AESEngine.encryptBlock");
    let rounds = kw.len() - 1;
    let c0 = le_to_u32(inb, 0);
    let c1 = le_to_u32(inb, 4);
    let c2 = le_to_u32(inb, 8);
    let c3 = le_to_u32(inb, 12);

    let mut t0v = c0 ^ kw[0][0];
    let mut t1v = c1 ^ kw[0][1];
    let mut t2v = c2 ^ kw[0][2];

    let mut r = 1usize;
    let (mut r0, mut r1, mut r2);
    let mut r3 = c3 ^ kw[0][3];
    while r < rounds - 1 {
        r0 =
            t0(t0v) ^ sh(t0(t1v >> 8), 24) ^ sh(t0(t2v >> 16), 16) ^ sh(t0(r3 >> 24), 8) ^ kw[r][0];
        r1 =
            t0(t1v) ^ sh(t0(t2v >> 8), 24) ^ sh(t0(r3 >> 16), 16) ^ sh(t0(t0v >> 24), 8) ^ kw[r][1];
        r2 =
            t0(t2v) ^ sh(t0(r3 >> 8), 24) ^ sh(t0(t0v >> 16), 16) ^ sh(t0(t1v >> 24), 8) ^ kw[r][2];
        r3 =
            t0(r3) ^ sh(t0(t0v >> 8), 24) ^ sh(t0(t1v >> 16), 16) ^ sh(t0(t2v >> 24), 8) ^ kw[r][3];
        r += 1;
        t0v = t0(r0) ^ sh(t0(r1 >> 8), 24) ^ sh(t0(r2 >> 16), 16) ^ sh(t0(r3 >> 24), 8) ^ kw[r][0];
        t1v = t0(r1) ^ sh(t0(r2 >> 8), 24) ^ sh(t0(r3 >> 16), 16) ^ sh(t0(r0 >> 24), 8) ^ kw[r][1];
        t2v = t0(r2) ^ sh(t0(r3 >> 8), 24) ^ sh(t0(r0 >> 16), 16) ^ sh(t0(r1 >> 24), 8) ^ kw[r][2];
        r3 = t0(r3) ^ sh(t0(r0 >> 8), 24) ^ sh(t0(r1 >> 16), 16) ^ sh(t0(r2 >> 24), 8) ^ kw[r][3];
        r += 1;
    }

    r0 = t0(t0v) ^ sh(t0(t1v >> 8), 24) ^ sh(t0(t2v >> 16), 16) ^ sh(t0(r3 >> 24), 8) ^ kw[r][0];
    r1 = t0(t1v) ^ sh(t0(t2v >> 8), 24) ^ sh(t0(r3 >> 16), 16) ^ sh(t0(t0v >> 24), 8) ^ kw[r][1];
    r2 = t0(t2v) ^ sh(t0(r3 >> 8), 24) ^ sh(t0(t0v >> 16), 16) ^ sh(t0(t1v >> 24), 8) ^ kw[r][2];
    r3 = t0(r3) ^ sh(t0(t0v >> 8), 24) ^ sh(t0(t1v >> 16), 16) ^ sh(t0(t2v >> 24), 8) ^ kw[r][3];
    r += 1;

    // final round: simple S-box (instance `s` == forward S here)
    let o0 = sbox(r0)
        ^ (sbox(r1 >> 8) << 8)
        ^ (sbox(r2 >> 16) << 16)
        ^ (sbox(r3 >> 24) << 24)
        ^ kw[r][0];
    let o1 = sbox(r1)
        ^ (sbox(r2 >> 8) << 8)
        ^ (sbox(r3 >> 16) << 16)
        ^ (sbox(r0 >> 24) << 24)
        ^ kw[r][1];
    let o2 = sbox(r2)
        ^ (sbox(r3 >> 8) << 8)
        ^ (sbox(r0 >> 16) << 16)
        ^ (sbox(r1 >> 24) << 24)
        ^ kw[r][2];
    let o3 = sbox(r3)
        ^ (sbox(r0 >> 8) << 8)
        ^ (sbox(r1 >> 16) << 16)
        ^ (sbox(r2 >> 24) << 24)
        ^ kw[r][3];

    u32_to_le(o0, outb, 0);
    u32_to_le(o1, outb, 4);
    u32_to_le(o2, outb, 8);
    u32_to_le(o3, outb, 12);
}

/// Port of `AESEngine.decryptBlock` (equivalent inverse cipher; `kw` is the
/// decryption schedule BC produced via `generateWorkingKey(.., false)`).
///
/// # Panics
///
/// Same conditions as [`encrypt_block`]; prefer [`try_decrypt_block`].
pub fn decrypt_block(kw: &[[u32; 4]], inb: &[u8], outb: &mut [u8]) {
    demand_block_args(kw, inb, outb, "AESEngine.decryptBlock");
    let rounds = kw.len() - 1;
    let c0 = le_to_u32(inb, 0);
    let c1 = le_to_u32(inb, 4);
    let c2 = le_to_u32(inb, 8);
    let c3 = le_to_u32(inb, 12);

    let mut t0v = c0 ^ kw[rounds][0];
    let mut t1v = c1 ^ kw[rounds][1];
    let mut t2v = c2 ^ kw[rounds][2];

    let mut r = rounds - 1;
    let (mut r0, mut r1, mut r2);
    let mut r3 = c3 ^ kw[rounds][3];
    while r > 1 {
        r0 =
            ti(t0v) ^ sh(ti(r3 >> 8), 24) ^ sh(ti(t2v >> 16), 16) ^ sh(ti(t1v >> 24), 8) ^ kw[r][0];
        r1 =
            ti(t1v) ^ sh(ti(t0v >> 8), 24) ^ sh(ti(r3 >> 16), 16) ^ sh(ti(t2v >> 24), 8) ^ kw[r][1];
        r2 =
            ti(t2v) ^ sh(ti(t1v >> 8), 24) ^ sh(ti(t0v >> 16), 16) ^ sh(ti(r3 >> 24), 8) ^ kw[r][2];
        r3 =
            ti(r3) ^ sh(ti(t2v >> 8), 24) ^ sh(ti(t1v >> 16), 16) ^ sh(ti(t0v >> 24), 8) ^ kw[r][3];
        r -= 1;
        t0v = ti(r0) ^ sh(ti(r3 >> 8), 24) ^ sh(ti(r2 >> 16), 16) ^ sh(ti(r1 >> 24), 8) ^ kw[r][0];
        t1v = ti(r1) ^ sh(ti(r0 >> 8), 24) ^ sh(ti(r3 >> 16), 16) ^ sh(ti(r2 >> 24), 8) ^ kw[r][1];
        t2v = ti(r2) ^ sh(ti(r1 >> 8), 24) ^ sh(ti(r0 >> 16), 16) ^ sh(ti(r3 >> 24), 8) ^ kw[r][2];
        r3 = ti(r3) ^ sh(ti(r2 >> 8), 24) ^ sh(ti(r1 >> 16), 16) ^ sh(ti(r0 >> 24), 8) ^ kw[r][3];
        r -= 1;
    }

    r0 = ti(t0v) ^ sh(ti(r3 >> 8), 24) ^ sh(ti(t2v >> 16), 16) ^ sh(ti(t1v >> 24), 8) ^ kw[r][0];
    r1 = ti(t1v) ^ sh(ti(t0v >> 8), 24) ^ sh(ti(r3 >> 16), 16) ^ sh(ti(t2v >> 24), 8) ^ kw[r][1];
    r2 = ti(t2v) ^ sh(ti(t1v >> 8), 24) ^ sh(ti(t0v >> 16), 16) ^ sh(ti(r3 >> 24), 8) ^ kw[r][2];
    r3 = ti(r3) ^ sh(ti(t2v >> 8), 24) ^ sh(ti(t1v >> 16), 16) ^ sh(ti(t0v >> 24), 8) ^ kw[r][3];

    // final round: simple inverse S-box (instance `s` == Si here), key KW[0]
    let o0 = sibox(r0)
        ^ (sibox(r3 >> 8) << 8)
        ^ (sibox(r2 >> 16) << 16)
        ^ (sibox(r1 >> 24) << 24)
        ^ kw[0][0];
    let o1 = sibox(r1)
        ^ (sibox(r0 >> 8) << 8)
        ^ (sibox(r3 >> 16) << 16)
        ^ (sibox(r2 >> 24) << 24)
        ^ kw[0][1];
    let o2 = sibox(r2)
        ^ (sibox(r1 >> 8) << 8)
        ^ (sibox(r0 >> 16) << 16)
        ^ (sibox(r3 >> 24) << 24)
        ^ kw[0][2];
    let o3 = sibox(r3)
        ^ (sibox(r2 >> 8) << 8)
        ^ (sibox(r1 >> 16) << 16)
        ^ (sibox(r0 >> 24) << 24)
        ^ kw[0][3];

    u32_to_le(o0, outb, 0);
    u32_to_le(o1, outb, 4);
    u32_to_le(o2, outb, 8);
    u32_to_le(o3, outb, 12);
}

// ── Key schedule (port of `AESEngine.generateWorkingKey`) ──────────────────

/// `subWord` — apply the forward S-box to each byte of a packed word.
#[inline(always)]
fn subword(x: u32) -> u32 {
    (AES_S[(x & 255) as usize] as u32)
        | ((AES_S[((x >> 8) & 255) as usize] as u32) << 8)
        | ((AES_S[((x >> 16) & 255) as usize] as u32) << 16)
        | ((AES_S[((x >> 24) & 255) as usize] as u32) << 24)
}
// BC GF(2^8)-on-words helpers (constants m1..m5 from AESEngine).
#[inline(always)]
fn ffmulx(x: u32) -> u32 {
    ((x & 0x7f7f7f7f) << 1) ^ (((x & 0x80808080) >> 7).wrapping_mul(0x0000001b))
}
#[inline(always)]
fn ffmulx2(x: u32) -> u32 {
    let t0 = (x & 0x3f3f3f3f) << 2;
    let mut t1 = x & 0xC0C0C0C0;
    t1 ^= t1 >> 1;
    t0 ^ (t1 >> 2) ^ (t1 >> 5)
}
#[inline(always)]
fn inv_mcol(x: u32) -> u32 {
    let mut t0 = x;
    let mut t1 = t0 ^ sh(t0, 8);
    t0 ^= ffmulx(t1);
    t1 ^= ffmulx2(t0);
    t0 ^= t1 ^ sh(t1, 16);
    t0
}

/// Fail-loud [`generate_working_key`], naming the exception BouncyCastle's
/// `AESEngine.generateWorkingKey` itself raises for a bad key length
/// (`IllegalArgumentException("Key length not 128/192/256 bits.")`).
///
/// **Supported key lengths: 16, 24, 32 bytes only.** Everything else is
/// explicitly rejected — there is deliberately no zero-pad or truncate-to-16
/// fallback, because either would silently key the cipher with material the
/// caller never supplied.
pub fn try_generate_working_key(key: &[u8], for_encryption: bool) -> CryptoResult<Vec<[u32; 4]>> {
    generate_working_key(key, for_encryption).ok_or_else(|| {
        CryptoFailure::illegal_argument(format!(
            "Key length not 128/192/256 bits. (got {} bytes)",
            key.len()
        ))
    })
}

/// Port of `AESEngine.generateWorkingKey`. Returns the expanded schedule
/// `KW[round][col]` for a 16/24/32-byte key, or `None` for an invalid length
/// (the caller raises `IllegalArgumentException`, matching BC). `for_encryption
/// == false` applies the equivalent-inverse-cipher `inv_mcol` to the middle
/// round keys, exactly as BC does.
///
/// This one is already fail-loud in shape: `None` is not a usable schedule, so
/// no caller can mistake it for a successful key expansion, and the in-tree
/// caller does raise (`native-builtins/src/phases_late/bouncycastle.rs:5837`
/// → `bc_aes_bad_key()`). [`try_generate_working_key`] simply attaches the JDK
/// exception name to the same refusal.
pub fn generate_working_key(key: &[u8], for_encryption: bool) -> Option<Vec<[u32; 4]>> {
    let key_len = key.len();
    if key_len < 16 || key_len > 32 || (key_len & 7) != 0 {
        return None;
    }
    let kc = key_len >> 2;
    let rounds = kc + 6;
    let mut w = vec![[0u32; 4]; rounds + 1];

    match kc {
        4 => {
            const RCON: [u32; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];
            let (mut c0, mut c1, mut c2, mut c3) = (
                le_to_u32(key, 0),
                le_to_u32(key, 4),
                le_to_u32(key, 8),
                le_to_u32(key, 12),
            );
            w[0] = [c0, c1, c2, c3];
            for i in 1..=10 {
                let colx = subword(sh(c3, 8)) ^ RCON[i - 1];
                c0 ^= colx;
                c1 ^= c0;
                c2 ^= c1;
                c3 ^= c2;
                w[i] = [c0, c1, c2, c3];
            }
        }
        6 => {
            let (mut c0, mut c1, mut c2, mut c3) = (
                le_to_u32(key, 0),
                le_to_u32(key, 4),
                le_to_u32(key, 8),
                le_to_u32(key, 12),
            );
            w[0] = [c0, c1, c2, c3];
            let mut c4 = le_to_u32(key, 16);
            let mut c5 = le_to_u32(key, 20);
            let mut i = 1usize;
            let mut rcon = 1u32;
            loop {
                w[i][0] = c4;
                w[i][1] = c5;
                let colx = subword(sh(c5, 8)) ^ rcon;
                rcon <<= 1;
                c0 ^= colx;
                w[i][2] = c0;
                c1 ^= c0;
                w[i][3] = c1;
                c2 ^= c1;
                w[i + 1][0] = c2;
                c3 ^= c2;
                w[i + 1][1] = c3;
                c4 ^= c3;
                w[i + 1][2] = c4;
                c5 ^= c4;
                w[i + 1][3] = c5;
                let colx = subword(sh(c5, 8)) ^ rcon;
                rcon <<= 1;
                c0 ^= colx;
                w[i + 2][0] = c0;
                c1 ^= c0;
                w[i + 2][1] = c1;
                c2 ^= c1;
                w[i + 2][2] = c2;
                c3 ^= c2;
                w[i + 2][3] = c3;
                i += 3;
                if i >= 13 {
                    break;
                }
                c4 ^= c3;
                c5 ^= c4;
            }
        }
        8 => {
            let (mut c0, mut c1, mut c2, mut c3) = (
                le_to_u32(key, 0),
                le_to_u32(key, 4),
                le_to_u32(key, 8),
                le_to_u32(key, 12),
            );
            w[0] = [c0, c1, c2, c3];
            let (mut c4, mut c5, mut c6, mut c7) = (
                le_to_u32(key, 16),
                le_to_u32(key, 20),
                le_to_u32(key, 24),
                le_to_u32(key, 28),
            );
            w[1] = [c4, c5, c6, c7];
            let mut i = 2usize;
            let mut rcon = 1u32;
            loop {
                let colx = subword(sh(c7, 8)) ^ rcon;
                rcon <<= 1;
                c0 ^= colx;
                w[i][0] = c0;
                c1 ^= c0;
                w[i][1] = c1;
                c2 ^= c1;
                w[i][2] = c2;
                c3 ^= c2;
                w[i][3] = c3;
                i += 1;
                if i >= 15 {
                    break;
                }
                let colx2 = subword(c3);
                c4 ^= colx2;
                w[i][0] = c4;
                c5 ^= c4;
                w[i][1] = c5;
                c6 ^= c5;
                w[i][2] = c6;
                c7 ^= c6;
                w[i][3] = c7;
                i += 1;
            }
        }
        _ => return None,
    }

    if !for_encryption {
        for j in 1..rounds {
            for col in 0..4 {
                w[j][col] = inv_mcol(w[j][col]);
            }
        }
    }
    Some(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }
    fn arr16(s: &str) -> [u8; 16] {
        let mut b = [0u8; 16];
        b.copy_from_slice(&unhex(s));
        b
    }

    // Validate the full chain — generate_working_key + encrypt/decrypt_block —
    // against FIPS-197 known-answer vectors.
    fn kat(key_hex: &str, pt_hex: &str, ct_hex: &str) {
        let key = unhex(key_hex);
        let pt = arr16(pt_hex);
        let expect_ct = arr16(ct_hex);

        let enc = generate_working_key(&key, true).unwrap();
        let mut ct = [0u8; 16];
        encrypt_block(&enc, &pt, &mut ct);
        assert_eq!(ct, expect_ct, "encrypt KAT key={key_hex}");

        let dec = generate_working_key(&key, false).unwrap();
        let mut back = [0u8; 16];
        decrypt_block(&dec, &ct, &mut back);
        assert_eq!(back, pt, "decrypt round-trip key={key_hex}");
    }

    #[test]
    fn fips197_aes128() {
        // FIPS-197 Appendix C.1 and Appendix B
        kat(
            "000102030405060708090a0b0c0d0e0f",
            "00112233445566778899aabbccddeeff",
            "69c4e0d86a7b0430d8cdb78070b4c55a",
        );
        kat(
            "2b7e151628aed2a6abf7158809cf4f3c",
            "3243f6a8885a308d313198a2e0370734",
            "3925841d02dc09fbdc118597196a0b32",
        );
    }

    #[test]
    fn fips197_aes192_aes256() {
        // FIPS-197 Appendix C.2 (AES-192) and C.3 (AES-256)
        kat(
            "000102030405060708090a0b0c0d0e0f1011121314151617",
            "00112233445566778899aabbccddeeff",
            "dda97ca4864cdfe06eaf70a0ec0d7191",
        );
        kat(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "00112233445566778899aabbccddeeff",
            "8ea2b7ca516745bfeafc49904b496089",
        );
    }

    #[test]
    fn invalid_key_length_is_none() {
        assert!(generate_working_key(&[0u8; 15], true).is_none());
        assert!(generate_working_key(&[0u8; 20], true).is_none());
        assert!(generate_working_key(&[0u8; 33], true).is_none());
    }

    // ---- fail-loud guards ----

    /// Exactly the three FIPS-197 key sizes are accepted; every other length
    /// raises rather than producing a schedule from truncated/padded material.
    #[test]
    fn supported_key_lengths_succeed_and_others_raise() {
        for len in [16usize, 24, 32] {
            let k = vec![0u8; len];
            assert!(
                try_generate_working_key(&k, true).is_ok(),
                "{len}-byte key should be supported"
            );
            assert!(try_generate_working_key(&k, false).is_ok());
        }
        for len in [0usize, 1, 8, 15, 17, 20, 23, 25, 31, 33, 64] {
            let err = try_generate_working_key(&vec![0u8; len], true)
                .expect_err("unsupported key length must be rejected");
            assert_eq!(
                err.java_class(),
                "java/lang/IllegalArgumentException",
                "{len}-byte key: wrong exception type"
            );
            assert!(
                err.message().contains("128/192/256"),
                "{len}-byte key: message should name the supported sizes"
            );
        }
    }

    /// An unkeyed or malformed engine must raise, not encrypt. Before the guard
    /// an empty schedule underflowed `kw.len() - 1`, and a *non-empty* but
    /// wrong-length one (2 or 4 entries) still ran off the end of `kw` on the
    /// final round — so a lower bound alone would not have been enough.
    #[test]
    fn malformed_key_schedule_raises_illegal_state() {
        let inb = [0u8; 16];
        let mut outb = [0u8; 16];
        for len in [0usize, 1, 2, 3, 4, 10, 12, 14, 16] {
            let kw = vec![[0u32; 4]; len];
            let err = try_encrypt_block(&kw, &inb, &mut outb)
                .expect_err("malformed schedule must be rejected");
            assert_eq!(
                err.java_class(),
                "java/lang/IllegalStateException",
                "{len}-entry schedule: wrong exception type"
            );
            let err = try_decrypt_block(&kw, &inb, &mut outb)
                .expect_err("malformed schedule must be rejected");
            assert_eq!(err.java_class(), "java/lang/IllegalStateException");
        }
        // ...and the output buffer was left untouched — no zero "ciphertext"
        // was produced that a caller could mistake for a real transform.
        assert_eq!(outb, [0u8; 16]);
    }

    /// The three real AES schedule lengths are accepted, so the strict check
    /// cannot reject a legitimately keyed engine.
    #[test]
    fn every_real_key_size_produces_an_accepted_schedule() {
        let inb = [0u8; 16];
        let mut outb = [0u8; 16];
        for key_len in [16usize, 24, 32] {
            let kw = generate_working_key(&vec![0u8; key_len], true).unwrap();
            assert!(
                VALID_SCHEDULE_LENS.contains(&kw.len()),
                "{key_len}-byte key produced a {}-entry schedule",
                kw.len()
            );
            try_encrypt_block(&kw, &inb, &mut outb).expect("real schedule must be accepted");
            let kw = generate_working_key(&vec![0u8; key_len], false).unwrap();
            try_decrypt_block(&kw, &inb, &mut outb).expect("real schedule must be accepted");
        }
    }

    /// A short input or output block raises rather than panicking on the
    /// `le_to_u32`/`u32_to_le` index.
    #[test]
    fn short_block_raises_illegal_argument() {
        let kw = generate_working_key(&[0u8; 16], true).unwrap();
        let mut out16 = [0u8; 16];
        let err = try_encrypt_block(&kw, &[0u8; 15], &mut out16).expect_err("short input");
        assert_eq!(err.java_class(), "java/lang/IllegalArgumentException");

        let mut out15 = [0u8; 15];
        let err = try_encrypt_block(&kw, &[0u8; 16], &mut out15).expect_err("short output");
        assert_eq!(err.java_class(), "java/lang/IllegalArgumentException");
    }

    /// The infallible pair cannot report a refusal, so it aborts — but it now
    /// aborts *deterministically and by name*, before any table lookup, rather
    /// than through whichever index happens to run off the end of `kw` first.
    /// A 2-entry schedule is the interesting case: it passes the `kw.len() >= 2`
    /// lower bound every in-tree caller uses, and the round structure then
    /// reads `kw[rounds]` past the end.
    #[test]
    #[should_panic(expected = "AES engine not initialised")]
    fn infallible_encrypt_block_aborts_on_a_two_entry_schedule() {
        let mut outb = [0u8; 16];
        encrypt_block(&vec![[0u32; 4]; 2], &[0u8; 16], &mut outb);
    }

    #[test]
    #[should_panic(expected = "AES engine not initialised")]
    fn infallible_decrypt_block_aborts_on_an_empty_schedule() {
        let mut outb = [0u8; 16];
        decrypt_block(&[], &[0u8; 16], &mut outb);
    }

    #[test]
    #[should_panic(expected = "AES block must be 16 bytes")]
    fn infallible_encrypt_block_aborts_on_a_short_block() {
        let kw = generate_working_key(&[0u8; 16], true).unwrap();
        let mut outb = [0u8; 16];
        encrypt_block(&kw, &[0u8; 15], &mut outb);
    }

    /// The fail-loud wrappers must be byte-identical to the infallible pair on
    /// every well-formed input — the guard adds a refusal, it does not change
    /// the transform.
    #[test]
    fn try_wrappers_match_the_infallible_pair() {
        let key = unhex("000102030405060708090a0b0c0d0e0f");
        let pt = arr16("00112233445566778899aabbccddeeff");
        let enc = generate_working_key(&key, true).unwrap();

        let mut a = [0u8; 16];
        encrypt_block(&enc, &pt, &mut a);
        let mut b = [0u8; 16];
        try_encrypt_block(&enc, &pt, &mut b).expect("well-formed input");
        assert_eq!(a, b);

        let dec = generate_working_key(&key, false).unwrap();
        let mut c = [0u8; 16];
        decrypt_block(&dec, &a, &mut c);
        let mut d = [0u8; 16];
        try_decrypt_block(&dec, &a, &mut d).expect("well-formed input");
        assert_eq!(c, d);
        assert_eq!(c, pt);
    }
}
