# Third-Party Notices

CratonVM (Copyright 2024-2026 Craton Software Company) is licensed under the
Apache License, Version 2.0 (see `LICENSE` and `NOTICE`). It bundles, ports, or
links the third-party software listed below, each under its own permissive
license. This file exists to satisfy the attribution requirements of those
licenses and Apache-2.0 §4(d) (NOTICE propagation). It ships inside the relevant
published crates so the attribution travels with the artifact.

If you redistribute CratonVM (in source or binary form), you must retain the
applicable notices below.

---

## 1. Ported / transcribed source

The following files in the `cratonvm-native-builtins` crate
(`native-builtins/src/`) are mechanically transcribed ("verbatim / faithful")
ports of Java source and precomputed tables from **The Legion of the Bouncy
Castle Inc.** (https://www.bouncycastle.org/). They reproduce BouncyCastle's
algorithm structure and constant tables so that the native fast-paths are
bit-identical to the interpreted BouncyCastle bytecode by construction.

| File | Ported from (BouncyCastle) |
| --- | --- |
| `bc_aes.rs` | `org.bouncycastle.crypto.engines.AESEngine` (T-tables, S-box, round structure) |
| `bc_chacha.rs` | ChaCha permutation kernels (`org.bouncycastle.crypto.engines.ChaChaEngine` / Salsa20 core) used by SPHINCS-256 |
| `bc_newhope.rs` | NewHope lattice kernels (`NTT` / `Reduce` / `Poly`) |
| `bc_newhope_tables.rs` | NewHope precomputed Montgomery / NTT tables (`Precomp.java`, `NTT.java`) |

**Origin & copyright:** Portions of these files are derived from BouncyCastle,
Copyright (c) 2000-2024 The Legion of the Bouncy Castle Inc. The Craton
copyright header on these files applies only to the Craton-authored
glue/integration code; the algorithm tables and round logic are the work of the
BouncyCastle authors and are used under the Bouncy Castle Licence (reproduced
below).

### Bouncy Castle Licence (MIT-style)

> Copyright (c) 2000-2024 The Legion of the Bouncy Castle Inc.
> (https://www.bouncycastle.org)
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

> Note: some in-file docstrings refer to "BouncyCastle, Apache-2.0". The Bouncy
> Castle distribution itself is published under the MIT-style Bouncy Castle
> Licence reproduced above; that licence (not Apache-2.0) governs the
> BouncyCastle-derived portions. This is an attribution clarification only — the
> MIT-style licence is permissive and compatible with redistribution inside this
> Apache-2.0 project.

---

## 2. Dependency licenses

CratonVM depends on the following permissive third-party Rust crates (and the
system libraries some of them link). This list highlights the
license-sensitive entries; the authoritative, complete dependency graph and
versions are in `Cargo.lock`.

| Crate | License (SPDX-ish) | Notes |
| --- | --- | --- |
| `ring` | ISC AND MIT AND OpenSSL | Aggregate; includes BoringSSL/OpenSSL-derived asm |
| `openssl` / `openssl-sys` | Apache-2.0 | Links system OpenSSL 3.x (Apache-2.0) |
| `libsqlite3-sys` | MIT | Bundles SQLite (public domain) |
| `p256`, `sha2`, `sha3`, `aes`, `aes-gcm` | Apache-2.0 OR MIT | RustCrypto project |
| `getrandom`, `rand*` | Apache-2.0 OR MIT | |
| `p12` | MIT/Apache-2.0 | PKCS#12 reader (keystore tests) |
| `fontdue` | MIT OR Apache-2.0 OR Zlib | Glyph rasterization (AWT backends) |
| `cudarc` | MIT OR Apache-2.0 | Optional, behind the `cuda` feature |
| `mimalloc` | MIT | Optional allocator |
| `libffi` / `libffi-sys` | MIT | Native call thunks |
| `windows` | MIT OR Apache-2.0 | Windows AWT backend |
| `x11rb` | MIT OR Apache-2.0 | Linux AWT backend |
| `objc2*` | MIT OR Apache-2.0 | macOS AWT backend |
| `schannel`, `security-framework` | MIT OR Apache-2.0 | Platform TLS roots |
| `thiserror`, `parking_lot`, `rustc-hash`, `smallvec`, `tracing`, `clap`, `hashbrown`, `bitflags`, `bytemuck` | Apache-2.0 OR MIT | Core utility crates |

No copyleft (GPL / LGPL / AGPL) crate was found in `Cargo.lock`.

The full per-crate license text for each dependency is reproduced in that
crate's own source distribution and can be regenerated with a tool such as
`cargo about` or `cargo deny`.
