# `NativeKind` is ambient state, not a `register()` argument — and one line can mis-tag a thousand registrations

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31, re-verified
against the re-landed tree the same day. **DANGEROUS: causes silent
misclassification, not a clean failure, and it misclassifies in both
directions.**

## What changed on 2026-08-05 (second pass) — the instrument was narrower than the question

`kind_stated` is now true on **843 rows** (`native-builtins` 637, `native-io`
173, `native-awt` 23, `vm` 10) after the L5 residual pass took the remaining
`unknown`-marked registrars, the mixed sites, and `vm/src/runtime/instrument.rs`.

What that pass actually found is bigger than the count. **Two of the
adjudications L5/L5b made were wrong, and both were wrong for the same reason:
`image_declaring_method` asks about ONE class in ONE image.** Filed in full as
[`census-asks-one-class-on-one-platform.md`](census-asks-one-class-on-one-platform.md),
with the two instruments that close it. In short:

* **Inheritance.** Of the 2,542 rows the census calls *class present, method
  not declared*, **1,939 are inherited** — 1,612 concrete (§1.4 shadows), 308
  abstract (the every-implementor hazard), and **19 `ACC_NATIVE`** (§1.5
  bridges the census did not credit). Only 603 are genuinely dead.
  `sun/nio/ch/SocketDispatcher.close`, which this record's L5 residual called
  "dispatched 3× while resolving to no declared method", is one of them: the
  method is concrete on `sun.nio.ch.UnixDispatcher`. Mystery closed, and it was
  never a mystery — it was a column that could not see a supertype.
* **Platform.** CratonVM adjudicates an image it cannot *run*, so a Windows JDK
  unpacked on the Linux host yields a full census. **59 registrations are a
  genuine `ACC_NATIVE` bridge only on Windows** and 78 only on Linux;
  **1,735 are dead on both**. Every "needs a Windows-image census" in this
  directory is answerable with one command now.

Two concrete corrections to what is written elsewhere: the
`sun/awt/PlatformGraphicsInfo.hasDisplays0` marker was **right** (it is a
bridge, on the platform that has it), and `sun/nio/ch/WindowsFileDispatcherImpl`
exists on **neither** image — the Windows JDK names that class
`FileDispatcherImpl` too, so its 28 rows are dead everywhere.

## What changed on 2026-08-05 — the number is now PINNED, and re-measured

Still open, and still nothing reclassified. What is new is that the number can
no longer rise unnoticed: wave-2 lane L6 shipped a slack-free ratchet on it
([`L6-unadjudicated-bridge-ratchet-DONE-20260805.md`](../../internal/L6-unadjudicated-bridge-ratchet-DONE-20260805.md)).

```sh
JAVA_HOME=<JDK25> sh regression-suite/bridge-ratchet.sh
sh regression-suite/bridge-ratchet.sh --selftest   # hermetic: no VM, no JDK
```

It lives in `regression-suite/` and not in a unit test because the question needs
a real JDK image at measurement time. It boots the VM, takes the schema-3 census
itself (`--explain-jdk-only` — without it the column this record is about is
null), and scores it against `scripts/baselines/jdk-only-bridge-ratchet.json`,
keyed `<jdk-feature>/<os>` because the registrars are platform-conditional. A key
it has no entry for is a **refusal**, not a pass.

**Re-measured on dev `d010d611b4`, JDK 25.0.3, linux — every count in the
2026-08-04 table below is superseded by this one.** The shape is unchanged; the
tree moved (L1, L2, L9 and the `String` residuals landed).

**11,916 registrations**, not 11,909: 687 `Intrinsic`, 10,842 `Bridge`, 387
`SyntheticStub`.

| what the image says about the `Bridge` target | rows | share | (was 08-04) |
|---|---:|---:|---:|
| `ACC_NATIVE` — a genuine bridge, §1.5 | 773 | 7% | 760 |
| concrete bytecode (`has_code`) — a **shadow** | 4,755 | 44% | 4,796 |
| abstract method — intercepts every implementor | 1,321 | 12% | 1,321 |
| class present, method **not declared** | 2,497 | 23% | 2,489 |
| class absent from the image | 1,496 | 14% | 1,478 |
| **no `ACC_NATIVE` target** | **10,069** | **93%** | 10,084 |

**Two numbers are ratcheted, not one.** `10,069` and — separately — the `4,755`
shadowing rows, because that is the subgroup that has already produced a defect
(§7 step 3's decline reaching `UnsatisfiedLinkError` instead of the bytecode; see
*The first thing it found* below) and because the aggregate alone would let a
shadow trade places with an abstract-method intercept invisibly. A third
assertion, `total_rows >= 8_000`, is a **collapse detector, not a measurement**.

**`kind_stated` is no longer false on all rows.** 9 of the 687 `Intrinsic` rows
state their kind (the `java/lang/String` natives L9 migrated), and — as of L5,
L5b and L5c, all landed 2026-08-05 — **690 `Bridge` rows do too**, so
`kind_stated` is true on **699 of 11,916**: `native-io` 87, `native-awt` 21,
`native-builtins` 582. The sentence below, "`register_with_kind` exists and has
zero callers", is out of date by six hundred and ninety-nine.

**L5/L5b/L5c moved `kind_stated` by 690 and moved the 10,069 by nothing, and
that is not a disappointment — it is the two numbers measuring different
things.** The
ratchet above counts `Bridge` rows *with no `ACC_NATIVE` target*. The 87 rows L5
stated are exactly the rows that DO have one; they were never in the 10,069.
Expect every honest `register_with_kind` migration to look like this: it moves
`kind_stated`, and only a *reclassification* can move the ratchet. A migration
that did move the 10,069 would have done so by stating `Bridge` on rows the
image does not back — the codemod this record warns against.

**What L5 found that generalises.** Of the 204 registrations in `native-io`'s
four `JDK-ONLY-CLASSIFY: bridge` registrars, only 87 have an `ACC_NATIVE`
target. Not one of the four could have its `set_category` scope deleted, because
not one is wholly adjudicated — not even `random_access_file.rs`, where 10 of 11
are `ACC_NATIVE` and the eleventh (`close0()V`) is not declared by JDK 25 at
all. And in two of them a *single registration site* produced rows with
different verdicts, because one `for cls in [...]` loop registers the same
native under several platform class names and at most one of those names is the
declarer on any given image. **The unit of adjudication is the row, not the
registrar and not even the call site.** The 117 rows L5 declined to claim are
filed as [`l5-native-io-bridge-residuals.md`](l5-native-io-bridge-residuals.md);
the largest group there is 25 `Bridge` registrations on VM-minted
`cratonvm/synthetic/Process*` classes — the `Function$Identity` shape found in a
second place.

**L5b/L5c then measured the same thing at crate scale**
([`l5bc-awt-builtins-bridge-residuals.md`](l5bc-awt-builtins-bridge-residuals.md)),
and added three facts this record should carry:

* **The marker is the starting point; the image is the evidence.** The only
  `JDK-ONLY-CLASSIFY: bridge` verdict in the whole tree outside `native-io` —
  `sun/awt/PlatformGraphicsInfo.hasDisplays0()Z` — turns out **not to be
  declared at all on a Linux JDK 25 image**; it belongs to the Windows and macOS
  variants of the class. Every one of the 603 statements L5b/L5c made rests on
  the census, and the one marker that licensed a statement produced none.
* **`native-collections` has nothing to migrate.** Its single
  `set_category(Bridge)` covers 1,350 rows and the image declares `ACC_NATIVE`
  on **zero** of them — the crate marker's static claim, now confirmed per row
  at runtime. Every row there is a reclassification question, so no
  `register_with_kind` lane will ever touch it.
* **`ABSENT` on a platform-named class means "not measured here", not "dead".**
  `WindowsSocketOptions`, `WindowsFileDispatcherImpl`, `WinNTFileSystem`,
  `PlatformGraphicsInfo.hasDisplays0` are all correct registrations on the
  platform that has them. Until a Windows-image census exists, no automated pass
  may treat absence as evidence of a defect.

**Where they come from now** (top five registering files, `Bridge` rows with no
`ACC_NATIVE` target): `native-collections/src/lib.rs` 1,350 ·
`native-builtins/src/lib.rs` 1,097 · `native-builtins/src/lang_misc.rs` 1,022 ·
`phases_late/nio_file.rs` 406 · `phases_late/foreign_ffm.rs` 367. Re-derive with
`python3 scripts/jdk-only-adjudicate.py <census.json>` section 4; section 7 is
the machine-readable block the ratchet freezes.

**The under-tagging direction is still clean:** zero `SyntheticStub` rows target
a method the image declares `ACC_NATIVE`.

## What changed on 2026-08-04 — step 2 exists, and the blocking evidence gap is closed

*What specifically must change* lists three steps. Step 1 (provenance) was
already met. Step 2 is now available and step 3's prerequisite is met.

**`NativeMethodRegistry::register_with_kind(class, method, desc, cb, kind)`
exists.** It states the kind at the registration site instead of inheriting it
from whatever `set_category` an ancestor frame last ran. It sets and restores
`current_category` around the inner `register` rather than passing the kind
down, deliberately: `register`'s body reads that field in a dozen places (the
two drop arms and the `keep_real_*` heuristics), and threading a parameter
through some of them would leave the ambient field authoritative for the rest —
exactly the split the entry point exists to remove. `#[track_caller]` on both,
so provenance still points at the registrar.

**The census can now tell "chosen" from "inherited".** *Evidence needed that we
do not have* said the blocking question was which of the 157 baseline entries
are deliberate stubs and which merely inherited the default. `registered_by`
answers *where* a registration was written; it does not answer whether anybody
decided what it is. The schema-2 census carries a **`kind_stated`** boolean per
row, true only for a registration made through `register_with_kind`.

Read it with the direction of the mistake in mind. `kind_stated: false` on a
`SyntheticStub` row means only "no `set_category` covered this call site", since
`SyntheticStub` is the default — very different from a deliberate stub, and the
two were previously indistinguishable. **`kind_stated: false` on a `Bridge` row
is the dangerous one**: `Bridge` is never the default, so it can only have been
inherited from a `set_category` line covering more registrations than its author
was thinking about. That is the shape of the 1,195-registration
`native-collections` verdict below, and the census will now say so per row
instead of per crate.

## The census exists now — 2026-08-04, and it changes the numbers below

*Evidence needed that we do not have* (bottom of this file) asked for the
schema-2 census taken from a real-JDK boot. It has been taken, with a column
schema 2 did not have, and **every count in the "Scale" and "Direction A"
sections below is wrong** — all in the same direction, and by a lot.

### The column that was missing

`real_declaring_method` answers from the **loaded** class store, so `loaded:
false` means "this workload never touched the class". That is the honest answer
to the question it asks and the wrong instrument for adjudicating a registry,
because the registrations most in need of a verdict are the ones no single
workload exercises. Schema 3 adds **`image_declaring_method`**, which asks the
same four questions of the bytes on the class path — parsed and discarded,
never loaded, because force-loading every registered name would *fabricate a
synthetic stub for every name the image lacks* and manufacture several hundred
violations out of the measurement itself.

Take it with `scripts/jdk-only-adjudicate.py`:

```sh
cratonvm --real-jdk --java-home <JDK25> --explain-jdk-only \
    --dump-native-registry census.json -cp probes JdkOnlyCensusLoadProbe
python3 scripts/jdk-only-adjudicate.py census.json
```

`--explain-jdk-only` is not optional; without it the column is `null` and the
script refuses rather than printing zeroes that read like a clean result.

### What it says (JDK 25 image, `--real-jdk`, 2026-08-04)

**11,909 registrations, not "about 8,000".** 678 `Intrinsic`, 10,844 `Bridge`,
387 `SyntheticStub`. Under `--jdk-only`: 11,522 rows and **zero**
`SyntheticStub`, refused at the door exactly as `stub_ratchet` says.

**`kind_stated` is `false` on all 11,909 rows.** `register_with_kind` exists and
has **zero callers**. Step 2's migration has not begun — which the section below
says, but the census makes it a measurement rather than a claim. *(Superseded
2026-08-05: 96 rows state their kind. See the section above.)* *(Superseded
2026-08-05: 96 rows now state their kind. See the section above.)*

**The `Bridge` population, adjudicated against the image:**

| what the image says about the target | rows | share |
|---|---:|---:|
| `ACC_NATIVE` — a genuine bridge, contract §1.5 | 760 | 7% |
| concrete bytecode (`has_code`) — a **shadow** | 4,796 | 44% |
| abstract method — intercepts every implementor | 1,321 | 12% |
| class present, method **not declared** — dead or misdescribed | 2,489 | 23% |
| class absent from the image (third-party library natives) | 1,478 | 14% |

So **10,084 of 10,844 `Bridge` registrations have no `ACC_NATIVE` target**, and
every one of them inherited its kind. The record below scopes this as a
`native-collections` problem at 1,195 registrations. It is a whole-tree problem
at 10,084, and `native-collections` is 1,338 of them — the largest single file,
but 13% of the total. `native-builtins/src/lib.rs` contributes 1,136 and
`lang_misc.rs` 1,022.

**The under-tagging direction is currently clean.** Zero `SyntheticStub` rows
target a method the image declares `ACC_NATIVE`. The 2026-07-14 regression shape
is not present in this image.

**`has_code` is not by itself a defect.** 554 of the 678 `Intrinsic` rows shadow
concrete bytecode, which is what an intrinsic *is*. The column is a defect
signal for `Bridge` specifically, because §1.5 defines a `Bridge` by its
`ACC_NATIVE` target.

### The first thing it found

Reading the three `java/lang/Thread` rows — `start0()V` `acc_native: true`,
`start()V` and `run()V` both `has_code: true` — pointed straight at a defect
nobody was looking for: under `--jdk-only` this VM **could not start a thread**,
because §7 step 3's decline fell through to `UnsatisfiedLinkError` instead of to
the bytecode. Fixed, with the evidence, in
[`jdk-only-section7-step3-unsatisfiedlinkerror-FIXED-20260804.md`](../../internal/jdk-only-section7-step3-unsatisfiedlinkerror-FIXED-20260804.md).
The other 4,795 shadowing `Bridge` rows can all reach that same path.

### What the census does *not* settle

The three `JDK-ONLY-CLASSIFY: unknown` groups that need something else:

* `register_stream_natives` — asked for a **benchmark**, not a `javap`. Still
  open; the image confirms `java.util.stream` has no `ACC_NATIVE` method
  anywhere, so the question is `Intrinsic` vs delete, and only measurement
  answers it.
* `register_interface_natives` — asked which of the 23 abstract-method
  registrations are dispatched **and against which receiver classes**. The
  census has invocation counts but not receiver classes; the second half needs a
  new column.
* `register_properties_natives` — asked for invocations plus `overwrote` across
  a real-JDK boot, and **both are in schema 2 already**. This one is answerable
  now.

## What is still open — which is the bulk of it

Nothing has been reclassified, and nothing here changes a single native's kind.
That is deliberate and matches the constraint the record itself sets: contract
§8 says *"Do not edit `native-builtins/src/lib.rs`; the 157-stub
reclassification is a separate wave with its own subsystem-per-PR discipline."*

Step 2's migration (registrar by registrar, starting with the
`JDK-ONLY-CLASSIFY`-marked ones) and step 3 (reclassify, then flip the default
last) are that wave. The tooling for it exists now; the wave does not.

The one thing to do before it starts is the run this record asks for: **take the
schema-2 census from a real-JDK boot of a workload that actually exercises the
classes being adjudicated**, and read `kind_stated` alongside `kind` and
`registered_by`. Do not guess a per-entry disposition before that run exists —
and note that a row of all-`false` in `real_declaring_method` means "this run
did not exercise the class", not "the JDK does not declare this method".

## What is wrong

A native's `NativeKind` is never stated at its registration site. It is
inherited from a mutable field on the registry that the *enclosing* registrar
function happens to have set.

`native-api/src/registry.rs`, `NativeMethodRegistry::register` (currently
~4667, `#[track_caller]`):

```rust
#[track_caller]
pub fn register(
    &mut self,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: NativeCallback,
) {
```

— four arguments, none of them a kind. The kind comes from
`self.current_category`, which is:

* declared as a plain field (`current_category: NativeKind`, ~4351);
* initialised in `new()` to **`NativeKind::SyntheticStub`** (~4480);
* mutated by `set_category(kind)` (~4553) and by the scoped
  `with_category(kind, |r| { ... })` (~4566).

The type's own doc comment (~4125) states the intent explicitly:

> The registry's `current_category` defaults to `SyntheticStub` — the
> conservative choice, so anything an author forgets to tag stays visible to the
> audit and gateable, never silently trusted.

The intent is defensible. The consequence is not, and it runs in **both**
directions.

## Direction A — the dominant one: wholesale over-tagging as `Bridge`

This is the finding that inverts the original premise of this record. It came
out of the ambient-category audit
([`docs/jdk-only-ambient-category-audit.md`](../../jdk-only-ambient-category-audit.md))
and is recorded in code as a `JDK-ONLY-CLASSIFY` verdict on
`register_collections_natives` in `native-collections/src/lib.rs` (~1484):

> **JDK-ONLY-CLASSIFY: stub — crate-wide verdict, applied by one line.** The
> `set_category(Bridge)` below is not a per-registration judgement: it is a
> dynamic ambient assignment that every callee in this file inherits, and it
> covers 1,195 of the crate's 1,219 registrations. Cross-checked against JDK 25
> with `javap -p -s`, **not one** of those 1,195 targets an `ACC_NATIVE` method
> (622 target methods with concrete bytecode, 214 are abstract interface
> methods, the remainder are absent from the image or use a class name this
> audit could not resolve statically). `java.util` is pure Java: there is no
> VM/OS boundary in this crate to bridge to.

One `set_category(Bridge)` line, at the head of `register_collections_natives`,
tags 1,195 registrations. Contract §1.5 defines a `Bridge` as what an
`ACC_NATIVE` method binds to; none of these are. Contract §1.4 lets a `Bridge`
lose to concrete bytecode, so `Compatible` mode is not wrong today — but the
census says "1,195 legitimate bridges" where the truth is "1,195 registrations
nobody adjudicated", and `--jdk-only` admits every one of them.

The same marker states the constraint on fixing it, and it must be respected:

> **DO NOT flip this line to `SyntheticStub` as a bulk edit.** That is the exact
> shape of the 2026-07-14 regression, at ~8x the blast radius, and the 214
> abstract-interface registrations additionally decide dispatch for every USER
> subclass, not just for `java.util` classes.

`JDK-ONLY-CLASSIFY` markers now exist in 18 files (12 in
`native-awt/src/natives.rs`, 7 in `native-collections/src/lib.rs`, 6 in
`native-io/src/lib.rs`, the rest one or two each across `native-io`,
`native-builtins-security`, `native-builtins-crypto` and `native-awt`). They are
the per-registrar verdicts; read them before touching a `set_category` line.

## Direction B — under-tagging, which drops the registration

`register()` does not merely *label* a stub. Under the strict policy it refuses
it (~4689), and under `CRATONVM_NO_STUBS` it drops it (~4721):

```rust
if self.drop_synthetic_stubs && self.current_category == NativeKind::SyntheticStub {
    …
    return;
}
```

So under `CRATONVM_NO_STUBS` — and under `--jdk-only`, which the contract
defines as a stricter superset of the same rule — a mis-tagged bridge is not
registered at all. The method then falls through to real bytecode that may not
exist, or to a `NoSuchMethodError`, at a point far from the registration.

### Prior occurrence — this is not hypothetical

The drop path carries its own historical note inside `register`:

> `CRATONVM_DBG_DROPPED_STUBS=1`: list every registration this mode silently
> drops. Added 2026-07-14 while chasing a real-JDK-mode bootstrap regression
> (`InternalError: null property: java.home`) that traced back to a whole
> `register_*` function's worth of permanent bridges (`java.util.Properties`'
> side-table natives) being mis-tagged `SyntheticStub` by inheriting the wrong
> ambient category at one of its call sites.

One ambient-category mistake cost an entire registrar's worth of permanent
bridges and presented as an unrelated bootstrap `InternalError`.

## What has already been retagged — do not redo it

**JMX and `java.util.function.Function$Identity` are `NativeKind::Bridge`
today.** They are not in the residual 157. The retags are in
`native-builtins/src/jmx.rs` (`set_category(Bridge)` at the head of each
registrar) and `native-builtins/src/lib.rs`'s
`register_function_identity_natives` (~36914), which carries the reasoning:

> Tagging this `SyntheticStub` broke real-JDK-mode boot the moment
> `set_drop_synthetic_stubs(true)` started actually dropping `SyntheticStub`
> registrations (dev `d8092acb`, 2026-07-14): WildFly's very first
> `getRuntimeMXBean()`-adjacent lambda hit `UnsatisfiedLinkError:
> Function$Identity.andThen`. Tag as `Bridge` (needed in both modes) instead —
> same class of bug/fix as the `native-builtins/src/jmx.rs` JMX cluster retag,
> both landed together.

`vm/src/vm/vm_init.rs` (~1467) records the same conclusion from the contract's
side: these are *permanent bridges with no real-bytecode fallback*, which by §1's
terminology table makes them `Bridge`, not stubs.

### The successor defect: `Function$Identity` now survives a class §5 forbids

Retagging fixed the registration. It created a new inconsistency that nobody has
adjudicated:

* The five `Function$Identity` natives are now `Bridge`, so **`--jdk-only`
  registers and invokes them**.
* `java/util/function/Function$Identity` **has no class file anywhere**. The
  registrar's own comment says so: *"there is no real classfile named
  `Function$Identity` to fall back to at all."* It is minted by
  `alloc_concurrent_synthetic(ctx, "java/util/function/Function$Identity", 0)`
  (`native-builtins/src/lib.rs` ~36608, `phases_late/streams.rs` ~2898), which
  bottoms out in `ensure_synthetic_class`.
* Contract §5 forbids fabricating exactly that class under `JdkOnly`, and
  `ClassManager::try_ensure_synthetic_class`'s doc comment names it as the
  *chief* intended caller of the refusing entry point.

So under `--jdk-only` the surviving `Bridge` native is a bridge to a receiver
whose class the policy says may not exist. Today nothing breaks, because
`ensure_synthetic_class` records the violation and fabricates anyway (see
[`ensure_synthetic_class` cannot enforce](ensure-synthetic-class-cannot-enforce-only-record.md)) —
the two defects are cancelling each other out. Fixing either one alone exposes
the other. The real fix is upstream: `Function.identity()` should return the
real lambda, not a VM-minted stand-in.

## Scale

**Re-measured 2026-08-04 — see the census section at the top of this file. The
`set_category` site count below is correct; the 1,195-registration figure it is
usually paired with is not (the true figure is 10,084 unadjudicated `Bridge`
registrations tree-wide, 1,338 of them in `native-collections`).**

`set_category(` / `with_category(` appear **1,169 times across 123 files**
(ripgrep over the workspace, re-counted 2026-07-31 against the re-landed tree),
concentrated in `native-builtins/src/phases_early.rs` (121),
`native-collections/src/lib.rs` (104), `native-builtins/src/phases_late.rs` (75),
`native-builtins/src/phases_late/bouncycastle.rs` (68) and
`native-builtins/src/jmx.rs` (50). Every one of those is a scope whose
*interior* — including anything it transitively calls — silently adopts a kind.

*(The original record said 1,174 across 134 files. The difference is the
re-land plus the retags, not a measurement error in either direction; treat both
as "about twelve hundred".)*

## Why it was not fixed in wave 1

Contract §8 says explicitly: *"Do not edit `native-builtins/src/lib.rs`; the
157-stub reclassification is a separate wave with its own subsystem-per-PR
discipline."* Reclassifying is also not a refactor that can be done blind — it
requires knowing, per registration, whether the tag was *chosen* or *inherited*.

## What specifically must change

1. **Provenance is now captured — use it.** `register` is `#[track_caller]` and
   `NativeCensusEntry.registered_by` is populated from
   `core::panic::Location`, redacted unless `--explain-jdk-only`. The census
   can now distinguish "tagged deliberately at this line" from "inherited from
   a `set_category` three frames up". That was the blocking prerequisite in the
   original filing and it is met.
2. Add an explicit-kind entry point (`register_with_kind(class, method, desc,
   cb, kind)`) and migrate registrars to it subsystem by subsystem, so the kind
   is a local fact rather than a property of the call stack. **Start with the
   `JDK-ONLY-CLASSIFY`-marked registrars**, which already carry an adjudicated
   verdict. *(Entry point: done. `native-io`'s four `bridge` registrars: done
   2026-08-05, 87 of 204 registrations — a marked verdict is where to start, not
   a licence to convert the whole function; adjudicate per row.)* *(Entry point: done. `native-io`'s four `bridge` registrars: done
   2026-08-05, 87 of 204 registrations — the marked verdict is a starting point,
   not a licence to convert the whole function; adjudicate per row.)*
3. Only then reclassify. Flip the default last: once every registration states
   its kind, `current_category` can default to something that fails loudly (or
   be deleted).

## How to verify a fix

* `native-builtins/tests/stub_ratchet.rs` asserts
  `BASELINE_SYNTHETIC_STUBS = 157` **exactly**, with `SLACK = 0`. Any
  reclassification changes that number; re-baseline it to the count the test
  itself prints, and keep `SLACK` at zero.
* The same file's `essential_registry_is_populated` asserts only
  `total >= MIN_TOTAL_REGISTRATIONS` (`8_000`). That is a vacuity floor, **not**
  a claim about the exact total — do not treat any specific total as a verified
  invariant. Its strict sibling asserts `strict_total >= 7_500` for the same
  reason: to catch a registry that reports zero stubs because it is empty.
* `strict_registry_has_zero_synthetic_stubs` passes today, and the test says why
  in terms worth keeping: *"not because the 157 stubs are gone, but because
  `register()` refuses them at the door under `JdkOnly`."* A reclassification
  that makes it pass for a different reason has changed the meaning of the gate.
* `CRATONVM_DBG_DROPPED_STUBS=1` on a real-JDK boot lists every registration the
  strict path removes. A reclassification is wrong if this list gains an entry
  whose method the boot then needs.
* Per-subsystem: run with `CRATONVM_NO_STUBS=1` before and after. A newly-broken
  boot means a bridge was mis-tagged; a newly-*working* path that used to return
  a fake means a stub was correctly unmasked.

## Blast radius if done wrong

* **Stub → Bridge (too generous):** the fake survives `--jdk-only`. Contract §11
  ("final native registry contains zero `SyntheticStub` entries", "zero
  synthetic-stub invocations through any path") becomes false while the census
  reports green. This is the failure mode that makes the whole gate worthless,
  it is invisible, and — per the `native-collections` verdict above — **it is
  already the majority disposition in the tree**, not a hypothetical.
* **Bridge → Stub (too strict):** exactly the 2026-07-14 regression — a whole
  registrar disappears under `CRATONVM_NO_STUBS` / `--jdk-only` and surfaces as
  an unrelated error much later in boot.

Because both directions are silent at the point of the mistake, reclassification
must be done in reviewable subsystem-sized batches with the ratchet re-run each
time, never as a bulk sweep. The 1,195-registration `set_category` line is the
proof that a one-line "fix" here is a one-line thousand-registration change.

## Evidence needed that we do not have — SUPERSEDED 2026-08-04

This section asked for the census taken from a real-JDK boot, and for the
per-entry adjudication built on it. Both exist; see *The census exists now* at
the top of this file, and re-take it with `scripts/jdk-only-adjudicate.py`
rather than reasoning from the counts below.

What is still genuinely missing, and is the next instrument to build:

* **Receiver classes per invocation.** `register_interface_natives`'s verdict
  turns on which classes a native-on-an-abstract-interface-method actually
  intercepts, including user-defined implementors. The census counts
  invocations; it does not record what they were dispatched *against*.
* **A workload broader than one probe.** `probes/JdkOnlyCensusLoadProbe.java`
  dispatched 401 of 11,909 slots. That is enough to adjudicate the *static*
  question for every row — `image_declaring_method` does not depend on the
  workload — but the invocation column is only as wide as what ran. Take the
  census from H2 or Spring Boot before deciding anything that turns on "is this
  ever called".
