# `ClassManager::ensure_synthetic_class` can record a JDK-only violation but cannot refuse one

**Status:** OPEN, **and no longer dangerous on any measured workload.** Filed
2026-07-31; instrumented 2026-08-04; **migrated 2026-08-05 by JDK-only wave-2
lane L7**, which took the census, migrated every call site that fires, and
replaced this record's "52 call sites" with a measured 10. What remains is
step 3 — deleting the infallible entry point — and the reason it is still open
is not the call sites. See *What is still open*.

The original defect: under `--jdk-only` this API recorded the violation and
then fabricated the class anyway, so the run reported a violation while
continuing in the exact state the contract forbids. That is now false for every
path the three measured workloads reach.

## The measurement (2026-08-05, real JDK 25, `--jdk-only`)

| workload | rows before → after | `compatibility-stub` before → after |
|---|---|---|
| `StrictBoot` (a `main` that prints one line) | 392 → 379 | **13 → 0** |
| `JdkOnlyCensusLoadProbe` | 704 → 656 | **17 → 0** |
| `JdkOnlyBreadthProbe` | 828 → 709 | **18 → 0** |

`--jdk-only-report`'s `counts.compatibility_classes` agrees. The row total falls
because a refused fabrication is a class that never enters the store, and
because the retagged factories now hand back real `java.base` classes the
census counts under `boot-image` instead.

Two further probes, as a check that these three were not a lucky sample:
`L1LoaderIdentityProbe` 13 → **0** with byte-identical stdout, and
`MapLayoutMatrixProbe` 15 → **1**. That last row is `java/util/Enumeration$Impl`
from `native-builtins/src/classloader.rs`'s `getResources` helpers — see
*What is still open*.

**Ten call sites fire, not 52** — the union over the three workloads:

| site | class(es) |
|---|---|
| `vm/src/vm/vm_init.rs:1052` | `java/util/Enumeration$Impl` |
| `vm/src/vm/vm_init.rs:1110` | `java/util/Comparator$Native` |
| `vm/src/vm/vm_init.rs:1227` | 11 × `cratonvm/internal/Unmodifiable*` |
| `native-collections/src/lib.rs:4844` | `cratonvm/internal/ArrayListSubList` |
| `native-collections/src/lib.rs:12143` | `java/util/HashMap$KeyItr` |
| `native-collections/src/lib.rs:15904` | `cratonvm/internal/StreamCollector` |
| `native-collections/src/lib.rs:39187` | `java/util/TreeSet$Itr` |
| `native-builtins/src/lib.rs:25891` | `cratonvm/internal/SystemLogger` |
| `native-builtins/src/lib.rs:37099` | `java/util/function/Function$Identity` |
| `native-builtins/src/phases_late/streams.rs:2730` | `java/util/function/Function$AndThen` |

All ten are migrated. The retired lane write-up
(`L7-ensure-synthetic-class-migration-RETIRED-20260805`) carries the full
before/after, the `Compatible`-mode diff and the HotSpot control.

### The instrument was wrong until it was fixed

Every one of the seven native-minted classes was attributed to a single line —
`vm_exec.rs:13670`, the `NativeContextImpl::ensure_synthetic_class` forwarder
that all ~2,000 native allocation sites funnel through. `#[track_caller]` is now
threaded through the two `NativeContext` trait declarations, their two impls,
and the three allocation funnels. Control: totals and stub counts
byte-identical before and after that change; only `requested_by` moved.

**Caveat, and it bites:** `origin_requesters` records the *first* requester of a
class name, and a refusal records one. Once a site refuses a name, a later
*successful* fabrication of the same name by a different site is still
attributed to the refusing one. The residual rows below are reported as
`vm_init.rs:484` — the refusal helper — but are minted in `native-collections`.

### The population, recounted

**39 live sites in 25 files**, not 52 in 27. The difference is entirely
mis-scoped test code: `vm/src/vm.rs`'s six sit behind
`#[cfg(all(test, feature = "synthetic-jdk"))]`, which a `#[cfg(test)]` scan
misses, and all four of `proxy_gen.rs`'s are in its test module — so this
record's *"`proxy_gen.rs` has 5 sites in the current tree, not 1; each needs its
own adjudication"* was counting tests.

## What is still open

**Step 3 — deleting `ensure_synthetic_class` — and the blocker is not the call
sites.** Three of the 39 *are* the infallible allocation funnels themselves:

* `native-collections::alloc_synthetic`
* `native-io::alloc_synthetic`
* `native-builtins::alloc_concurrent_synthetic`

Between them they have roughly **2,300 callers**, none of which returns a
`Result`. Deleting `ensure_synthetic_class` means making those three fallible,
which is that many call sites — not the 39, and certainly not "52". Any plan
that sizes this work off a grep of `.ensure_synthetic_class(` is sizing the
wrong thing.

The fallible siblings those funnels need already exist and now have real
callers: `try_alloc_synthetic` (native-collections) and
`try_alloc_concurrent_synthetic` (native-builtins), added by L7 alongside the
infallible ones.

**The unmodifiable/factory/comparator family is closed — and the order was the
whole lesson.** After the bootstrap migration, `JdkOnlyCensusLoadProbe` still
fabricated `cratonvm/internal/UnmodifiableSet` and `JdkOnlyBreadthProbe` four
more, from `native-collections`' `alloc_unmod_wrapper` / `alloc_unmod_list_itr`
/ `make_comparator` and `native-builtins::lang_system::wrap_system_env_map`.

Making those allocators fallible **on their own** reaches 0 / 0 / 0 and breaks
real JDK `<clinit>`s, because `java.util.Collections.unmodifiable*` and
`List.of` are real methods the JDK's own bootstrap calls. Measured on that
build: `SECTION-FAILED zip: NullPointerException: zone` and `SECTION-FAILED
reflection: NoSuchMethodError: cratonvm.synthetic.AnonymousObject$16.newInstance`
— neither naming a refused class, which is the opposite of a diagnosable
refusal — with the breadth probe going 4 → 8 failures.

**Retag first, then refuse.** `register_factory_natives`,
`register_unmodifiable_natives`, `register_comparator_natives` and the six
`Collections.unmodifiable*` factories are now `SyntheticStub`, so `--jdk-only`
drops them and `java.base`'s bytecode runs; the allocators are fallible behind
that. 0 / 0 / 0, with the strict failure count *unchanged* at 5 and 4.

The test that decides whether a family can be retagged is **not** "does a real
class with this name exist" — it is **"does the real product delegate, or does
it read the backing object's own fields?"** A real `Collections$UnmodifiableMap`
delegates every call to the map it was handed, and that map's CratonVM natives
still answer, so it works. A real `HashMap$KeyIterator` reads the real
`table[]`, which CratonVM's `HashMap.put` native never fills, so retagging
`HashSet.iterator()` would return a silently EMPTY iteration instead of a loud
error.

> **Updated 2026-08-05.** That reasoning about *retagging* still holds, and
> `HashSet.iterator()` still must not be retagged. The conclusion drawn from it
> did not hold: the family did not have to stay a refusal. A third option
> existed — keep the native, and when the policy refuses the fabricated
> iterator class, hand back the snapshot through a real `Arrays$ArrayList`'s
> own iterator, which reads only the `Object[]` it was given. All six sections
> are fixed; see
> `docs/internal/jdk-only-strict-boot-refused-five-classes-FIXED-20260806.md`
> (retired from this directory 2026-08-06, once its fifth class — the
> `System.Logger` one, which had taken out every `ObjectInputStream`
> construction — landed on a real `jdk.internal.logger.SimpleConsoleLogger`).
> The rule is
> narrower than "delegates vs reads its own fields": what matters is whether
> SOME real class exists whose fields we can legitimately fill, not whether the
> obvious one can.

## What is wrong (unchanged in shape)

`classloading/src/class_manager.rs`:

```rust
pub fn ensure_synthetic_class(&mut self, name: &str, num_fields: usize) -> ClassId
```

The return type is a bare `ClassId`. There is no error channel, so the function
passes `enforce: false` and fabricates regardless of mode. `enforce` is
documented on `fabricate_class` exactly as the defect describes it: *"`true`
returns the `ClassNotFoundException` the contract asks for, `false` records the
violation and fabricates anyway. Either way the violation is recorded, and
either way `Compatible` mode fabricates."*

Contract §5 requires the opposite: *"Under `JdkOnly`, every path that today
fabricates a class … must instead return the specification-appropriate
`ClassNotFoundException` / `NoClassDefFoundError` and record a
`CompatibilityClassRequested` violation."*

**The `load_class` chain does enforce.** An absent enterprise or JDK class
arriving through ordinary class loading is correctly refused
(`create_synthetic_stub`, routing through `admit_compatibility_class`). It is
this *direct* API — used by VM bootstrap and by natives that want an allocation
shape — that cannot.

`admit_compatibility_class` is called from exactly two places
(`create_synthetic_stub` and `fabricate_class`), *"so there is no third place a
stub can be minted without the policy seeing it."* The problem was never "the
policy can be bypassed"; it was "the policy is seen and then overridden by a
signature".

## The fallible siblings, and what a refusal has to look like

* `try_ensure_synthetic_class(name, n) -> Result<ClassId, VmError>` —
  `ClassNotFoundException` under `JdkOnly`. Byte-for-byte
  `ensure_synthetic_class` under `Compatible`.
* `ensure_generated_class(name, n, origin)` — for arrays, hidden classes,
  lambdas, proxies, reflection accessors and VM-internal shapes (contract §1
  item 6). Never refused, in either mode; `debug_assert`s that the caller did
  not pass a `CompatibilityStub` origin.

**A refusal must be catchable, which needed a third piece.**
`impl From<ClassIdentityError> for MethodCallFailed` yields
`MethodCallFailed::InternalError`, documented as *"not catchable by Java code —
aborts execution entirely"*. That is the right shape for a VM invariant and the
wrong one for a policy refusal. `native_api::refusal_to_java_failure` (added
2026-08-05) builds the throwable instead: `NoClassDefFoundError` for a policy
refusal, message = the internal name, matching the VM-side
`raise_no_class_def_found` so a refusal reaching Java from a native and one from
constant-pool resolution are indistinguishable to a `catch` block;
`IncompatibleClassChangeError` for an ambiguous name. It falls back to the
uncatchable form only when the throwable itself cannot be constructed.

For a caller with no error channel at all — the VM bootstrap block — "refuse
diagnosably" means the recorded violation *plus* a `tracing::warn!` that the
CLI's default WARN/stderr filter prints with no extra flag and that states the
consequence. `vm_init::ensure_bootstrap_compat_class` is the shape to copy.

## The `Unmodifiable*` family is adjudicated as staying `CompatibilityStub`

They are the largest group and the most tempting to reclassify — no class file
exists under `cratonvm/internal/UnmodifiableList`, which is the `VmInternal`
shape. But they stand in for `java.util.Collections$UnmodifiableList` and
friends: the real `Collections.unmodifiableList()` bytecode is not running, and
that is a compatibility substitution whatever the stand-in is named.
Reclassifying them is the dangerous direction in *Blast radius* — it silences
the violation, keeps fabricating, and makes the zero-stub census green while
the substitution continues. L7 acted on that verdict: the bootstrap site
**refuses** them rather than relabelling them. See
[VM-internal classes are mislabelled `CompatibilityStub`](../../internal/jdk-only-wave2-vm-internal-classes-mislabelled-RETIRED-20260806.md)
(RETIRED 2026-08-06).

## What specifically must change

1. ~~Migrate the call sites that fire~~ — done 2026-08-05, all ten.
2. ~~Make `vm_init.rs`'s bootstrap block fail loudly under `JdkOnly`~~ — done;
   `StrictBoot` reaches `main` with **zero** fabricated compatibility classes.
3. ~~Retag the unmodifiable / factory / comparator natives, then migrate their
   allocators~~ — done 2026-08-05; all three measured workloads are at zero.
4. ~~The four shapes that are still refused rather than removed —
   `java/util/HashMap$KeyItr`, `cratonvm/internal/ArrayListSubList`,
   `StreamCollector`, `SystemLogger`~~ — all four now have somewhere for the
   refusal to LAND, which the note above was wrong to think impossible: the
   first three on 2026-08-05 (real `Arrays$ArrayList` iterator,
   `Spliterators.iterator`), `SystemLogger` on 2026-08-06 (a real
   `jdk.internal.logger.SimpleConsoleLogger`, built through its own `<init>`).
   That last one was reached from `ObjectInputFilter$Config.<clinit>`, so
   refusing it had been costing every `ObjectInputStream` construction in the
   VM. See
   `docs/internal/jdk-only-strict-boot-refused-five-classes-FIXED-20260806.md`.
   **Still open here:** `java/util/Enumeration$Impl` in `classloader.rs`'s
   `getResources` helpers, which has no such landing yet. And the refusals are
   landings, not removals — the natives themselves are still registered, which
   is item 5's business.
5. Make the three allocation funnels fallible (~2,300 call sites), then delete
   `ensure_synthetic_class`.

## How to verify a fix

* A `--jdk-only` boot on a complete real JDK image must reach `main` with
  **zero** compatibility-class *fabrications* in the `--jdk-only-report` JSON
  (`counts.compatibility_classes`). **Not** zero violations:
  `admit_compatibility_class` records the request before it refuses it,
  deliberately, so the violation list is the backlog and the count is the
  result. `StrictBoot` is at 0 with 13 recorded requests.
* Grep gate: `.ensure_synthetic_class(` must match zero non-test sites. 39
  today.
* `--dump-class-origins` must show no `compatibility-stub` rows for non-array
  JDK/application/dependency classes (contract §11).
* `Compatible` mode must be byte-for-byte unchanged — the existing regression
  suite plus `native-builtins/tests/stub_ratchet.rs`. That baseline moved
  157 → 165 on 2026-08-05 and the constant's doc comment explains why (a
  relabelling of eight already-existing fakes, not eight new ones); it must not
  move again without the same kind of explanation.
* Count **call sites**, not violations, when checking migration progress:
  `admit_compatibility_class` dedupes by class name (`origin_violations_seen`),
  so a migrated caller that stops fabricating a name some *other* caller also
  requests will not change the violation count. And see the `requested_by`
  first-writer caveat above before trusting an attribution.

## Blast radius if done wrong

* Migrating a **legitimately-generated** class to `try_ensure_synthetic_class`
  makes `--jdk-only` reject proxies, lambdas or array shapes — an immediate,
  loud, but wrong failure that will be misread as "strict mode doesn't work".
* Migrating a **compatibility stub** to `ensure_generated_class` is the
  dangerous direction: it silences the violation, keeps fabricating, and makes
  the zero-stub census report green while the substitution is still happening.
  Contract §11's acceptance criterion becomes unfalsifiable. Because
  `ensure_generated_class` only `debug_assert!`s on a `CompatibilityStub`
  origin, a release build will not catch this at all.
* Migrating an allocator whose class stands in for a **real JDK method the JDK
  itself calls during `<clinit>`** breaks the boot in a way that does not name
  the refused class — R1's measured outcome. Retag the native first.
