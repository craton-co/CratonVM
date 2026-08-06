# L7 — Make fabrication refusable, and migrate the callers that fire — RETIRED 2026-08-05

**Was:** `docs/feature-designs/jdk-only-wave2/L7-ensure-synthetic-class-migration.md`
**Owned:** `classloading/src/class_manager.rs` and its fabrication call sites
**Done when (from the brief):** *"Every call site that fires on the measured
workloads is fallible and refuses diagnosably under `JdkOnly`, and the record's
'52' is replaced by the measured count with the workload named."* — met.

Two residuals are handed off, both with evidence, both named at the end.

---

## The measurement, which is the point of the lane

`--jdk-only`, real JDK 25 (`/home/victor/jdk25`), branch off `origin/dev`
`d010d611b4`, `--dump-class-origins` + `--jdk-only-report`, one process per
workload, 2026-08-05.

| workload | rows before → after | `compatibility-stub` before → after |
|---|---|---|
| `StrictBoot` (a `main` that prints one line) | 392 → 379 | **13 → 0** |
| `JdkOnlyCensusLoadProbe` | 704 → 656 | **17 → 0** |
| `JdkOnlyBreadthProbe` | 828 → 709 | **18 → 0** |

`--jdk-only-report`'s `counts.compatibility_classes` agrees: 13 / 17 / 18 →
**0 / 0 / 0**. `counts.synthetic_stub_invocations` is 0 before and after.

Two further probes, added afterwards as a check that the three above were not a
lucky sample:

| workload | `counts.compatibility_classes` before → after | strict stdout |
|---|---|---|
| `L1LoaderIdentityProbe` | 13 → **0** | **byte-identical** |
| `MapLayoutMatrixProbe` | 15 → **1** | 2 of 9 sections now `NoClassDefFoundError: java/util/HashMap$KeyItr` |

`L1LoaderIdentityProbe` is the interesting one: thirteen fabrications removed
and *not one byte* of its output changed, which is what "these were
substitutions nothing depended on" looks like when it is true.

`MapLayoutMatrixProbe`'s last row is `java/util/Enumeration$Impl`, minted by
`native-builtins/src/classloader.rs`'s `enumeration_from_url_strings` /
`enumeration_from_rooted_urls` / `empty_enumeration_impl` behind
`ClassLoader.getResources`. It fires on none of the three workloads the brief
names. Left alone deliberately: `classloader.rs` is L1's owned file, and unlike
the families below this one has no drop-in real-bytecode fallback — the VM owns
the classpath, so `getResources` is a genuine `Bridge` and it is only its
*return shape* that is a substitution. Named here so the next lane starts from
a location rather than a grep.

The brief also asked for H2 or Spring Boot. **Not measured:** neither corpus is
provisioned on this build host (`apps/h2database/h2` is an empty directory, no
`craton-testcp.txt`), and standing one up is L8's job, not a side-quest inside
this lane. The five workloads above are what this lane's numbers rest on.

The row total falls, and that is intended: a refused fabrication is a class
that never enters the store. The brief's *"total unchanged unless you intend
otherwise"* describes a **reclassification**; this is a **refusal**, so the
"otherwise" is the intended case. On the strict boot the drop is exactly the
refusal count — 392 → 379, thirteen classes, thirteen refusals. On the two
probes it is larger (48 and 119) because a refused stand-in also stops dragging
in the interfaces and supertypes `fabricate_class` would have loaded to wire it
up, and because the retagged factories hand back real `java.base` classes the
census counts under `boot-image` instead; those rows moved, they did not
vanish.

### The 10 call sites that fire — not 52

| site | class(es) | workloads |
|---|---|---|
| `vm/src/vm/vm_init.rs:1052` | `java/util/Enumeration$Impl` | all three |
| `vm/src/vm/vm_init.rs:1110` | `java/util/Comparator$Native` | all three |
| `vm/src/vm/vm_init.rs:1227` | 11 × `cratonvm/internal/Unmodifiable*` | all three |
| `native-collections/src/lib.rs:4844` | `cratonvm/internal/ArrayListSubList` | censusload, breadth |
| `native-collections/src/lib.rs:12143` | `java/util/HashMap$KeyItr` | censusload, breadth |
| `native-collections/src/lib.rs:15904` | `cratonvm/internal/StreamCollector` | censusload |
| `native-collections/src/lib.rs:39187` | `java/util/TreeSet$Itr` | censusload |
| `native-builtins/src/lib.rs:25891` | `cratonvm/internal/SystemLogger` | breadth |
| `native-builtins/src/lib.rs:37099` | `java/util/function/Function$Identity` | breadth |
| `native-builtins/src/phases_late/streams.rs:2730` | `java/util/function/Function$AndThen` | breadth |

A separate recount of the *population* (not the firing set) with `#[cfg(test)]`
blocks excluded properly: **39 live sites in 25 files**, not 52 in 27. The
difference is `vm/src/vm.rs`'s six (behind `#[cfg(all(test, feature =
"synthetic-jdk"))]`, which a plain `#[cfg(test)]` scan misses) and
`proxy_gen.rs`'s four, all four of which are in its test module — the record's
*"`proxy_gen.rs` has 5 sites … each needs its own adjudication"* was counting
tests.

### The instrument had to be fixed before the census meant anything

All seven native-minted classes were attributed to **one line** —
`vm_exec.rs:13670`, `NativeContextImpl::ensure_synthetic_class`, which every
native in the workspace forwards through. `#[track_caller]` was threaded down
the chain: the two `NativeContext` trait declarations, their two
`NativeContextImpl` impls, and the three allocation funnels
(`native-collections::alloc_synthetic`, `native-io::alloc_synthetic`,
`native-builtins::alloc_concurrent_synthetic`, the last with ~2,000 call
sites). Control run: totals and stub counts byte-identical before and after
that change (392/704/828 and 13/17/18) — only `requested_by` moved.

**Caveat found while doing this, worth keeping:** `origin_requesters` records
the *first* requester of a class name, and a refusal records one. So once a
site refuses a name, a later *successful* fabrication of the same name by a
different site is still attributed to the refusing one. All five residual rows
below are reported as `vm_init.rs:484` (this lane's new refusal helper) but are
actually minted in `native-collections`.

## What changed

### 1. The three bootstrap sites (`vm_init.rs`)

New `ensure_bootstrap_compat_class` helper calls `try_ensure_synthetic_class`
and returns `Option<ClassId>`; each of the three call sites skips its wiring on
`None`. A refusal is diagnosable twice over: the
`CompatibilityClassRequested` violation (class, reason, Rust site) that
`--jdk-only-report` and `--trace-jdk-only` both show, plus a `tracing::warn!`
that the CLI's default WARN/stderr filter prints with no extra flag and that
states the *consequence*, not just the fact.

The boot deliberately continues, and does: `StrictBoot` still prints `BOOT-OK`
and exits 0. This is the area the brief flagged as having produced
`InternalError: null property: java.home`; it did not recur.

The eleven `cratonvm/internal/Unmodifiable*` stay `CompatibilityStub` — the
2026-08-04 adjudication is acted on, not reversed. They are refused, not
relabelled.

### 2. The native sites

`try_alloc_synthetic` (native-collections) and `try_alloc_concurrent_synthetic`
(native-builtins) are the fallible siblings of the two allocation funnels; the
five measured native sites use them, and `craton_alloc_system_logger` became
fallible outright.

**A refusal has to be catchable.** `impl From<ClassIdentityError> for
MethodCallFailed` yields `InternalError`, which the exception model defines as
*"not catchable by Java code — aborts execution entirely"*. That is the right
shape for a VM invariant and the wrong one for a policy refusal: contract §5
asks for *"the specification-appropriate `ClassNotFoundException` /
`NoClassDefFoundError`"*. New `native_api::refusal_to_java_failure` builds the
throwable — `NoClassDefFoundError` for a policy refusal (message = the internal
name, same shape as the VM-side `raise_no_class_def_found`, so a refusal
reaching Java from a native and one from constant-pool resolution are
indistinguishable to a `catch` block), `IncompatibleClassChangeError` for an
ambiguous name — and falls back to the uncatchable form only if the throwable
itself cannot be constructed.

### 3. `Proxy$Instance` (brief item 3) — answered, deliberately not done

The brief said *"two of which gate native-vs-bytecode dispatch. Find those two
first; they decide whether this is a one-line flip or a dispatch change."*

Found, and they decide the opposite of what was expected: **neither can observe
`Proxy$Instance`.** Both predicates test `real_protected_stub_class(name)`
before reading `is_synthetic_stub`, and that allow-list is eleven literal names
plus the `CRATONVM_REAL` selector (unset by default). It is still a dispatch
change, through three read sites the record never named. Full write-up, with
why the obvious `!origin.has_real_bytes()` separation does not work either, is
in [the `vm-internal-classes-mislabelled-compatibility-stub` record][r2]. It is
fabricated on **none** of the three measured workloads, so by this lane's own
"migrate what fires" rule it is out of scope; the record keeps it.

### 4. `Function$Identity` (brief item 4) — fixed upstream, as the brief asked

The brief: *"the two defects cancel … Fixing either one alone exposes the
other. The real fix is upstream — `Function.identity()` should return the real
lambda."*

Both halves landed together. `Function.identity` / `UnaryOperator.identity` /
`Function$Identity.{apply,andThen,compose}` and streams.rs's
`Function.{compose,andThen,identity}` are retagged `SyntheticStub`, so
`register()` refuses them under `JdkOnly` (recording a
`SyntheticNativeRegistered` violation naming the registration site) and the
real `java.base` bytecode runs; and `native_function_identity` moved to the
fallible allocator, so if the retag is ever reverted the fabrication fails
loudly instead of cancelling out again.

**The 2026-07-14 revert is not re-litigated.** That bisect was about
`CRATONVM_NO_STUBS` / `set_drop_synthetic_stubs(true)`, an operator switch that
is opt-in and still off by default; `Compatible` and `--real-jdk` keep
`SyntheticStub` registrations, so neither changes. `Bridge` was wrong on its
own terms — it asserts "no working real-bytecode fallback exists", and
`java.base`'s `Function.identity()` is one line of invokedynamic returning
`t -> t`.

Evidence: the breadth probe's `lambdas` line under `--jdk-only` is now
byte-identical to HotSpot 25 —

```
lambdas sq=36 add=7 sup=0 p=true u=[z] sorted=[a, bb, ccc] comp=5 id=i
```

— `id=i` being `Function.identity().apply("i")` through the real lambda.

## Verification

**Compatible mode is unchanged.** Same binary, `--real-jdk`, all three
workloads, stdout diffed against the pre-fix binary: byte-identical on
`StrictBoot` and `JdkOnlyBreadthProbe`; the single line that differs on
`JdkOnlyCensusLoadProbe` is the ephemeral socket port the probe prints
(`net port=33389` vs `39663`), which differs run to run on the same binary.

**HotSpot control**, JDK 25, same classpath: `CENSUSLOAD sections=9 failed=0`,
`PROBE2 sections=15 failed=0`.

**Strict boot still boots**, exit 0.

**The dispatch check the sibling record asks for** is answered by construction
for this change rather than by a run: no class origin was flipped, and the one
input that did change — the `NativeKind` of eight `java/util/function/*`
triples — is consumed by `synthetic_stub_kind_should_yield_to_real_bytecode`
only *before* the `real_protected_stub_class` gate, which is `false` for
`java/util/function/Function` in both modes. The verdict is `false` before and
`false` after.

**Gates**, all `--release` on the Azure Linux host:

| gate | result |
|---|---|
| `native-builtins --test stub_ratchet` | 4 passed — after re-freezing the baseline, see below |
| `native-api` | 315 passed across 7 binaries, 0 failed |
| `classloading --test jdk_only_class_origin` | 12 passed |
| `native-collections` | 202 passed across 13 binaries, 0 failed |
| `native-io` | 380 passed, 0 failed |
| `vm --test jdk_only_dispatch` | 12 passed, 0 failed |
| `vm --lib` | 2403 passed, 0 failed |
| `native-builtins --lib` | 3275 passed, 0 failed (after the dev merge — see below) |
| `vm --lib --features synthetic-jdk` | 3914 passed, 5 failed — **all five identical on pristine `origin/dev`**, see below |
| `regression-suite/bridge-ratchet.sh` | PASS, selftest 14/14; baseline re-frozen, see below |

**The synthetic-jdk `vm` gate is red on `origin/dev` itself**, which contradicts
the standing note that it is blocking at zero. Measured on `f80fa34b6` in a
clean worktree, same command: `inet_socket_address_basics`,
`linked_hashmap_put_get`, `linked_hashmap_put_if_absent`,
`linkedhashmap_first_last_entry_p64`, `scanner_next_line_p51` — five failures,
byte-identical assertions to this branch's (`linked_hashmap_put_get` returns
`Object(…)` where the test wants `Int(42)`, on both). This branch adds none of
them.

A sixth, `jit::helpers::tests::jit_getfield_never_tears_against_concurrent_jit_putfield_int`,
appeared 3 times in 55 runs here and 0 in 30 on pristine — but it is a
scheduling flake, not a signal. The test spawns two raw threads hammering one
heap slot through `jit_putfield_int` / `jit_getfield`; it performs no native
dispatch, loads no class, and fabricates nothing, so nothing in this change can
reach it. Its final assertion is `seen_a > 0 && seen_b > 0` — *"test may not be
exercising real contention"* — which is a statement about thread scheduling,
and every observed failure was during the window when the build host was at
load 150–400 with 2,000+ sessions. 25 consecutive passes once the box quieted.

`native-builtins --lib` failed once *before* the dev merge, on
`net_phase_e::re3_get_by_address_uses_hotspot_ipv6_text_and_concrete_layout`,
and it was **not this change**: `origin/dev` moved past this branch's fork
point (`d010d611b4`) and `742fee6fa fix(net): the getByAddress unit test
asserted the contract the JDK does not have` fixed exactly that test. Worth
recording how that was nearly misdiagnosed: the first A/B ran the test
*filtered* on a pristine tree and *unfiltered* here, which is not an A/B at all
— and the pristine tree had silently been created from the *newer*
`origin/dev`, so it would have exonerated the branch for the wrong reason. The
verdict only became sound after running the full suite on both trees and
diffing the two commits' `net_phase_e.rs`.

**`stub_ratchet`'s baseline moved 157 → 165**, which is a ratchet moving the
wrong way and therefore needs the explanation the test's own failure message
demands. It is in the constant's doc comment: the eight are exactly the
`java/util/function/*` triples retagged above, so this change added no fake —
it re-labelled eight that were already there and that the wrong tag was hiding
from this gate. The number rising is the gate becoming more honest. The
direction to be suspicious of is a `SyntheticStub` quietly becoming a `Bridge`,
which lowers it while changing nothing.

**One `Compatible`-mode delta that is not behavioural but is real**, and should
not be discovered later by someone else: `jit/helpers.rs`'s
`resolve_native_site` refuses to install a JIT native fast-path site when the
callee's `NativeKind` is `SyntheticStub`, in *every* mode. The eight retagged
`java/util/function/*` triples therefore lose that dispatch shortcut in
`Compatible` too. Semantics are identical — the call still reaches the same
callback through the ordinary funnel — which the byte-identical probe output
above demonstrates.

## The cost, stated plainly

Nine probe sections that passed under `--jdk-only` before now fail, each with a
catchable `NoClassDefFoundError` naming the class:

| class | sections |
|---|---|
| `java/util/HashMap$KeyItr` | censusload `collections`, `net`, `concurrent`; breadth `time` |
| `cratonvm/internal/ArrayListSubList` | censusload `text`; breadth `regex` |
| `cratonvm/internal/UnmodifiableList` | breadth `textformat` |
| `cratonvm/internal/StreamCollector` | censusload `interfaces` |
| `cratonvm/internal/SystemLogger` | breadth `serialization` |

This is exposure, not regression: those shapes were being substituted for real
JDK types on every strict run, silently. `counts.synthetic_stub_invocations`
was 0 the whole time, which is exactly why the *class-origin* census — not the
invocation counter — is the instrument §11 gates on.

The count did not grow when R1 landed: censusload stayed at 5 failures and
breadth at 4, while the census went 1 → 0 and 5 → 0. Retagging the factories
first is what bought that, and the next section is the measurement that says
why it was necessary.

**The four remaining ones cannot be fixed the way `Function$Identity` and the
unmodifiable family were**, and the measurement says why. Under `--jdk-only`
the strict report already lists `java/util/HashMap.{put,get,entrySet}`,
`HashSet.size`, `ArrayList.{get,iterator,size}` and 40 more as
`native-shadows-bytecode`: the map's state lives in CratonVM natives, not in
the real `table[]`. Retagging `HashSet.iterator()` so the real bytecode runs
would iterate an empty real table and return a **silently wrong** answer
instead of a loud one. That is the collections reclassification wave
(L10/L11), and it has to come first.

> **Updated 2026-08-05.** The retagging argument above is still right, and
> `HashSet.iterator()` was NOT retagged. But "cannot be fixed" was too strong,
> and the four are fixed without L10/L11: the natives stay, and where the
> policy refuses the fabricated iterator class they hand the snapshot back
> through a real `Arrays$ArrayList`'s own iterator — which reads only the
> `Object[]` it was handed, not a `table[]`. `StreamCollector` went the other
> way and needed no `Consumer` at all (`Spliterators.iterator`). Details and
> measurements in
> `docs/internal/jdk-only-strict-boot-refused-five-classes-FIXED-20260806.md`.
> Worth recording as a process note: this page's premise was derived by reading
> the code, and a probe that measured it instead
> (`probes/StrictIterPrimitivesProbe`) found the actual cause was different —
> one site minting the class through the INFALLIBLE `alloc_synthetic`, which
> made every other site's refusal order-dependent.

The distinction is the whole content of R1: a factory whose product
**delegates** to the collection it was handed (`Collections.unmodifiableMap`,
`List.of`) can be retagged today, because the backing collection's natives
still answer. A factory whose product **reads the backing object's real
fields** (`HashSet.iterator`) cannot, because nothing fills them.

(`native-shadows-bytecode` drops 63→48 and 65→43 on the two probes. That is not
an improvement — sections that die early stop dispatching, and the retagged
factories stop dispatching on purpose. Do not read it as one.)

## R1 — closed, and the order mattered

After the bootstrap migration, `JdkOnlyCensusLoadProbe` still fabricated one
(`cratonvm/internal/UnmodifiableSet`) and `JdkOnlyBreadthProbe` five
(`UnmodifiableList`, `UnmodifiableListItr`, `UnmodifiableMap`,
`UnmodifiableSet`, `java/util/Comparator$Native`), minted by
`native-collections`' `alloc_unmod_wrapper` / `alloc_unmod_list_itr` /
`make_comparator` and by `native-builtins::lang_system::wrap_system_env_map` —
*not* by the site the census named (see the attribution caveat above).

**Making those allocators fallible on their own was tried first, and it is
wrong.** It does reach 0 / 0 / 0, and it breaks real JDK `<clinit>`s, because
`java.util.Collections.unmodifiable*` and `List.of` are real methods the JDK's
own bootstrap calls. Observed on that build: `SECTION-FAILED zip:
java.lang.NullPointerException: zone` and `SECTION-FAILED reflection:
NoSuchMethodError: cratonvm.synthetic.AnonymousObject$16.newInstance` — neither
naming a refused class, which is the opposite of a diagnosable refusal. Breadth
went 4 → 8 failures and two of them became unattributable.

**Retag first, then refuse.** `register_factory_natives`,
`register_unmodifiable_natives`, `register_comparator_natives` and the six
`Collections.unmodifiable*` factories are `SyntheticStub`, so `--jdk-only`
drops them and `java.base`'s own bytecode runs; the allocators are fallible
behind that. Result: **0 / 0 / 0**, with the failure count unchanged at 5 and 4
— no cascade, no new unattributable error, `lambdas` still byte-identical to
HotSpot.

Two notes for whoever audits this:

* **L6's bridge ratchet moved the good way and is re-frozen in this change**,
  as its own output demands: `bridge_without_acc_native` 10,069 → **9,705**
  (−364) and `bridge_shadows_bytecode` 4,755 → **4,696** (−59), with
  `total_rows` unchanged at 11,916 — the 364 rows moved from `bridge` to
  `synthetic-stub`, they did not disappear. That is the same 364 registrations
  this change retagged, counted independently by a gate that takes its own
  census from a running VM.
* `stub_ratchet`, by contrast, **does not move at all** — still 165. Its census
  runs `register_essential_natives` only, and `register_collections_natives` is
  installed separately by `vm_init`, so none of the 364 is in its population.
  That is a real blind spot in that gate: the two ratchets disagree by 364 rows
  about how many synthetic stubs the VM registers. Not this lane's to fix, but
  worth knowing before quoting "165" as the backlog.
* `Bridge` was wrong for this family on its own terms — it asserts "no working
  real-bytecode fallback exists" — and the fallback works *because the wrapper
  delegates*. That is the test to apply to the next family, not "does a real
  class with this name exist".

## Residual handed off

**`ensure_synthetic_class` cannot be deleted, and the reason is not the 39
call sites.** Three of them *are* the infallible allocation funnels
(`native-collections::alloc_synthetic`, `native-io::alloc_synthetic`,
`native-builtins::alloc_concurrent_synthetic`), which have roughly 2,300
callers between them, none returning `Result`. The record's step 3 is gated on
making those funnels fallible, not on the call sites. Both records are updated
to say so.

The other open ends are named where they live: the four
`HashMap$KeyItr`/`ArrayListSubList`/`StreamCollector`/`SystemLogger` refusals
above (collections reclassification wave), and `Enumeration$Impl` in
`classloader.rs` (L1's file, and the only one of the set without a drop-in
real-bytecode fallback).

[r2]: ../known-issues/jdk-only/vm-internal-classes-mislabelled-compatibility-stub.md
