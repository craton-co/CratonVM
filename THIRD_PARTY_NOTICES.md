# Third-Party Notices

CratonVM is distributed under the Apache License, Version 2.0 (see
[LICENSE](LICENSE) and [NOTICE](NOTICE)). It incorporates a number of
third-party open-source components. This document enumerates the **direct**
workspace dependencies declared in the CratonVM `Cargo.toml` files, with
their upstream licences and SPDX identifiers, so downstream redistributors
can satisfy Section 4 of the Apache 2.0 licence.

This file does **not** attempt to enumerate transitive dependencies. For a
complete graph use `cargo tree` or `cargo about generate` against the
locked manifest. For machine-readable SBOM output, see
`cargo metadata --format-version 1`.

## How to Read This File

- **SPDX**: the SPDX licence identifier as declared by the upstream crate.
- For dual-licensed crates marked `Apache-2.0 OR MIT`, CratonVM elects to
  use the **Apache-2.0** option, consistent with the project's own licence.
- "Notes" calls out NOTICE-file preservation obligations or unusual terms.

For the full text of each licence, follow the upstream crate's repository
link. NOTICE files of Apache-2.0 upstreams are reproduced in
[`dist/THIRD_PARTY_NOTICES/`](dist/) where applicable in release tarballs.

---

## Apache-2.0 OR MIT (CratonVM uses Apache-2.0)

The bulk of the Rust ecosystem is dual-licensed under `Apache-2.0 OR MIT`.
CratonVM elects Apache-2.0 for each of the following.

| Crate | Used by | Upstream |
|-------|---------|----------|
| `thiserror` | workspace-wide | https://github.com/dtolnay/thiserror |
| `tracing` | workspace-wide | https://github.com/tokio-rs/tracing |
| `tracing-subscriber` | workspace-wide | https://github.com/tokio-rs/tracing |
| `parking_lot` | workspace-wide | https://github.com/Amanieu/parking_lot |
| `rustc-hash` | workspace-wide | https://github.com/rust-lang/rustc-hash |
| `smallvec` | `jfr`, `vm` | https://github.com/servo/rust-smallvec |
| `bitflags` | `reader`, `native-awt` | https://github.com/bitflags/bitflags |
| `strum` | `reader` | https://github.com/Peternator7/strum |
| `hashbrown` | `classloading` | https://github.com/rust-lang/hashbrown |
| `memmap2` | `classloading`, `native-io` | https://github.com/RazrFalcon/memmap2-rs |
| `socket2` | `native-api`, `native-builtins` | https://github.com/rust-lang/socket2 |
| `native-tls` | `native-api`, `native-builtins` | https://github.com/sfackler/rust-native-tls |
| `regex` | `native-io`, `native-builtins`, `vm` | https://github.com/rust-lang/regex |
| `libc` | `native-io`, `native-builtins`, `vm` | https://github.com/rust-lang/libc |
| `tempfile` | `native-io`, `native-builtins`, `vm` (dev) | https://github.com/Stebalien/tempfile |
| `clap` | `vm-cli` | https://github.com/clap-rs/clap |
| `anyhow` | `vm-cli` | https://github.com/dtolnay/anyhow |
| `serde` | `vm` | https://github.com/serde-rs/serde |
| `serde_json` | `vm` | https://github.com/serde-rs/json |
| `indexmap` | `vm` | https://github.com/indexmap-rs/indexmap |
| `bitfield-struct` | `vm` | https://github.com/wrenger/bitfield-struct-rs |
| `libloading` | `vm` | https://github.com/nagisa/rust_libloading |
| `criterion` | `vm` (dev) | https://github.com/bheisler/criterion.rs |
| `notify` | `native-io` | https://github.com/notify-rs/notify |
| `libffi` | `native-builtins` | https://github.com/tov/libffi-rs |
| `fancy-regex` | `native-builtins` | https://github.com/fancy-regex/fancy-regex |
| `flate2` | `native-builtins` | https://github.com/rust-lang/flate2-rs |
| `unicode-normalization` | `native-builtins` | https://github.com/unicode-rs/unicode-normalization |
| `httparse` | `native-builtins` | https://github.com/seanmonstar/httparse |
| `rusqlite` | `native-builtins` | https://github.com/rusqlite/rusqlite |
| `quick-xml` | `native-builtins` | https://github.com/tafia/quick-xml |
| `sha1` | `native-builtins` | https://github.com/RustCrypto/hashes |
| `sha3` | `native-builtins` | https://github.com/RustCrypto/hashes |
| `aes` | `native-builtins` | https://github.com/RustCrypto/block-ciphers |
| `aes-gcm` | `native-builtins` | https://github.com/RustCrypto/AEADs |
| `cbc` | `native-builtins` | https://github.com/RustCrypto/block-modes |
| `ctr` | `native-builtins` | https://github.com/RustCrypto/stream-ciphers |
| `chacha20poly1305` | `native-builtins` | https://github.com/RustCrypto/AEADs |
| `getrandom` | `native-builtins` | https://github.com/rust-random/getrandom |
| `ed25519-dalek` | `native-builtins` | https://github.com/dalek-cryptography/curve25519-dalek |
| `p12` | `native-builtins` | https://github.com/hjiayz/p12 |
| `bytemuck` | `cuda-bridge`, `vm` | https://github.com/Lokathor/bytemuck |
| `fontdue` | `native-awt` | https://github.com/mooman219/fontdue |
| `cesu8` | `reader` | https://github.com/emk/cesu8-rs |
| `mimalloc` | `vm-cli` | https://github.com/purpleprotocol/mimalloc_rust |

SPDX: `Apache-2.0 OR MIT` (CratonVM elects `Apache-2.0`).

NOTICE files: where an upstream provides a `NOTICE` file (e.g. `tracing`,
`parking_lot`, `serde`, `rustls`), redistributors of CratonVM binaries must
preserve them under the Apache-2.0 attribution requirement.

---

## Apache-2.0

| Crate | Used by | SPDX | Upstream |
|-------|---------|------|----------|
| `rustls` | `native-builtins` | `Apache-2.0 OR ISC OR MIT` (we use Apache-2.0) | https://github.com/rustls/rustls |
| `rustls-pemfile` | `native-builtins` | `Apache-2.0 OR ISC OR MIT` (we use Apache-2.0) | https://github.com/rustls/pemfile |
| `rustls-pki-types` | `native-builtins` | `Apache-2.0 OR ISC OR MIT` (we use Apache-2.0) | https://github.com/rustls/pki-types |
| `rustls-native-certs` | `native-builtins` | `Apache-2.0 OR ISC OR MIT` (we use Apache-2.0) | https://github.com/rustls/rustls-native-certs |
| `cudarc` | `cuda-bridge` (optional, gated on `cuda` feature) | `Apache-2.0 OR MIT` (we use Apache-2.0) | https://github.com/coreylowman/cudarc |

NOTICE: `rustls` is additionally subject to the ISC option; the project
elects Apache-2.0. `cudarc` links the NVIDIA CUDA Driver API at runtime —
the CUDA toolkit itself is **not** redistributed by CratonVM and is
governed by NVIDIA's separate End User License Agreement.

---

## MIT-only

| Crate | Used by | SPDX | Upstream |
|-------|---------|------|----------|
| `zip` (2.x) | `native-io` (workspace dep) | `MIT` | https://github.com/zip-rs/zip2 |
| `zip` (0.6) | `native-builtins` (legacy pin) | `MIT` | https://github.com/zip-rs/zip-old |

NOTICE: pinning rationale for the dual `zip` versions is documented in the
root `Cargo.toml`.

---

## Windows / macOS / Linux platform crates

These are conditionally compiled per-target.

| Crate | Target | SPDX |
|-------|--------|------|
| `windows` | Windows (`native-awt`) | `Apache-2.0 OR MIT` (we use Apache-2.0) |
| `objc2` | macOS (`native-awt`) | `Apache-2.0 OR MIT` (we use Apache-2.0) |
| `objc2-foundation` | macOS (`native-awt`) | `Apache-2.0 OR MIT` (we use Apache-2.0) |
| `objc2-app-kit` | macOS (`native-awt`) | `Apache-2.0 OR MIT` (we use Apache-2.0) |
| `x11rb` | Linux/BSD (`native-awt`) | `Apache-2.0 OR MIT` (we use Apache-2.0) |

---

## Fuzzing (not shipped in release binaries)

| Crate | Used by | SPDX | Upstream |
|-------|---------|------|----------|
| `libfuzzer-sys` | `fuzz` crate | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/rust-fuzz/libfuzzer |

The `fuzz` crate is excluded from default builds (`publish = false`,
`version = "0.0.0"`); these notices apply only when running
`cargo fuzz`.

---

## Updating This File

When a direct workspace dependency is added or removed:

1. Update the table above with the new crate, its declared SPDX, and the
   upstream URL.
2. If the new dep is Apache-2.0 (not dual-licensed), check whether it ships
   a `NOTICE` file and arrange to bundle it under `dist/THIRD_PARTY_NOTICES/`
   on release.
3. Mention the change in [CHANGELOG.md](CHANGELOG.md) under
   *Dependencies*.

For a machine-generated cross-check, run:

```bash
cargo install cargo-about
cargo about generate about.hbs > THIRD_PARTY_NOTICES.generated.md
```

and diff the result against this hand-maintained file.
