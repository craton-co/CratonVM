# Bug U — cached default `java.util.Locale` not a GC root → stale pointer → SIGSEGV

**Severity:** High (hard VM crash). Confirmed on
`org.apache.catalina.util.TestServerInfo` (**FIXED — now PASS, OK (22 tests)**).
**Status on CratonVM:** CRASH (EXCEPTION_ACCESS_VIOLATION) → **FIXED**.
**HotSpot:** PASS. **Run date:** 2026-06-13 (tag `loop2`).

> Note: `TestSwallowAbortedUploads` shows the same "Stale pointer …
> java/util/Locale" *warnings* but its SIGSEGV persists after this fix — it has a
> **separate** crash (file-upload buffer path, the Bug-D conservative-JIT-root /
> young-GC family), tracked separately.
>
> Note (2026-06-13 follow-up): this Locale GC-root fix removed TestServerInfo's
> *deterministic* crash (it now passes ~2 of 3 runs), but a **residual
> probabilistic** SIGSEGV remains (observed ~1 in 3 runs, flaky on GC timing) —
> the same pre-existing Bug-D conservative-JIT-root / young-GC family, unrelated
> to this fix or the later TLS work. Not yet fixed (see BUG-D).

## Symptom

```
WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual
  receiver (ptr=0x26a87460, all-zero header) — falling back to CP class java/util/Locale
...
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF65671A8E5
```

The interpreter's defensive "stale pointer in invokevirtual receiver" guard
fires repeatedly on a `java/util/Locale` receiver (its header is all-zero — the
object was reclaimed/relocated), then the run SIGSEGVs once that freed slot is
reused by another allocation. The memory dump around the fault register shows
class-name bytes (`…catalina/util/TestServerInfo…`), i.e. a reclaimed object's
slot reused for unrelated data.

## Root cause

CratonVM caches synthetic `java.util.Locale` objects in process-global Rust
mutexes — `locale_default` + `locale_data` (`native-builtins/src/lib.rs`) and
`cached_default_locale` + `synthetic_locale_data` (`locale_bootstrap.rs`).
`Locale.getDefault()` returns the cached `ObjectRef`. But unlike the singleton
class loaders (which ARE scanned/remapped — `gc_scan_loader_singleton_roots`),
these Locale caches were **not** GC roots and had **no post-GC remap**. A moving
young collection reclaims/relocates the cached Locale, the cache keeps handing
back the stale `ObjectRef`, and a later `Locale` method dispatch crashes. (The
`gen_heap.rs` code even carries a comment about the "intermittent stale
Locale/ClassLoader" hazard — the ClassLoader half was fixed; Locale was not.)

## Fix

Mirror the class-loader root fix for the Locale caches:

- `native-builtins/src/lib.rs`: `gc_scan_locale_roots` / `gc_update_locale_refs`
  (push/remap `locale_default` + every `locale_data` key; delegate to
  `locale_bootstrap`).
- `native-builtins/src/locale_bootstrap.rs`: same for `cached_default_locale` +
  `synthetic_locale_data`.
- Wire both into `vm/src/memory/roots.rs` (step 18b) and
  `vm/src/memory/gc.rs` (step 18b), next to the class-loader hooks.

The ObjectRef-keyed side-tables are rebuilt with relocated keys so accessor
natives keep resolving the Locale's language/country after a GC.

## Reproduction

```
cratonvm.exe -cp <tomcat-test-cp> org.junit.runner.JUnitCore \
  org.apache.catalina.util.TestServerInfo
# Before: "Stale pointer … java/util/Locale" then EXCEPTION_ACCESS_VIOLATION
# HotSpot: PASS
```
