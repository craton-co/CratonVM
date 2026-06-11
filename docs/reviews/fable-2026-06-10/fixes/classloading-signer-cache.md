# Fix: memoize verified JAR signer cert chain per ClassPathEntry (classloading P1)

**Finding:** `docs/reviews/fable-2026-06-10/classloading.md` P1 (perf). After the
V1 per-entry-digest fix landed, `ClassPath::find_class_code_source_info` re-ran
the *entire* signed-JAR verification on **every class lookup** for a signed
archive: full central-directory rescan, re-read of every `*.RSA`/`*.SF`, PKCS#7
SignedData parse, RSA/ECDSA/DSA signature verification, trust-chain walk, and
now also a `MANIFEST.MF` re-read plus a re-read-and-re-hash of every signed
entry (`verify_signed_entries`). The `CodeSource` certificates are identical for
all classes in one archive, so this was pure repeated work on the hot
class-load path.

**Root cause:** `extract_jar_signer_blocks(archive)` is a pure function of the
archive but was called unconditionally per lookup; nothing cached its result.

**Exact change (`classloading/src/class_path.rs`):**
- Added a `signer_cache: OnceLock<Vec<Vec<u8>>>` field to both
  `ClassPathEntry::JarFile` and `ClassPathEntry::NestedJar` (the only two
  variants that carry user-visible signer certs).
- The two hot call sites in `find_class_code_source_info` (the `JarFile` arm and
  the `NestedJar` arm) now do
  `signer_cache.get_or_init(|| Self::extract_jar_signer_blocks(archive)).clone()`
  instead of calling `extract_jar_signer_blocks` every time. First signed lookup
  verifies once; every subsequent lookup clones the cached chain.
- Initialized `signer_cache: OnceLock::new()` at the 3 construction sites (two
  `JarFile` pushes, one `NestedJar` push).
- Updated the 4 other exhaustive (`{ … }` without `..`) destructures of these
  variants to ignore the new field (`, ..`); the `..`-bearing matches and the
  `Debug` impls needed no change. `OnceLock` was already imported.

**Security: unchanged.** The full verification — signature, `.SF`→MANIFEST
binding, and per-entry digest match (`verify_signed_entries`) — still runs in
full; it just runs **once** per archive instead of once per class. An empty
result (unsigned JAR, or a JAR that fails verification) is also cached, so
unsigned archives skip the rescan too, and a JAR that fails verification keeps
reporting an empty/null `CodeSource.getCertificates()` exactly as before
(matching HotSpot `jarsigner -verify` semantics). `OnceLock` guarantees the
closure runs at most once even under concurrent class loading; the result is
deterministic, so a benign init race would compute identical bytes.

**Second-layer cache (`class_manager.rs::find_class_code_source`, ~L3182):
evaluated, declined.** That method is a thin wrapper that calls
`find_class_code_source_info` and wraps the result in `CodeSource::new`. It is
invoked when a protection domain / `CodeSource` is needed (≈ per class
definition), not per method call, so it is already amortized. With the
per-entry `signer_cache` in place the residual cost is only cheap
central-directory `by_name` probes + a `Vec` clone. Adding a
`class_name → CodeSource` cache here would require invalidation on
`RedefineClasses` (note the `class_redefine_generation` machinery immediately
below it), introducing a stale-`CodeSource`-after-redefine hazard for marginal
gain. Not worth the correctness risk; left as a deliberate non-change.

**Files touched:** `classloading/src/class_path.rs`.

**Tests added:** none new in this pass (the behavior is identical, only memoized).
The existing `classloading/tests/wp_security_robustness.rs` is black-box (it does
not reach into `ClassPathEntry`), so it continues to exercise the signed-JAR
path through the public API. Verified no regression by re-running it (see below).

**Follow-up & risk:** very low. The only behavioral subtlety is that signer
verification now happens lazily on first `find_class_code_source_info` rather
than possibly-repeatedly; any caller that expected the verification side effects
(there are none — it returns certs, no global state) on every call would be
affected, but none exists.
