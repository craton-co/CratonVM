# `ClassManager::ensure_synthetic_class` is deleted — FIXED 2026-08-10

**Status: CLOSED.** The infallible entry point is gone, and with it the
`NativeSystemAccess` trait method and the `NativeContextImpl` override that
forwarded to it. `.ensure_synthetic_class(` matches **zero** sites in the tree —
not zero non-test sites, zero. So do the three infallible allocation funnels
that reached it.

The original defect, for the record: under `--jdk-only` the API recorded the
`CompatibilityClassRequested` violation and then fabricated the class anyway,
because its signature had no error channel. A strict run reported a violation
while continuing in the exact state contract §5 forbids, and nothing in the
chain could say otherwise.

Filed 2026-07-31 as *`ClassManager::ensure_synthetic_class` can record a
JDK-only violation but cannot refuse one*; instrumented 2026-08-04; call sites
migrated by wave-2 lane L7 on 2026-08-05; two funnels made fallible 2026-08-06;
the third (1,904 sites) landed on the fifth attempt 2026-08-07. This record
closes **step 3**, which was all that remained.

## What step 3 turned out to be

The record's own sizing was right about the shape and wrong about the number.
Not "72 direct callers": **41 production call sites**, plus 52 in tests, plus
three infallible funnels whose last callers had to move first. The compiler
enumerated the set exactly, as it always does; a grep of the call name never
could.

Each surviving caller took one of three shapes, and the shape — not the count —
is the decision:

| shape | what it becomes | examples |
|---|---|---|
| a legitimately-generated VM class (contract §1 item 6) | `ensure_generated_class` with an explicit `ClassOrigin`, reached from natives as the new `NativeContext::ensure_vm_internal_class` | `java/lang/reflect/Proxy$Instance` |
| a stand-in for a class whose real bytes should have been found | `try_ensure_synthetic_class`, refusal propagated through the native's own error path | the four enterprise-shim `alloc_object_for` helpers, `alloc_impl`, the array-element class lookups in `lang_string` / `keystore` / `regex_matcher` |
| a caller with no error channel at all | the refusal absorbed at a site that says so and warns, naming the class | `vm_init::ensure_bootstrap_compat_class`, the new `jni::jni_class_or_refuse`, `cglib_enhancer`'s `-> Option<..>` proxy builder, `process::spawn_exit_waiter`'s `-> bool` |

**`ensure_vm_internal_class` is the piece that made the deletion possible.**
`java/lang/reflect/Proxy$Instance` is the superclass of a generated proxy, which
§1 item 6 permits in every mode — routing it through the compatibility door was
a mislabel, and refusing it under `--jdk-only` would have broken every dynamic
proxy with a failure that reads as "strict mode doesn't work" (the first entry
in the old record's *Blast radius*). The trait method is infallible **because
the specification says no class file must exist for that name**, not by
oversight, and its doc comment says so along with the warning that reaching for
it to silence a refusal makes contract §11's zero-stub census unfalsifiable.

## The one site the previous record said must not move — and what actually moved

`native_ksv_iterator` (the `ConcurrentHashMap` key-set iterator) was left on the
infallible funnel on purpose, and the argument was sound as far as it went:
every other snapshot iterator LANDS on a real `Arrays$ArrayItr` when strict mode
refuses the fabricated shape, but `HotSpot`'s `KeySetView.iterator()` returns a
`KeyIterator` whose `remove()` writes through to the map, `RChmKeySetView`
exercises exactly that, and a fixed-size list's iterator answers
`UnsupportedOperationException: remove`. Landing it would trade a working
capability for a fidelity gain.

**Refusing is not landing.** In the default `Compatible` mode
`try_alloc_synthetic` is byte-for-byte what the infallible spelling did, so
`RChmKeySetView` and every ordinary run are unchanged. Under `--jdk-only` the
fabrication of `java/util/HashMap$KeyItr` — a name no JDK image declares — now
raises a `NoClassDefFoundError` naming that class instead of silently running a
synthetic collection iterator in place of `java.base`'s bytecode. That is what
§11 asks for, and it is the last non-zero row
`counts.compatibility_classes` was reporting.

The capability is recovered, not traded away, when CratonVM's
`ConcurrentHashMap` carries a real `table[]` its own `KeyIterator` can walk —
the collections reclassification wave, not this one.

## Acceptance, 2026-08-10

| check | result |
|---|---|
| grep gate: `.ensure_synthetic_class(` | **0** matches, tests included |
| `cargo check --workspace --all-targets` (Linux) | clean; **0** `unused Result`, **0** dead-code warnings |
| `cargo check --workspace --all-targets` (Windows) | clean — see *Two platforms* below |
| `--jdk-only-report` `counts.compatibility_classes` | **0** (`JdkOnlyCensusLoadProbe`, `JdkOnlyBreadthProbe`; 1,210 violations still recorded — the backlog, not the result) |
| `scripts/jdk-only-strict-probes.sh` | **PASS**, 0 divergent sections observed against 2 baselined; all three probes byte-identical to HotSpot in **both** modes |
| `native-builtins/tests/registry_contracts.rs` | **7 passed, 0 failed** (1 ignored) |
| `native-builtins/tests/stub_ratchet.rs` | **8 passed, 0 failed** — `BASELINE_SYNTHETIC_STUBS` did NOT move |

### Two platforms, because this migration broke Windows the last time

A span-driven sweep edits text but is corrected only by diagnostics, and rustc
emits none for a `#[cfg(windows)]` block on Linux — those arms are rewritten and
never type-checked, which is the worst of both. Attempt 5 was green on the Azure
Linux host and broke the Windows build with 8 stray `?`. This step therefore ran
`cargo check --workspace --all-targets` on **both** hosts before merging, and
`native-io`'s `process.rs` / `nio_*` files — the ones carrying `#[cfg(windows)]`
arms — are among the files it edited.

## What making the last funnel fallible found

Nothing new broke, which is itself the finding: the three funnels had already
been made fallible, so step 3's remaining sites were the ones no funnel covered.
The two that were worth a comment at the site:

* **`vm_init::ensure_system_streams` has no error channel and must produce two
  objects.** On any complete image `load_class_concurrent("java/io/PrintStream")`
  has already put the real class in the store, so the fallible ask resolves to
  it and nothing is fabricated. On a refusal it now hands back zero-slot
  `java/lang/Object` streams rather than an undersized 1-slot layout: the GC's
  field guard rejects every access on the latter, so the failure would have
  surfaced as a stream of dropped writes instead of at whatever first tries to
  USE `System.out`.
* **JNI cannot throw from `ToReflectedMethod` / `ToReflectedField` /
  `NewDirectByteBuffer`.** Its documented failure value is NULL, so
  `jni_class_or_refuse` warns, names the class, and the entry point returns
  null — the caller sees it at its own call site.

## The `Unmodifiable*` family, unchanged

They stay `CompatibilityStub` and the bootstrap site refuses them rather than
relabelling them. Reclassifying them is the dangerous direction: it silences the
violation, keeps fabricating, and makes the zero-stub census green while the
substitution continues.

## What is left, and it is not this record's

`counts.compatibility_classes` is 0 on the three measured workloads and the
violation list is 1,210 — the backlog of *requests*, recorded before they are
refused, deliberately, so the violation list stays a to-do list and the count
stays the result. Driving the backlog down is the stub-removal wave; see
`fixed-bugs/jdk-only-census-one-class-one-platform-FIXED-20260810.md` for what
that list can and cannot be read as.
