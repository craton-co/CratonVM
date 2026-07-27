# SPB.1 (`org/springframework/util/`) — RESOLVED 2026-07-26: ban REMOVED, root cause was a GC-root-scanning gap, not a JIT bug

**Status: FIXED.** The repro-3 heap-corruption residual this doc originally
flagged as "separately worth its own investigation" turned out to be the
actual explanation for the whole SPB.1 ban — not a JIT allocate-then-putfield
miscompile at all, but a GC-root-scanning gap. The ban on
`org/springframework/util/` has been **removed**
(`vm/src/jit/skip_list.rs`).

## Resolution summary

Repro-3 (`docs/known-issues/repros/spb1-classutils/ClassUtilsProbe3.java`) was
isolated further this session. A single `URLClassLoader`-loaded
`ClassUtils.<clinit>` run under concurrent GC pressure reproduced the
corruption ~40-60% of the time — reproduced identically with the JIT fully
disabled (`CRATONVM_DISABLE_JIT=1`), and did **not** reproduce when the same
class was loaded via the system classloader instead (matching repros 1/2's
original clean result). That ruled out both "JIT-only" and "requires 30
rounds of loader churn" — the defect needed only a user-defined `ClassLoader`
plus a concurrent GC.

**Root cause** (`vm/src/memory/roots.rs`, `gc/src/vm_heap.rs`): when
`conditional_loader_metadata` is active (a full/major-mark window for the
Generational collector — the very case a `System.gc()`-hammering background
thread reliably creates), four root-scan sections — static fields, class-lock
objects, CONSTANT_Dynamic roots, and `java.lang.Class` mirrors for
user-defined-loader classes — deferred an object to the `metadata_pin`
side-channel instead of pushing it directly onto the root set. That
side-channel is consulted **only** by the Generational backend's OLD-GEN mark
BFS (`gen_heap.rs::old_gen_gc`), which never scans the young generation. A
value that is still in YOUNG gen (the overwhelmingly common case for a static
field's value immediately after `<clinit>` assigns it — nothing else
references a freshly-`new`'d `HashMap`/`ConcurrentReferenceHashMap` until
`<clinit>` returns) had no path to ever be marked: skipped from the direct
root set, and invisible to the old-gen-only BFS. The GC then reclaimed it and
its memory was reused by the very next allocation `<clinit>` made — which is
exactly the observed symptom: `commonClassCache` (or an internal
`ConcurrentReferenceHashMap` field) read back as a live, valid, but
completely unrelated object (a `ReentrantLock$NonfairSync`, a
`ConcurrentReferenceHashMap$Reference`, an `Assert` instance, or a raw class-name
string's byte content), producing `ClassCastException`,
`NoSuchMethodError`, `ConcurrentModificationException`, or a null read
depending on exactly what filled the freed slot first.

**Fix**: `VmHeap::metadata_pin_deferrable(addr)` (`gc/src/vm_heap.rs`) — only
defer to `metadata_pin` when the object is confirmed already in old gen
(matching the consumer side's own `old_gen.contains` check in
`gen_heap.rs`/`g1.rs`/`zgc.rs`); otherwise root it directly. Applied at all
four `roots.rs` producer sites, plus the identical pattern found and fixed in
`native-builtins/src/phases_late.rs`'s `ClassValue` memoization-cache root
scan (same defect class, same fix, threaded through as a closure parameter to
avoid a new `cratonvm-gc` dependency in `native-builtins`).

G1 and ZGC were never affected: both backends' `metadata_pin` consumers
(`g1.rs`, `zgc.rs`) walk every live region uniformly during the same
full-mark pass that activates `conditional_metadata`, so deferring a
young-resident object is sound for them — only the Generational backend's
old-gen-only BFS has the gap.

## Verification

Azure host (`ClassUtilsProbe3`, 30 rounds of fresh `URLClassLoader` +
`Class.forName(..., true, loader)` + concurrent GC-pressure thread, real
`spring-core-7.0.7.jar`):

- **Ban kept**: 50/50 clean runs post-fix (was failing on round 0 in most runs
  pre-fix).
- **Ban lifted** (`CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/util/`):
  47/47 clean runs post-fix — no distinct JIT-specific symptom appears once
  the underlying GC-root bug is fixed, which is why the ban was removed
  rather than just left liftable.
- Repros 1 and 2 (system classloader, no loader churn): still clean with the
  ban both kept and lifted, as before.
- `cargo test --release -p cratonvm-gc --lib`: 959 passed, 0 failed both
  before and after the ban removal.
- `cargo test --release -p cratonvm-native-builtins --lib`: 3098/3103 passed;
  the 5 failures (`cglib_enhancer`, `lang_string`, `logmanager` x2,
  `regex_matcher`) are confirmed **pre-existing and unrelated** — identical
  failures reproduce on pristine `dev` with this fix stashed out.
- `cargo test --release -p cratonvm-vm --lib`: 2403 passed, 18 failed — all
  18 are in `runtime::lock_order::tests` and are a `--release`-only artifact
  (that module's checks are `cfg!(debug_assertions)`-gated, so its
  `#[should_panic]`-style tests can't panic in a release build); confirmed
  pre-existing and unrelated to this fix.

## Original investigation (2026-07-26, superseded above)

Priority item 2 from `docs/known-issues/jit-skip-list-open-bans-20260725.md`'s
"Recommended next session priority": re-test whether the SPB.1
"allocate-then-putfield" theory (`org/springframework/util/`, specifically
`ClassUtils.registerCommonClasses`'s ~100-`HashMap.put` loop) still holds
post the 2026-07-04 general fix, or whether — like
TOMCAT-DOHEAD-JUNIT-ITERATOR.1 — it might be stale. No fixture app
(`apps/SportMe-master`) is available on this host, so built three
progressively more faithful standalone repros against a real
`spring-core-7.0.7.jar` (found in `~/.gradle/caches/...`, not in `~/.m2`).

### Repro 1 — minimal (`ClassUtilsProbe.java`)

`Class.forName("org.springframework.util.ClassUtils")` once, then read back
`commonClassCache` via reflection and exercise `ClassUtils.forName` 3.4M
times (200k iterations × 17 primitive/array type names). **Both baseline
(ban in place) and lifted (`CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/util/`)
passed cleanly, 0 errors, 0 mismatches.**

### Repro 2 — HashMap-warmed (`ClassUtilsProbe2.java`)

Same, but runs 500×200 `HashMap.put` calls first to force
`put`/`putVal`/`newNode`/`afterNodeInsertion` JIT-hot *before* triggering
`ClassUtils.<clinit>` — closer to a real Spring Boot boot where those
methods are already hot by the time `ClassUtils` loads (matches the
original ban comment's own crash-frame sequence). **Both configs still
passed cleanly.**

### Repro 3 — GC-pressure + classloader churn (`ClassUtilsProbe3.java`)

Added the other ingredient the ban comment calls out ("esp. across a
GC-triggering call"): a concurrent daemon thread continuously allocating
64KB arrays and calling `System.gc()`, while the main thread re-triggers
`ClassUtils.<clinit>` 30 times via a fresh `URLClassLoader` per round (since
clinit only runs once per defining loader). **Both configs crashed — but
differently, and neither the way the original bug was described:**

- **Baseline (ban in place):** `cratonvm::gc::guard: gen_heap::read_slot:
  corrupt Value cell (out-of-range discriminant)` — 15 corrupted-slot errors
  logged, then `ClassUtils.<clinit>` fails with
  `NullPointerException: Cannot invoke "Class.getName()" because "clazz" is
  null` inside `registerCommonClasses` (line 202) — this **is** the original
  bug's exact crash site/shape, but under the *baseline* config where
  `org/springframework/util/` should be running fully interpreted (the ban
  should prevent any JIT miscompile in this package specifically).
- **Lifted:** `ClassCastException: org.springframework.util.ConcurrentReferenceHashMap$Reference
  cannot be cast to java.util.Map` — reading back `commonClassCache`
  returns an object of the wrong type (a `ConcurrentReferenceHashMap$Reference`,
  used by an unrelated internal cache elsewhere in the same class) instead
  of the `Map` it should be. This looks like classic field-slot corruption
  (right shape, wrong content) but is a different symptom than baseline's
  crash.

**This session's re-investigation confirmed why baseline crashed too**: the
GC-root gap above applies regardless of whether `org/springframework/util/`
itself is JIT-compiled, so the ban never could have prevented it.

## Established pattern across this session's three SPB/CGL/PIC-family tests

| Ban | Test method | Result |
|---|---|---|
| `org/jboss/as/` | Real WildFly boot | Confirmed live JIT-only bug (`ModelTypeValidator.validTypes` NPE) — KEEP |
| `org/h2/` | Real 218-class H2 suite | Confirmed live correctness bug (`Schema not found` on reconnect) — KEEP |
| `org/springframework/util/` | Synthetic standalone repros (3 iterations), then a 4th under GC pressure | Ban REMOVED — the crash was real but not JIT-specific (GC-root bug, fixed) |

## Related

- `docs/internal/jit-ban-sweep-20260725.md` — this session's tracking doc.
- `docs/known-issues/jit-skip-list-open-bans-20260725.md` — the shared
  cross-session coordination doc this priority item came from (updated with
  this resolution).
- `gc/src/vm_heap.rs`'s `metadata_pin_deferrable` doc comment — the full
  technical writeup of the fix.
