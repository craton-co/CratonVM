# VM-internal and generated classes mislabelled `ClassOrigin::CompatibilityStub` — RETIRED 2026-08-06

**Status:** ✅ FIXED and retired. `java/lang/reflect/Proxy$Instance` is
`ClassOrigin::VmInternal`, the dispatch question `is_synthetic_stub` was
conflating with the census question has its own predicate, the fabricated
generated-name families are classified where they arrive, and
**`Class::is_synthetic_stub` is deleted** — `origin` is the single source of
truth (contract §5's "delete it in a later wave", done).

Filed 2026-07-31; `AnonymousObject$N` migrated 2026-08-04; re-scoped 2026-08-05
by wave-2 lane L7; closed 2026-08-06.

Previous location:
`docs/known-issues/jdk-only/vm-internal-classes-mislabelled-compatibility-stub.md`.

## The five steps the record asked for

| # | step | outcome |
|---|---|---|
| 1 | migrate `cratonvm/synthetic/AnonymousObject$N` | done 2026-08-04 (unchanged here) |
| 2 | decide the right origin for `Proxy$Instance` | done: `VmInternal` |
| 3 | split `is_synthetic_stub`'s two meanings, then flip `Proxy$Instance` | **done** |
| 4 | audit `ensure_synthetic_class` callers matching the generated-name families | **done, and made permanent** |
| 5 | delete `is_synthetic_stub` | **done** |

## Step 3 — the split, and why it is provable rather than argued

`is_synthetic_stub` answered two unrelated questions with one bool:

1. *is this a compatibility substitution?* — the census and `--jdk-only` policy
   question. `ClassOrigin` is authoritative for it.
2. *does this class have no class file, so dispatch must look for a native
   registered under its own exact name instead of walking to an inherited
   `java/lang/Object` body?* — the question the three read sites the record
   named were actually asking.

They coincided only because everything the VM fabricated was tagged
`CompatibilityStub`. `Proxy$Instance` needs branch (2) — it has a NATIVE-flagged
`<init>` from `synthetic_stub_ctor_methods` and native registrations in
`reflect_annotations.rs` — and is not (1).

**`Class::dispatch_lacks_class_file()`** answers (2) and only (2):

```rust
match &self.origin {
    ClassOrigin::CompatibilityStub { .. } => true,
    ClassOrigin::VmInternal => self.methods.iter().any(|m| m.is_native()),
    _ => false,
}
```

The record's own rejected spelling, `!origin.has_real_bytes()`, does not work
because `VmInternal` and `VmArray` both answer "no real bytes" — it would move
every `cratonvm/synthetic/AnonymousObject$N` (the allocation shape behind every
`HashMap` node in the VM) and every `AmbiguousName$…` stand-in onto branches
they take the other arm of today. The **method table** is the discriminator that
does not: a fabricated class's only callable surface is the native registered
under its own name, and `synthetic_stub_ctor_methods` declares NATIVE-flagged
entries for exactly the fabricated names that have one. An allocation shape has
an empty method table and no registration; there is nothing to find by exact
name.

That makes the predicate **equal to the old bool for every class in the store**,
before and after the flip, which is exactly the proof obligation the record set.
`dispatch_predicate_matches_the_stub_bit` in
`classloading/tests/jdk_only_class_origin.rs` walks a live store and asserts it,
with `Proxy$Instance` as the single stated exception;
`vm_internal_allocation_shapes_stay_off_the_dispatch_branch` pins the rejected
spelling's failure mode so nobody re-derives it.

The three read sites now ask the new predicate:

* `vm_exec.rs`'s synthetic-stub retarget (`prefer_exact_class_native`);
* `vm_exec.rs`'s `if !class.is_synthetic_stub { → invoke_on_class_shared }` arm;
* `vm_object.rs`'s `validate_native_coverage` all-classes scan.

Cost: one enum discriminant test for a real class. Only fabricated and
VM-internal classes, whose method tables hold 0–12 entries, pay the scan.

## Step 4 — the audit, as code rather than a date

`fabricated_origin_for_name` in `classloading/src/class_manager.rs` is now what
`ensure_synthetic_class` / `try_ensure_synthetic_class` pass to
`fabricate_class`, instead of a hard-coded `CompatibilityStub`:

* `java/lang/reflect/Proxy$Instance` → `VmInternal`;
* `$$Lambda` → `GeneratedLambda`, `$ProxyN` → `GeneratedProxy`,
  `Generated*Accessor*` → `ReflectionAccessor` — the same answer
  `classify_defined_origin` already gives those names when they arrive with real
  bytes. Neither path may report the same class differently depending on whether
  it happened to be fabricated.

`GeneratedProxy` was the record's original (wrong) prescription for
`Proxy$Instance`: that variant carries `interfaces: Arc<[ClassId]>`, which the
shared *supertype* has no value for, and `is_generated_proxy_name` already says
in terms that this name "must NOT be counted as a generated proxy".

L7 measured that none of the three generated-name families fires on a strict
boot, `JdkOnlyCensusLoadProbe` or `JdkOnlyBreadthProbe`; re-measured 2026-08-06,
still none. `fabricated_generated_names_are_not_compatibility_stubs` is the
permanent form of that one-off audit.

## Step 5 — `is_synthetic_stub` deleted

192 occurrences across 25 files: 53 struct-literal initialisers deleted, the
reads rewritten to `origin.is_compatibility_stub()`, and the field removed from
`Class`. `RedefineInvariantSnapshot` used to snapshot the same fact twice (once
as `origin`, once as the derived bool) and assert both; it now snapshots
`origin` alone, which covers it.

There is no longer a mirror that can go stale, so `set_origin`'s reason for
existing narrows to the one that remains: it bumps `class_origin_epoch`, the
invalidation signal for every provenance-derived memo.

## Verification

* **`--dump-class-origins`, A/B on `probes/ProxyProbe.java`** (two dynamic
  proxies over two interface sets, `isProxyClass`/`getInvocationHandler`/
  `getInterfaces`/`instanceof` all asserted), against a real JDK 25 image:

  | | BASE (`origin/dev` a7a04421b) | after |
  |---|---|---|
  | total rows | 420 | **420** |
  | `compatibility-stub` | 14 | **13** |
  | `vm-internal` | 1 | **2** |
  | `Proxy$Instance` | `compatibility-stub` | **`vm-internal`** |

  Exactly one class moved and it is the intended one — this record's own
  acceptance criterion (*"must drop by exactly the number of classes
  reclassified — if it drops by more, something else was swept up"*), met
  verbatim. Under `--jdk-only` the same probe goes from **1 compatibility-stub
  row to 0**.
* **The dispatch check that actually matters.** For `Proxy$Instance` the
  `synthetic_stub_should_yield_to_real_bytecode` verdict is `false` before and
  `false` after **by construction**: both predicates test
  `real_protected_stub_class(name)` before reading the origin, that allow-list
  does not name `Proxy$Instance`, and `CRATONVM_REAL` is unset by default. The
  predicate itself is not touched by this change, so the ~35 production read
  sites the record warned about are not reached — the three that can observe
  this class are the ones migrated, and their answer is pinned by
  `dispatch_predicate_matches_the_stub_bit`.
* **`native-builtins/tests/stub_ratchet.rs`** — this record's change touches
  class origins, not native registrations, so the ratchet must not move for it,
  and it does not. (It moves by 2 in the same branch for the unrelated
  `ThreadPoolExecutor` retirement, which says so in its own entry.)
* `ProxyProbe` output is identical to HotSpot 25 and to the BASE binary in both
  modes. `scripts/jdk-only-strict-probes.sh`: `RESULT: PASS`.
* Full workspace `cargo check --all-targets`, the `jdk_only_class_origin` suite
  (15 tests), and the release `cratonvm-vm --lib` gate in both the default and
  `--features synthetic-jdk` configurations.

## What stays as the record left it

The eleven `cratonvm/internal/Unmodifiable*` classes **stay
`CompatibilityStub`**, as adjudicated 2026-08-04. They stand in for
`java.util.Collections$UnmodifiableList` and friends; the real
`Collections.unmodifiableList()` bytecode is not running, and that is a
compatibility substitution whatever the stand-in is called. Reclassifying them
is the dangerous direction — it silences the violation, keeps fabricating, and
makes the zero-stub census report green while the substitution continues. L7
already acted on that verdict the right way: the call site is fallible, so under
`--jdk-only` the eleven are refused instead of fabricated. See
`docs/known-issues/jdk-only/ensure-synthetic-class-cannot-enforce-only-record.md`.

`Proxy$Instance` also remains a low-stakes class in practice:
`real_proxy_super()` defaults **on**, so generated `$ProxyN` classes extend the
real `java.lang.reflect.Proxy` and the `Proxy$Instance` super is the opt-out
(`CRATONVM_REAL_PROXY_SUPER=0`). It is still *minted* on every proxy path —
`reflect_annotations.rs` ensures it before spec build — which is why it appears
in the census above at all.
