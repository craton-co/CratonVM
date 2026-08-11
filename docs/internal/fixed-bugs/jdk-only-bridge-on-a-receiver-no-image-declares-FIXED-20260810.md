# A `Bridge` on a receiver class no JDK declares — 248 rows, and the deletion list that would have removed them

**Status:** FIXED 2026-08-10. Closes the last open item the two L5 bridge-residual
records carried, and corrects the disposition the sibling census record
prescribed for it.

Retired records this supersedes:
`retired/l5-native-io-bridge-residuals-RETIRED-20260810.md`,
`retired/l5bc-awt-builtins-bridge-residuals-RETIRED-20260810.md`.

## The claim being tested

Contract §1.5 defines a `Bridge` as what an `ACC_NATIVE` method binds to. Both
L5 records ended on the same unresolved item, worded almost identically:

> **The `cratonvm/synthetic/Process*` cluster.** Adjudicate `Bridge`-tagged
> registrations whose receiver class is VM-minted, **as a class of defect**, in
> both places it is now known to occur.

That was done for one cluster on 2026-08-06 (the 29 `cratonvm/synthetic/Process*`
rows) and stopped there. This is the same defect in **forty-three** more classes.

## What was found first: the prescribed fix was wrong

The open item the records handed off was a deletion —
`scripts/baselines/jdk-only-dead-everywhere.tsv`, **791 rows**, "dead on every
image the project supports AND unreached: the honest candidates". Applying it
was the plan. Three measurements say it must not be applied to more than
two-thirds of itself.

### 1. The dispatch filter measures the workload, not the registration — 30 rows

`scripts/jdk-only-dead-sweep.py` subtracts every triple its input censuses
dispatched, so the list is "dead" only to the extent that the workloads reached
things. Two broad workloads had been run, and between them they had rescued four
rows: `HashMap$KeyItr` and `TreeSet$Itr`, iterating.

`probes/DeadSweepReachProbe.java` is written against the *list* instead of
against JDK surface — sixteen sections, one per family the list calls dead. It
dispatches **30 of the 791**, and every one is a VM-minted class wearing a JDK
name: `LinkedList$Itr`, `ArrayDeque$Itr`, `CompletedFuture`, `Enumeration$Impl`,
`LogManager$StringEnumeration`, `Function$Compose`, `Consumer$AndThen`, the
three `Predicate$$Lambda$*`, and all three `Atomic*FieldUpdater$RustJvmImpl`.

The sharpest form of it is not the count. **Eight classes were split down the
middle by nothing but which methods a probe happened to call:**

| class | registrations | dispatched | on the deletion list |
|---|---:|---:|---:|
| `AtomicIntegerFieldUpdater$RustJvmImpl` | 12 | 4 | **8** |
| `AtomicLongFieldUpdater$RustJvmImpl` | 12 | 4 | **8** |
| `AtomicReferenceFieldUpdater$RustJvmImpl` | 8 | 3 | **5** |
| `CompletedFuture` | 5 | 4 | **1** |
| `Enumeration$Impl` | 4 | 2 | **2** |
| `ArrayDeque$Itr` | 3 | 2 | **1** |

Deleting the eight `AtomicIntegerFieldUpdater$RustJvmImpl` rows would have
removed `getAndAdd`, `decrementAndGet`, `lazySet` and five siblings from a class
whose `get`, `set`, `compareAndSet` and `incrementAndGet` are dispatched in the
same run, on the same object.

### 2. The sweep had no macOS arm — 13 rows

It censused JDK 21 and 25 on linux and windows, and
`docs/jdk-only-migration.md` puts the supported matrix at exactly those. But
`sun/nio/ch/KQueuePort` (4 rows) and `sun/nio/fs/PollingWatchService` (9) are on
**both** macOS images, and a class missing from the platforms you swept reads
exactly like a class missing from every platform.

Cheap to fix and cheap to check: the adjudication parses class bytes off the
module image and never executes them, so a macOS JDK unpacked on the Linux host
produces a complete census — the same trick the Windows arm already used.

### 3. The fifth image mints every remaining name — the other 14 classes

That left fourteen legacy names that looked genuinely dead: `java/lang/Compiler`
and `java/lang/UNIXProcess` (removed in JDK 9), `java/net/PlainSocketImpl`,
`sun/misc/Cleaner`, `sun/misc/URLClassPath`, `sun/reflect/Reflection`,
`java/rmi/activation/*`, `jdk/internal/misc/SharedSecrets`,
`sun/nio/ch/WindowsFileDispatcherImpl` (on no image, ever).

Under `--jdk-only` and `--real-jdk` every one of them is
`ClassNotFoundException`. Under **`--synthetic-jdk` every one of them loads**,
with `--dump-class-origins` reporting `compatibility-stub` for all fourteen: the
VM mints a stand-in on demand, and these registrations are its only
implementation.

The sibling census record had already written the rule —

> **The rule: a synthetic stub is never a deletion candidate. Gate it, never
> delete it.**

— and then applied it only to rows already carrying `kind: synthetic-stub`. But
the kind is the thing a reclassification wave is *deciding*; gating on it makes
the instrument agree with whatever the tree currently says. The image-side fact
is what the rule actually rests on, and it holds for the whole bucket.

## The disposition: a kind, not a deletion

For a registration whose receiver class no supported image declares, both
branches give the same answer and neither is deletion:

* the VM mints the receiver → the native is that stand-in's implementation, i.e.
  a synthetic stub; or
* nothing can ever produce a receiver → the tag decides nothing and
  `SyntheticStub` is inert.

That disjunction is why this needs no census of what the VM mints — which
matters, because no such census exists and a grep for the mint sites
under-reports badly: the names are passed as a `const`, a local or a loop
variable, and a search over `alloc_synthetic`-family call sites with literal
arguments finds 22 of them.

`native-api/src/no_image_receiver.rs` holds the table, the measurement, and the
predicate; `NativeMethodRegistry::register` applies it before any of its policy
arms, so the re-tag also decides the `--jdk-only` refusal. Rows re-tagged this
way report `kind_stated`, because a measurement adjudicated them.

**Why centrally and not at the sites:** the property is a fact about the class,
measured against six images, and the 248 rows sit at ~165 registration sites —
many of them loops that also register live rows on other classes. A site cannot
know the fact, and 165 restatements of it would drift on the first image bump.
`scripts/jdk-only-kind-map.py` freezes every row's kind, so the change is still
a reviewable per-row diff.

## The half that is blocked, and how the blocked set was found

Re-tagging is half a fix, and the L5 record said so before this table existed:

> a surviving `Bridge` whose receiver class the policy says may not exist, held
> together today only because `ensure_synthetic_class` cannot enforce.
> **Neither half can be fixed alone.**

Exactly right. Strict mode still fabricates some of these classes — it records
the §5 violation and mints anyway — so dropping their natives turns a silent
contract violation into an `UnsatisfiedLinkError`. Five were in that
state and were excluded at first, with the reason in the source:

| receiver | why excluded | how found |
|---|---|---|
| `java/lang/reflect/Proxy$Dispatch` | `proxy_gen` emits `INVOKESTATIC` to it from every `$ProxyN`; the registration *is* the definition | corpus: `RJdkProxy` |
| `java/lang/reflect/Proxy$Instance` | the VM's invented proxy supertype; already `ClassOrigin::VmInternal` | corpus: `RJdkProxy` |
| `java/util/HashMap$KeyItr` | strict mode mints it (`compatibility-stub`) | corpus: `RChmKeySetView` |
| `AtomicIntegerFieldUpdater$RustJvmImpl` | likewise | **no vector — latent** |
| `AtomicLongFieldUpdater$RustJvmImpl` | likewise | **no vector — latent** |
| `AtomicReferenceFieldUpdater$RustJvmImpl` | likewise | **no vector — latent** |

The two proxy entries are reviewed VM services (§11 admits those to strict mode,
and `NativeKind` has no variant that says so, so `Bridge` is the only tag that
survives `allowed_in(JdkOnly)`). The other four are the same defect as the 248,
waiting on wave-2 item 4.

**Worth keeping: the strict corpus found two of the five, and a class-origin
census found the other three.** A green corpus says "nothing I exercise broke";
`cratonvm --jdk-only --dump-class-origins` answers the question that was being
asked — *which of these classes does strict mode still create?* — in one run, for
all of them. Re-run that, not the corpus, when the exclusion list is next
revisited.

## Measured

One binary per arm, same tree, JDK 25.0.4+7 / linux unless stated. The A/B arm
is a binary built from `dev` (`219c5fe34`) **without** this change, on the same
host and against the same images — which is the only thing that separates "this
change broke it" from "this was already red", and it was needed twice below.

* **Census, `--real-jdk`:** rows unchanged at 11,555; **248 kind changes, all
  `bridge` → `synthetic-stub`, across 43 receiver classes**; nothing else moved.
  `bridge` 10,005 → 9,757, `synthetic-stub` 887 → 1,135, `intrinsic` 663
  unchanged. `kind_stated` 901 → 1,149, i.e. **+248 exactly**.
* **`CRATONVM_NO_STUBS=1` + dropped-stub listing, both arms:** boot succeeds and
  the probe completes on both. Drop list 939 → 1,187: **248 entries only in the
  new arm, 0 only in the old.** This is the check the 2026-07-14
  `java.util.Properties` regression would have failed.
* **Compatible regression suite:** 35 passed / 0 failed, **both arms**.
  `DeadSweepReachProbe` under `--real-jdk` is identical between arms apart from
  a temp filename.
* **`--jdk-only` regression corpus (`SUITE=all`):** 52 passed / 6 failed, **both
  arms** — `RReflect`, `RChmKeySetView`, `RJdkHandles`, `RJdkReflect`,
  `RJdkForkJoin`, `RJdkJmx`, all red on `dev` alone. An earlier arm of this work,
  before the five exclusions, was 50/7.
* **L6 `bridge-ratchet.sh`:** `bridge_without_acc_native` **8,977**,
  `class_absent` **872**, `shadows_bytecode` 4,555. Both gates fired on the
  un-refrozen baselines — which is them working, since a
  `Bridge`→`SyntheticStub` mass re-tag makes the counts *fall* and the aggregate
  ratchet cannot see that direction. Re-frozen with the note, and both PASS
  after: `BRIDGE-RATCHET: PASS`, `KIND-MAP: PASS — no registration changed kind.`
* **`stub_ratchet`:** `BASELINE_SYNTHETIC_STUBS` 689 → **923** and
  `STRICT_MIN_TOTAL_REGISTRATIONS` 10,500 → **10,200**, both re-frozen with the
  reasoning in the constants' own histories. 7 passed, 0 failed.
  `strict_registry_has_zero_synthetic_stubs` and
  `strict_registry_drops_only_the_stubs` both still pass. Lowering a *collapse
  detector* is the move it exists to make suspicious, so it is carried by the
  drop-list diff above rather than by the count.
* **Unit tests:** `native-api` 287, `native-collections` 105, `classloading`
  790, `types` 536, `registry_contracts` 8, `doc_citation_paths` 2 — all
  passing. `native-builtins --lib` is 3,380 / 22 and `native-io --lib` 436 / 2,
  and `native-awt` 259 / 1: **every one of those failure sets is byte-identical
  on the `dev`-only arm**, diffed by test name, so none is this change's. The
  first two are `dc55e8057`'s and are filed separately; the `native-awt` one
  (`image::tests::get_rgb_oob`) has been pre-existing since the L5b record.

## What shipped

| | |
|---|---|
| `native-api/src/no_image_receiver.rs` | the table, four sub-tables (listed / stand-in / VM service / blocked), the predicate, five unit tests |
| `native-api/src/registry.rs` | `register` applies it before the policy arms; body split to `register_inner` so the set/restore cannot miss an early return |
| `probes/DeadSweepReachProbe.java` | the workload written against the baseline; the regression test for all of it |
| `scripts/jdk-only-dead-sweep.py` | refuses an image set that omits a platform; routes `class-absent` to the gated section with the reason; `--gated-out` |
| `scripts/jdk-only-no-image-receivers.py` | re-derives the table from censuses and fails on drift; `--selftest` shows both directions |
| `scripts/baselines/jdk-only-dead-everywhere.tsv` | **791 → 287 rows**, all `method-nowhere` |
| `scripts/baselines/jdk-only-gated-never-delete.tsv` | new: 175 rows, with why each is gated |
| `scripts/baselines/jdk-only-bridge-ratchet.json`, `…kind-map-25-linux.tsv` | re-frozen |
| `native-builtins/tests/stub_ratchet.rs` | both constants re-frozen: 689 → 923, floor 10,500 → 10,200 |

## What is left, and where it lives now

* **287 deletion candidates**, all `method-nowhere`: the class *is* a real JDK
  class on some image and the method is nowhere in its hierarchy on any of them.
  The receiver's existence is not in question there, so the argument above does
  not apply and the list stands. Owned by
  `docs/known-issues/jdk-only/census-asks-one-class-on-one-platform.md`.
* **The 8,977-row `Bridge`-without-`ACC_NATIVE` population**, of which 8,010 own
  a slot. Owned by
  `docs/known-issues/jdk-only/bridge-reclassification-wave.md`, filed with this
  change because the two L5 records that used to hold it are retired.
* **Nothing blocked.** The four receivers held back for wave-2 item 4 were
  released the same day, when `ensure_synthetic_class` was deleted; a
  `--jdk-only` run of `RChmKeySetView` now dies on
  `NoClassDefFoundError: java/util/HashMap$KeyItr`, which is §5 enforcing rather
  than recording. `STRICT_STILL_FABRICATES` is an empty table kept for the shape.
  One wrong turn on the way is recorded in that constant's doc: the vector
  reddening was briefly read as this change's doing, and it reddens on
  unmodified `dev` too.
