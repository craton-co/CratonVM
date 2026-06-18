# Fix: oss-meta — OSS release metadata

## Finding
The `oss-readiness.md` audit flagged several metadata-level release blockers/warnings:
the default publish gate is wide open (immature GPU/AWT crates would publish to
crates.io); the verbatim BouncyCastle ports in `native-builtins` have no
attribution surface and NOTICE has no third-party section; `craton-gpu/Cargo.toml`
is the only member missing `[lints] workspace = true`; `gc/src/shadow_stack.rs` is
the one production source file lacking an SPDX header; and `homepage` duplicates
`repository` instead of pointing at the company site.

## Root cause
Metadata/attribution gaps only — no behavioral defect. The publish policy was
"everything except `fuzz` is publishable" with no per-crate opt-out; the
third-party attribution that Apache-2.0 §4(d) expects for bundled BC ports was
never authored.

## Exact change
1. **Publish gates** — added `publish = false` to `[package]` of:
   - `cuda-bridge/Cargo.toml`
   - `jit-cuda/Cargo.toml`
   - `craton-gpu/Cargo.toml`
   - `native-awt/Cargo.toml`
   Library crates left publishable (no `publish` key). Versions untouched (0.3.0);
   no path-dep `version=` literals changed.
2. **Third-party attribution** — created `THIRD-PARTY-NOTICES.md` at repo root
   enumerating the BouncyCastle-derived files (`bc_aes.rs`, `bc_chacha.rs`,
   `bc_newhope.rs`, `bc_newhope_tables.rs`; enumerated via
   `glob native-builtins/src/bc_*.rs`) under the Bouncy Castle Licence (MIT-style,
   full text included) with an attribution-clarification note that the in-file
   docstrings' "Apache-2.0" label is the BC distribution's MIT-style licence, plus
   a table of the permissive Rust deps (ring/OpenSSL aggregate, RustCrypto,
   fontdue, cudarc, libffi, p12, windows/x11rb/objc2, etc.). Appended one pointer
   line to `NOTICE`: "This product includes third-party software; see
   THIRD-PARTY-NOTICES.md." (NOTICE keeps its Apache-2.0 statement.)
3. **Lint inheritance** — added `[lints]\nworkspace = true` to
   `craton-gpu/Cargo.toml` (placed after `[lib]` so the `[lib]` table is not
   captured).
4. **SPDX** — prepended `// SPDX-License-Identifier: Apache-2.0` +
   `// Copyright 2024-2026 Craton Software Company` to `gc/src/shadow_stack.rs`
   above the existing `//!` module doc. No other `.rs` file touched.
5. **Homepage** — set `homepage = "https://craton.com.ar"` in root
   `[workspace.package]`; `repository` left at the GitHub URL.

## Files touched
- `cuda-bridge/Cargo.toml`, `jit-cuda/Cargo.toml`, `craton-gpu/Cargo.toml`,
  `native-awt/Cargo.toml` — `publish = false` (+ `[lints]` on craton-gpu)
- `Cargo.toml` (root) — `homepage`
- `NOTICE` — third-party pointer line
- `THIRD-PARTY-NOTICES.md` — NEW
- `gc/src/shadow_stack.rs` — SPDX header

## Tests added
None (metadata/docs only; no testable code path).

## Follow-up & risk
- Low risk: TOML/comment additions and a header; the publish gates only restrict
  `cargo publish`, they don't affect builds. `craton-gpu` `[lints]` was placed
  after `[lib]` to avoid TOML table-capture.
- The per-file `bc_*.rs` headers still carry sole-Craton copyright + an
  "Apache-2.0" claim over BC-derived code. Correcting those in-file headers
  (dual SPDX `MIT AND Apache-2.0` + a `// Portions derived from BouncyCastle`
  line) is owned by the native-builtins fixer this round, not oss-meta — the
  repo-level `THIRD-PARTY-NOTICES.md` now provides the attribution surface
  regardless.
- Domain mismatch follow-up (out of scope this round): `SECURITY.md`,
  `MAINTAINERS.md`, and the `craton.co` org/email references still use `.co`
  while `homepage` now uses `craton.com.ar`. `SECURITY.md` is owned by no agent
  this round — owner must reconcile `craton.co` vs `craton.com.ar`.
- RELEASING.md §3 still contradicts the real publish config (default =
  publishable; withhold via `publish = false`). docs-fixes owns RELEASING.md.
- crate-name availability on crates.io still unverified (offline).
