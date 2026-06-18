# CratonVM `classloading` crate — code & test review

Date: 2026-06-10
Reviewer: Fable (Opus 4.8)
Scope: `classloading/src` (18 files, ~35k LOC) + `classloading/tests` (6 files, ~2.4k LOC)
Method: static review only (no cargo build/test/clippy run).

## Summary

The `classloading` crate is, overall, **high quality and defensively written**. The
untrusted-input surfaces (JAR/ZIP parsing, ASN.1/DER, X.509, PKCS#7 signer blocks,
classfile bytecode verification, module descriptors) are consistently bounded with
`checked_add`, depth limits, length caps, and explicit `Result` error paths. The hand-rolled
DER TLV reader (`jar_signer.rs`), the RSA PKCS#1 v1.5 verifier (full-EM reconstruction +
constant-time compare — no Bleichenbacher gap), and the path-traversal filters are all
correct and well-tested. There are **no reachable `unwrap`/`panic`/`unimplemented` outside
test code** in the runtime paths, and only two `unsafe` blocks, both sound (mmap read-only,
GC-remap `from_raw`).

The most material findings are **not memory-safety bugs** but **security-completeness gaps**
in the JAR signing trust model and a **decompression-bomb DoS** that the existing "zip-bomb"
defense does not actually cover (it caps pre-allocation, not the streaming read). There is
also a notable **per-class re-verification performance cost** on signed JARs.

### Headline items
- VULN (high): signed-JAR verification never checks per-entry manifest digests, so
  `getCodeSource().getCertificates()` can be attached to tampered class bytes.
- VULN (medium): zip-bomb / decompression-bomb DoS — `read_to_end` on JAR entries is
  unbounded; `safe_with_capacity` only clamps the initial `Vec` capacity.
- PERF (medium): full PKCS#7 + chain verification runs once *per class* on signed JARs, no caching.
- BUG (low): exception-handler `handler_pc` is not bounds-checked in the type-inference verifier.
- BUG (low): protected-member access check omits the JVMS 5.4.4 object-type constraint.

---

## Bugs

### B1 (low) — Exception `handler_pc` not bounds-checked in type-inference verifier
`classloading/src/verifier.rs:597` uses `entry.handler_pc as usize` directly as a target PC
for `merge_frame_into` and pushes it on the worklist, without the bounds check that branch
targets (`:557`) and fall-through (`:547`) receive. It does not panic — the worklist loop
guards with `if pc >= bytecode.len() { continue; }` (`:506`) — but an out-of-range
`handler_pc` is silently accepted instead of being rejected as a `VerifyError` per JVMS
§4.10.1 (the exception handler target must point at a valid instruction). Completeness gap,
not a safety bug.

### B2 (low) — Protected-member access misses the object-type constraint
`classloading/src/access_control.rs:74-87` (field) and `:131-144` (method) implement
protected access as "same runtime package OR accessor is a subclass of declaring". JVMS
§5.4.4 adds a further constraint for *instance* members: the static type of the object being
accessed must be the accessor class or a subclass thereof. That constraint is absent. This is
a relaxation (permits some accesses HotSpot would reject); it is checked at a different layer
in many JVMs, but if this module is the authoritative gate it is incomplete.

### B3 (low) — Stale "not implemented" doc comments for ECDSA/DSA
`jar_signer.rs:264-270`, `:1694`, `:2411` state that ECDSA/DSA signature/chain verification is
"not implemented" and surfaces `TrustError::NotImplemented`. In the current code ECDSA
(P-256/P-384) and DSA *are* fully verified via the RustCrypto `p256`/`p384`/`dsa` crates
(`verify_signature_with_spki`, `:1540`), and `link_signature_ok` (`:2429`) routes cert-to-cert
links through the same path. `NotImplemented` now only fires for genuinely unsupported
curves (P-521, Brainpool). The comments are misleading for a soon-to-be-public repo.

### B4 (low) — Proxy class-file header counts use unchecked `as u16` truncation
`proxy_gen.rs:235/245/256` emit interface/field/method counts via `len() as u16`. A proxy spec
with >65535 methods or interfaces would silently truncate the count and emit a corrupt
classfile. In practice the constant-pool builder's `checked_add().expect("...overflow")`
(`:467`) would panic first, and `emit_proxy_classfile` returns a `Result` — so the failure mode
is a panic rather than corruption, but neither is graceful. Proxy specs are app-controlled,
so severity is low.

### B5 (low, cosmetic) — UTF-8 mojibake in `access_control.rs` comments
`access_control.rs` contains 22 occurrences of double-encoded UTF-8 in doc comments (e.g.
`в†’` for `→`, `В§` for `§`, `вЂ”` for `—`). Localized to this one file. Cosmetic, but worth
cleaning before open-sourcing under Apache-2.0.

---

## Vulnerabilities

### V1 (high) — Signed-JAR trust is incomplete: no per-entry digest validation
`class_path.rs:1802 extract_jar_signer_blocks` verifies that each `*.SF` is correctly signed
by a leaf that chains to a trust anchor (`jar_signer::verify_signer_block`), and only then
returns the leaf certificate DER as the CodeSource certificates. **However it never verifies
that (a) the `.SF`'s `*-Digest-Manifest` matches the actual `MANIFEST.MF`, nor (b) that each
`MANIFEST.MF` per-entry `*-Digest` matches the actual class/resource bytes that
`find_class` returns.** The standard JAR signing chain is
signature→`.SF`→manifest→per-file-digest; this code validates only the first two links.

Consequence: an attacker can take a properly signed JAR, replace a `.class` body (leaving
`MANIFEST.MF`/`.SF`/`.RSA` intact), and `Class.getCodeSource().getCertificates()` will still
report the original signer's certificate as trusted. Any policy/`signedBy`/code-trust decision
keyed on those certificates is defeated. HotSpot's jarsigner rejects such a JAR (digest
mismatch). The module's own doc (`:1779-1794`) acknowledges it returns "only the real cert DER
on successful integrity check" but the integrity check covers the signature, not the content
binding. Recommend wiring manifest-digest and per-entry-digest validation, or downgrading the
returned certificates to "unsigned" until that lands.

### V2 (medium) — Decompression-bomb DoS: entry reads are unbounded
Every JAR/JMOD entry is read with `read_to_end` and the only "zip-bomb" mitigation is
`safe_with_capacity(entry.size())` (`class_path.rs:573`), which **clamps the initial
`Vec::with_capacity` to 512 MiB but does not bound the read**. The `zip` v2 reader streams the
deflate output up to the entry's *declared* uncompressed size, which is attacker-controlled;
a highly compressible payload (≈1000:1) lets a ~1 MB compressed entry decompress to hundreds
of MB or GB, and `read_to_end` grows the buffer to that full size regardless of the 512 MiB
constant. Affected sites: `:1053` (manifest), `:1144/:1157` (fat-jar classes), `:1190`
(nested jar), `:1394`, `:1844/:1872` (signer blocks), `:2692` (`find_in_archive`), `:2781`
(JMOD). The fat-jar path (`extract_fat_jar_entries`) reads **every** matching entry into an
in-memory `HashMap`, amplifying the impact across many entries.

The existing test `malformed_jar_zip_bomb_declared_size_is_capped`
(`tests/wp_security_robustness.rs:336`) only asserts the *declared-size constant* and uses a
≤1 MiB payload — it does not exercise an actual decompression bomb, giving false confidence.
Fix: read via `entry.take(MAX_UNCOMPRESSED_ENTRY_BYTES + 1)` and reject/skip entries that hit
the limit; add a per-archive aggregate cap for the fat-jar/JMOD extraction loops.

### V3 (low) — RSA modpow runs with attacker-controlled public exponent
`jar_signer.rs:1507` computes `s.modpow(&key.e, &key.n)` where `e` is the public exponent
parsed from an embedded certificate (`parse_spki`, `:1344`). A malicious cert can declare a
very large `e` (bounded only by `MAX_CERT_DER`), and the custom bitwise `modpow` (`:1250`) is
O(bits(e) × bits(n)²). This is a CPU-time DoS lever during chain verification, distinct from
V2. Real RSA verification exponents are tiny (3/65537); recommend rejecting `e` with
`bit_length()` above a small bound (e.g. > 64 bits) before `modpow`. Low because it requires a
crafted cert and the cost is polynomial, not exponential.

### V4 (low) — `cert_dates_ok` treats unparseable validity dates as valid
`jar_signer.rs:2516` returns `true` (valid) when `notBefore`/`notAfter` fail to parse
(`parse_asn1_time` → `None`). This is documented as intentional (the crypto link is the real
gate), but it means a cert with a malformed/non-`Z` time encoding bypasses expiry checking
entirely. Combined with V1 this widens the trusted window. Low; the chain signature still must
verify.

---

## Stubs and Unimplemented

The crate has **no `unimplemented!`/`todo!`/`NotImplemented`-return stubs in runtime paths**.
The items below are the documented fallbacks and intentional gaps.

- **Synthetic stub classes** (`class_manager.rs:4269 create_synthetic_stub`,
  `:1479 synthetic_stub_ctor_methods`, `synthetic_stub_fields`). In-memory placeholder classes
  for JDK types that have no `.class` file on the classpath. This is the documented "no real
  classfile" fallback (`docs/jvm-no-synthetic-stubs.md`), and the code actively **upgrades**
  stubs to real classes when bytecode later appears (`:1410`, `:1965-1980`). Not a forbidden
  "fake main" behavioral shim, but it is tech debt: the interface-vs-class guess relies on a
  brittle heuristic (`name.contains("$")` / `name.ends_with("able")` at `:4352`) plus two
  hardcoded allow-lists (`is_concrete_dollar_class` `:4316`, `is_known_jdk_interface` `:4330`)
  that must be hand-maintained.
- **JAR per-entry digest verification** — absent (see V1). Doc at `class_path.rs:1796`
  (`# TODO(post-orchestrator)`) tracks the signature-over-attributes work, which has since
  landed; the manifest/entry digest binding remains unimplemented.
- **`X509Cert::link_signature_ok` for unsupported curves** returns
  `TrustError::NotImplemented` (`jar_signer.rs:2437`) — correct fail-closed behavior for
  P-521/Brainpool, not a stub of supported functionality.
- **Parallel classfile parse** deliberately not implemented (`resolution.rs:9-29`), documented
  with rationale (rayon dependency surface vs. one-time startup win).

---

## Performance

### P1 (medium) — Signer blocks re-verified per class on signed JARs
`class_path.rs:1741/:1758` call `extract_jar_signer_blocks` inside
`find_class_code_source_info`, which runs for **every** class lookup in a signed JAR and
re-scans the whole central directory, re-reads every `*.RSA`/`*.SF`, re-parses PKCS#7, and
re-runs RSA/ECDSA/DSA verification + trust-chain walk. `class_manager.rs:3182
find_class_code_source` adds no cache either. The CodeSource certificates are identical for
all classes in one archive — compute once per `JarFile`/`NestedJar` entry and memoize (e.g. an
`OnceLock<Vec<Vec<u8>>>` on the entry).

### P2 (low) — `update_condy_refs`/`scan_condy_roots` iterate the whole condy map each GC
`resolution.rs:420/:429` linearly scan every cached `CONSTANT_Dynamic` value on each GC
pass/remap. Fine while condy caches are small; if they grow, consider tracking only
object-valued entries in a side list.

### P3 (low) — Module transitive-closure fixpoint is O(modules² × edges)
`module.rs:361-393` recomputes reader sets inside the `while changed` loop, rebuilding
`reader_idxs` by scanning all modules per transitive edge per iteration. Module counts are
small (hundreds), so this is acceptable today; a worklist-based closure would scale better if
large layer graphs appear.

### P4 (low) — `canonicalize_cached` capacity vs. cap
The canonicalize cache (`class_path.rs:169`, cap `CANONICALIZE_CACHE_CAP = 1024`, `:137`)
guards growth well; no action needed, noted as already-handled.

---

## Tests

Inventory: ~480 inline `#[test]` functions across 16 source files, plus 6 integration test
files (`wp1_7_annotation_lookup`, `wp2_3_define_class_backend`, `wp2_4b_redefine`,
`wp2_7_annotation_proxy`, `wp2_10_nest_host`, `wp_security_robustness`).

Well-covered areas (read directly):
- **jar_signer.rs** — 38 tests: self-consistent SHA-1/256 signer blocks, tamper rejection,
  truncated/garbage/oversized/empty inputs, wrong-OID, missing signer-info, real RSA
  end-to-end, ECDSA P-256/P-384 round-trips, DSA round-trip, RFC 5280 extension checks
  (keyUsage/EKU/basicConstraints/unknown-critical), JKS/PKCS12 trust-store load, expired-cert
  rejection. Strong.
- **class_path.rs** — 81 tests: path-traversal (`..`, absolute, backslash, NUL, drive letter,
  `./`), fat-jar/WAR/Spring-Boot structures, multi-release version selection, JMOD/jimage
  round-trips, wildcard expansion, manifest parsing/continuation/caps, truncated-bytes
  no-panic.
- **access_control.rs** (32), **bytecode_verifier.rs** (66), **vtype.rs** (60),
  **verify_frame.rs** (18), **module.rs** (34) — solid lattice/access/verification coverage,
  including the nest-host spoofing regression.

Gaps (most important missing tests):
1. **Decompression-bomb actual memory bound** — existing zip-bomb test only checks the
   declared-size constant with a ≤1 MiB payload (V2). No test feeds a real high-ratio deflate
   payload and asserts the read is bounded.
2. **JAR content-tamper detection** — no test plants a validly-signed JAR, swaps a class body,
   and asserts the certificates are dropped (V1).
3. **Out-of-range exception `handler_pc`** — no verifier test for B1.
4. **Protected instance-member object-type constraint** — no test for B2.
5. **Oversized RSA public exponent / chain-verify CPU bound** — no test for V3.
6. **`find_class_code_source` caching / repeat-lookup** behavior — no test pins the
   per-archive verification cost (would also lock in a P1 fix).

Estimated coverage: **~70%**. Basis: the security-critical crypto, JAR parsing, access
control, and bytecode verification have broad, meaningful tests (read directly); the gaps are
concentrated in adversarial-resource-exhaustion and the content-binding half of JAR signing,
plus a few verifier edge cases. Does **not** plausibly reach 85% for the *risk-weighted*
surface, because the two highest-impact issues (V1, V2) have no negative tests and the
synthetic-stub heuristic branches are only indirectly exercised.

---

## Feature Suggestions

1. **Complete JAR signature verification** — validate `.SF` digest-of-manifest and per-entry
   `*-Digest` against actual bytes before returning signer certificates (closes V1); makes
   `getCodeSource().getCertificates()` trustworthy.
2. **Hard read-size limits** — replace `read_to_end` with `take(MAX+1)` + overflow rejection,
   and add a per-archive aggregate cap for fat-jar/JMOD bulk extraction (closes V2).
3. **Per-archive CodeSource cache** — memoize verified certificates on the classpath entry
   (closes P1) and expose a `verify_jar` summary for diagnostics.
4. **Replace synthetic-stub interface heuristic** with a curated JDK class/interface manifest
   (generated from `java.base` module metadata) so `instanceof`/checkcast correctness no
   longer depends on `$`/`able` name guessing.
5. **Verifier strictness pass** — bounds-check exception `handler_pc`, enforce the protected
   object-type constraint, and reject malformed cert validity encodings (B1/B2/V4) behind a
   `--strict-verify` flag for conformance testing.
6. **Bounded RSA exponent + a "crypto budget"** on chain verification (max exponent bits, max
   chain length already present) to make signer-block processing constant-time-bounded against
   crafted certs (V3).

---

## Files sampled vs fully read

Fully read (or read in full across chunks) — high-risk untrusted-input paths:
- `jar_signer.rs` — DER/TLV reader, PKCS#7 parse, RSA/ECDSA/DSA verify, BigUint modpow,
  X.509 parse, chain/date/extension checks, base64 (the riskiest regions read line-by-line;
  the ~1300 lines of `#[cfg(test)]` fixtures skimmed).
- `class_path.rs` — read_file/mmap, manifest parse, fat-jar/JMOD extraction, find_class /
  find_resource / code-source, multi-release, wildcard, safe-entry/path filters (test module
  skimmed).
- `access_control.rs` — full access logic + nest-host confirmation read; tests skimmed.
- `verifier.rs` — type-inference worklist, branch/handler/merge logic read fully; tests skimmed.

Sampled (structure surveyed via grep + targeted reads of risk areas):
- `class_manager.rs` (8800 LOC) — synthetic-stub creation, redefine generation/locking,
  code-source delegation read; bulk of the class-registration machinery surveyed by signature
  grep.
- `resolution.rs` — module docs, condy GC remap (`unsafe`), cache structure read.
- `module.rs` — transitive-closure loop + `descriptor_from_module_attribute` read; rest
  surveyed.
- `proxy_gen.rs` — header emission + CP builder + panic sites read; rest surveyed.
- `bytecode_verifier.rs`, `verify_insn.rs`, `vtype.rs`, `verify_frame.rs` — grep-surveyed for
  panics/unsafe/indexing (all clean outside tests).
- `annotations.rs`, `loaders.rs`, `builtin_loaders.rs`, `class.rs`, `fx_hash.rs`, `lib.rs` —
  read or grep-surveyed; clean.

Tests: `wp_security_robustness.rs` read directly (zip-bomb/zip-slip/isolation/redefine);
others inventoried by grep.
