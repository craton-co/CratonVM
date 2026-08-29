# Fix note — classloading-sig (V1 + V2)

## Finding

From `docs/reviews/fable-2026-06-10/classloading.md`:

- **V1 (HIGH)** — Signed-JAR trust is incomplete: `extract_jar_signer_blocks`
  verifies the PKCS#7 signer block + the signature over `MANIFEST.MF` (via the
  `.SF`), but never checks per-entry manifest digests. A signed JAR whose class
  body was swapped (leaving `MANIFEST.MF`/`.SF`/`.RSA` intact) still passes, so
  `Class.getCodeSource().getCertificates()` reports the original signer for
  tampered bytes.
- **V2 (HIGH DoS)** — The zip-bomb defense (`safe_with_capacity`) clamps only the
  `Vec::with_capacity` pre-allocation, not the streaming inflate. A
  small-declared / huge-actual deflate entry (≈1000:1) still blows memory because
  `read_to_end` grows the buffer to the full inflated size regardless of the
  512 MiB constant.

## Root cause

- **V1**: the JAR signing trust chain is
  `signature -> .SF -> MANIFEST.MF -> per-entry digest -> bytes`. The code
  validated only the first two links. The third/fourth links
  (`.SF`'s `*-Digest-Manifest` binding the manifest, and each manifest
  `*-Digest` binding the entry bytes) were unimplemented (the report's "Stubs"
  section flagged this as the absent content-binding half).
- **V2**: every entry read used
  `Vec::with_capacity(safe_with_capacity(size)); entry.read_to_end(&mut v)`.
  `safe_with_capacity` bounds the *capacity hint* only; `read_to_end` reads the
  whole inflate stream and reallocates past the hint without limit.

## Exact change (file:line)

### V2 — streaming inflate bound (`classloading/src/class_path.rs`)
- Added `read_entry_capped<R: Read>(reader, declared_size) -> io::Result<Vec<u8>>`
  (`class_path.rs:~580`). It reads via `reader.take(MAX_UNCOMPRESSED_ENTRY_BYTES + 1)`
  and returns `io::Error(InvalidData)` the instant the actual inflated total
  exceeds `MAX_UNCOMPRESSED_ENTRY_BYTES` (512 MiB). The `Vec` is still seeded with
  the clamped capacity via `safe_with_capacity`, so the cap — not the
  attacker-declared size — is the authoritative bound.
- Replaced all 11 entry-read sites that previously used `read_to_end`:
  - `read_manifest` (`:~1086`)
  - fat-jar `BOOT-INF/classes` + `WEB-INF/classes` (`:~1178`, `:~1191`)
  - nested-JAR extraction (`:~1224`)
  - `build_nested_directory_from_jar` (`:~1429`)
  - signer-block read + `.SF` read in `extract_jar_signer_blocks` (`:~1880`, `:~1907`)
  - `verify_signed_entries` manifest + entry reads (`:~1973`, `:~1994`)
  - `find_in_archive` (`:~2804`)
  - `load_jmod` `classes/` pre-extract (`:~2893`)
  - In the 5 `.and_then(ZipResult)` closures the call is wrapped
    `Ok(read_entry_capped(...)?)` so the `io::Error` overflow converts to
    `zip::result::ZipError` (via its `#[from] io::Error`).

### V1 — per-entry digest binding
`classloading/src/jar_signer.rs` (new public helpers, inserted before the test
module, `:~2842`):
- `verify_sf_binds_manifest(sf_bytes, manifest_bytes) -> bool` — matches the
  `.SF` main-section `<alg>-Digest-Manifest` against `H_alg(manifest_bytes)`.
  Picks the strongest declared algorithm; fail-closed if absent. Intentionally
  does NOT accept `-Digest-Manifest-Main-Attributes` (that covers only the main
  section, not the per-entry digests we go on to trust).
- `parse_manifest_entry_digests(manifest_bytes) -> Vec<ManifestEntryDigest>` —
  parses each per-entry manifest section into `(name, alg, expected_digest)`,
  strongest `<alg>-Digest` per section; directory/no-digest sections dropped;
  4 KiB cap on `Name`.
- `digest_matches(alg, data, expected) -> bool` — constant-time
  (`ct_eq`) compare of `H_alg(data)` vs the recorded digest.
- Supporting: `ManifestEntryDigest` struct, `manifest_digest_alg`,
  `digest_strength`, `fold_manifest_lines` (self-contained manifest line-folder).

`classloading/src/class_path.rs`:
- New `ClassPath::verify_signed_entries(archive, sf_bytes) -> bool` (`:~1966`):
  reads `../../../../../apps/META-INF/MANIFEST.MF`, requires the `.SF` to bind it
  (`verify_sf_binds_manifest`), then for every entry the manifest digests,
  re-reads the entry and requires `digest_matches`. Fail-closed on missing
  manifest, missing/unreadable signed entry, unsafe entry name
  (`is_safe_entry_name`), or any digest mismatch.
- `extract_jar_signer_blocks` (`:~1934`): after `verify_signer_block` succeeds,
  the signer's certs are pushed **only if** `verify_signed_entries` passes;
  otherwise certs are dropped (CodeSource reported unsigned) — matching HotSpot's
  `jarsigner -verify` rejection of a digest mismatch.

## Files touched
- `classloading/src/jar_signer.rs`
- `classloading/src/class_path.rs`
- `docs/reviews/fable-2026-06-10/fixes/classloading-sig.md` (this note)

## Tests added
`jar_signer.rs` `#[cfg(test)]` (pure-function, no crypto fixtures; local
`b64enc`):
- `sf_binds_manifest_accepts_matching_digest`
- `sf_binds_manifest_rejects_tampered_manifest`
- `sf_binds_manifest_fails_closed_without_digest_manifest`
- `parse_manifest_entry_digests_extracts_each_section` (also covers tamper-reject)
- `parse_manifest_entry_digests_picks_strongest_alg`

No `class_path.rs` test added: an end-to-end content-tamper test needs a
fully-built signed-JAR fixture (private to `tests/wp_security_robustness.rs`,
not owned here). The report's gap #2 (plant signed JAR, swap class body, assert
certs dropped) should be added there — see Follow-up.

## Follow-up & risk

- **Compile risk — low/medium.** `read_entry_capped` uses `Read::take` +
  `Take::read_to_end` (std) and the `Ok(...?)` form relies on
  `ZipError: From<io::Error>` (confirmed via `#[from] io::Error` in
  `zip-2.4.2/src/result.rs`). `let ... else` and `&mut guard` deref-coercion
  to `&mut ZipArchive` both already appear in this file. Cannot `cargo build`
  in this environment (forbidden) — confidence is from static checking and
  mirroring existing idioms.
- **Behavior risk — low.** V2 only changes behavior for entries that actually
  inflate beyond 512 MiB (previously unbounded → OOM; now a clean skip/err).
  V1 only changes the signed-JAR path: a correctly-signed, untampered JAR still
  produces the same certs (the new checks pass); a tampered or
  unbindable-manifest JAR now yields empty certs instead of trusting the signer.
- **Real-world note.** `*-Digest-Manifest` is computed over the exact manifest
  bytes; we hash the bytes `read_entry_capped` returns (the verbatim entry),
  matching jarsigner. Multi-release / per-section manifest edge cases beyond the
  standard `Name:`/`<alg>-Digest:` layout are handled by the strongest-digest
  selection; exotic manifests that omit `*-Digest-Manifest` fail closed.
- **Suggested follow-ups** (not in scope / not owned here):
  1. Add the content-tamper integration test in
     `classloading/tests/wp_security_robustness.rs` (report gap #2) and a real
     high-ratio deflate-bomb test asserting `read_entry_capped` rejects (gap #1).
  2. P1: memoize verified certs per-archive (`extract_jar_signer_blocks` runs
     per class lookup; `verify_signed_entries` now re-reads + re-hashes every
     signed entry each call — correctness is fine, cost is higher; a
     `OnceLock<Vec<Vec<u8>>>` on the entry would fix both P1 and this).
