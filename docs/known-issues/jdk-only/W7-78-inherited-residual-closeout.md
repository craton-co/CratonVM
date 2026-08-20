# W7-78 — the inherited residuals, closed out: four retirements, one refusal, and five patch blocks that read like work

> # A34 2026-08-12 — RE-ADJUDICATED. The retirements verify, §5.3's vector was
> # RUN, and §4 — this record's headline finding — is SUPERSEDED IN THE TREE.
>
> **§4 is the one to read.** This record's most quoted result is that
> `service_accepts_type` has *"exactly one caller"* against a feature with two
> provider paths, and that the missing sibling check is what held `W6-2` back
> from retirement. Re-counted today, the way §7 says to count:
> **`service_accepts_type` has TWO callers**, `factory_return_is_subtype` and
> `constructor_form_is_subtype`, and each of those is applied on BOTH the
> iterator and the stream path (four call sites in
> `native-builtins/src/service_loader.rs`). The gap was closed by
> `f95b6b363` — *fix(serviceloader): W7-85 — apply the factory return-type gate
> on `stream()` too* — which already carries its own record number, and the
> constructor-form check §4 downgraded to *"deferred for want of a
> measurement"* is implemented and wired on both paths as well.
>
> So `W6-2` is no longer held back by the row this record held it back on. It
> may still be held back by something else; that is `W6-2`'s adjudication and
> not this one's, and it is nominated rather than assumed. **What is settled is
> that §4's stated blocker is gone.**
>
> The irony is worth keeping rather than smoothing over, because it is the
> generalisation this record itself ends on: §7 says *"ask who calls the fix,
> and count the answers against the number of paths the feature has"* — and
> this record's own answer to that question went stale within a day. A caller
> count is a **timestamp**, not a property. Re-run the `grep` before quoting
> the number, including when the number is your own.
>
> **§5.3 — the vector was run, and the news is good.** §5.3 states the four new
> nestmate checks *"have never been run on CratonVM"* and that a red would be
> the useful outcome. Run now on `scratchpad/bin/cratonvm-merged-dev.exe`
> against Microsoft JDK 25.0.3.9: `PASS RJdkReflect` on HotSpot, `--real-jdk`
> and `--jdk-only` alike. **`L15`'s landed narrowing is therefore not inert** —
> and specifically the fourth check, the non-nestmate private-field read that
> must raise `IllegalAccessException` and is the only one that discriminates a
> too-permissive gate from a correct one, passes on CratonVM. That was the
> question nothing had ever asked.
>
> One number in §5.3 is already stale: it says the vector goes *"60 → 64"*, and
> all three arms report **67 checks** today, so it has grown again since. The
> claim to carry forward is "the four checks are present and green in both
> modes", not the arithmetic.
>
> **Verified unchanged, by re-reading rather than by trusting the record:**
> `record_boot_loader_library` (§5.1) still has **zero** callers tree-wide —
> the only other occurrence of the name is inside a doc comment, which a naive
> grep would have counted as a caller — and the six records §2 retires
> (`W7-4`, `W7-11`, `W7-28`, `W7-32`, `W3-6`, `W5-2`) are all absent from this
> directory while `W6-2` is still present, exactly as §1's table claims.
>
> **Scheduling:** this record's evidence is the rare kind that *is* scheduled.
> `RJdkReflect` is a `regression-suite/src` vector, so `run.sh` re-runs it at
> every `SUITE=` value. Most of this directory's evidence lives under `probes/`,
> which `run.sh` never names.

**Status: COMPLETE, 2026-08-12.** One test file changed
(`regression-suite/src/RJdkReflect.java`, +4 checks, +1 helper class); no `.rs`
file changed; both runtime modes are untouched by every change here. Nothing was
built — this lane cannot run `cargo`. Everything stated as measured was measured
on HotSpot 25.0.3.9 (Eclipse Adoptium) or by reading the current tree.

This is the follow-on to W7-55-record-reconciliation.md, which adjudicated 58
records and then deliberately stopped short of three things it said were the
orchestrator's: retiring the five records it found empty, working the remaining
live inherited residuals, and finishing the DEAD-marking it claimed was already
complete. All three are done here.

---

## 1. The headline number, and the one that matters

| | count |
|---|---|
| Records nominated for retirement by W7-55 §4 | 5 |
| **Retired** | **4** |
| **Held back on evidence** | **1** (`W6-2`) |
| Retirements W7-46 had *declared* but never `git mv`'d | 2 (`W3-6`, `W5-2`) — completed |
| Records that left the directory in total | **6** |
| Records remaining | **84** |
| Dead prescriptions W7-55 said were "marked in place" | 9 |
| — actually marked adequately | **4** |
| — **marked weakly or not at all** | **5**, now fixed |
| New live residuals found while adjudicating | **1** (`W6-2`'s stream path) |
| Stale index rows corrected | 5 |

**The number that matters is 4 of 9.** W7-55's own §6 says all nine superseded
prescriptions are "marked in place". Five were not — and one of those five,
`W7-22`, had **zero** markers anywhere in its file (a case-insensitive grep for
`inert`, `revert`, `W7-25`, `SUPERSEDED`, `DEAD`, `reconcil` returned nothing)
while its section was still titled *"Live defect"* and its prescribed repair had
been written, measured inert in both modes, and reverted.

That is the same failure mode W7-55 exists to document, one level up: **a
reconciliation's own claim about its output went unverified.** The cost of not
checking would have been a lane rebuilding the JUL `LogManager` singleton
through its real constructor, which is a known-inert change.

---

## 2. The retirements

Evidence per record is in `RETIREMENT-20260812.md`; the four moved records each
carry a banner naming what discharged them. In summary:

* **`W7-4`** — instrument record, deliverable discharged and acted on five times
  over. **Retired partly *because* it is stale:** its 540-line HotSpot oracle
  predates the probe's declare/manifest ledger, and the current transcript is
  864 lines. Anyone diffing against the record would manufacture divergence.
* **`W7-11`** — 68/0 the day it was written; all four named defects landed
  (`46bb0ad2e`, `8b4443fc6`+`beb8acee7`, `5266bf8c7`, `87ab40daf`). Its quiet
  row — the `cratonvm/internal/Unmodifiable*` link — was filed as *"plausible
  and not yet proven"* and was never the mechanism; the four closures name four
  unrelated causes.
* **`W7-28`** — the worst *status* offender in the directory, still reading *"the
  switch that turns it off is NOT WIRED"* over four applied parts. See §3.
* **`W7-32`** — pure measurement, fully owned elsewhere. **Its successor is
  `W7-42`, not `W7-40`**, which is itself banner-marked superseded because five
  of its fourteen divergences were the instrument.

And two moves that were declared but never made: `W3-6` and `W5-2` both
self-declared *"RETIRED 2026-08-12 (W7-46)"* and were indexed as retired, while
sitting in the public known-issues directory contradicting their own status
lines. `W3-6`'s one remaining index row was re-checked before moving and is
closed: Windows `destroy()`/`destroyForcibly()` **do** route through `signal_pid`
(`native-io/src/process.rs:1830` → `destroy_handle` → `:1029`); the index row
saying otherwise was stale, and `signal_pid` staying private is correct because
both callers are in that file.

Completing those two also makes a row in `W6-10` true for the first time: it
said its target record *"was retired out of this directory, so decide where the
row belongs first"*, which was false until today.

---

## 3. The one row settled by running something instead of reasoning

`W7-28` part D asked whether the two TornadoVM benchmark class files are
actually preview-stamped, and **named the exact command**. Nobody had run it in
the four days the record was open. It costs one line:

```
$ od -An -tx1 -N8 bench-tornado/PolyEvalTornado.class
 ca fe ba be 00 00 00 45
$ od -An -tx1 -N8 bench-tornado/VectorAddTornado.class
 ca fe ba be 00 00 00 45
```

`minor = 0`, `major = 69`. Neither is preview-stamped, so neither can reach the
new arm. **W7-55's justification for retiring D was wrong** — it cited the reader
gate in `reader/src/class_file_version.rs`, which answers a question D never
asked; D is about two shell scripts. The conclusion survives only because the
measurement was finally taken.

**The generalisation.** A record that names its own falsifying command and is
then closed by *reasoning* has had its cheapest evidence left on the table. Run
the command the record already wrote before writing a paragraph about why you
do not need to.

---

## 4. `W6-2` was not retired, and the row that stopped it is a vacuous green

> **SUPERSEDED 2026-08-12 (A34) — the gap below is CLOSED in the tree; do not
> apply the fix this section describes.** `service_accepts_type` now has TWO
> callers (`factory_return_is_subtype`, `constructor_form_is_subtype`), each
> applied on BOTH the iterator and the `stream()` path. The stream-path gate
> landed in `f95b6b363` under its own number, `W7-85`. The constructor-form
> check this section downgrades to "deferred" below is implemented too. The
> *vacuous-green shape* this section names — a guard installed on one of two
> siblings, validated by a fixture that only ever satisfies it — remains a
> correct and useful catalogue entry; only its worked example is spent. Marked
> here at the finding rather than only in the banner, per this record's own §7.

W7-55 §4 argued `W6-2` was empty because its two *"deliberately NOT done"* items
are argued refusals. Both were re-read. The row that holds this record back is
**neither of them**, and no record, index or audit had ever mentioned it.

`service_accepts_type` (`native-builtins/src/service_loader.rs:1677`) — the
factory-return-type subtype check this record's headline fix is proud of — has
**exactly one caller**, at `:1897`, inside `native_sl_iterator`. The stream path
(`native_sl_stream`, factory block `:2401-2420`) computes `factory_return_type`
and uses it only to build the wrapper. It never asks. So:

```java
ServiceLoader.load(Svc.class).iterator()   // illegal provider -> ServiceConfigurationError  (correct)
ServiceLoader.load(Svc.class).stream()     // illegal provider -> quietly handed out          (divergence)
```

**Why 44/44 cannot see it.** The fixture's `FactoryGreeter.provider()` returns
`Greeter` — a *correct* subtype. The vector walks only the positive case, so a
check that is present on the path the test takes and absent on the path it does
not take reads green forever. This is a new shape for this directory's
vacuous-green catalogue: not a probe that cannot fail, and not a run that never
ran, but **a guard installed on one of two siblings, validated by a fixture that
only ever satisfies it.**

The technique that found it was not reading the record. It was asking `grep -n
"service_accepts_type"` **who calls this**, and counting the answers against the
number of paths the feature has. One caller, two paths.

Not fixed here: the block to mirror re-reads `sl` and the return-type mirror
through `read_native_pin` *after* the allocating `factory_return_type` call, in
an order a comment at `:1886-1889` spells out, and this lane cannot compile.
It is also a `Compatible` change — permissible, because raising
`ServiceConfigurationError` there is genuine HotSpot parity, but only behind a
measurement. The negative fixture and the exact command are in
`RETIREMENT-20260812.md` §3.

`W6-2`'s constructor-form subtype check is also **downgraded from "argued
refusal" to "deferred for want of a measurement"**, because its own stated reason
is *"this lane cannot measure that"*. That is a deferral. Calling it a decision
is how a record gets retired with a divergence inside it.

---

## 5. The three live inherited residuals

### 5.1 `W6-6` — verified live, plus a hazard on its *headline* nobody had checked

The residual is real: `lang_system::record_boot_loader_library`
(`native-builtins/src/lang_system.rs:3183`) has **zero callers tree-wide**, and
`jdk/internal/loader/BootLoader.loadLibrary` is a `|_ctx, _args| Ok(None)` no-op
(`native-builtins/src/lib.rs:14049-14054`; the record's `:13813` anchor had
rotted). The strict-only half W5-1 landed is genuinely there —
`LOADED_LIBRARIES` is a real `VmScoped` at `lang_system.rs:1757`, torn down from
`forget_vm_system_singletons` at `:3143`, not a process global — it simply has no
boot-loader event to record.

**The new finding is on the fix, not the residual.** That no-op registers with
the bare `registry.register(...)`, so its `NativeKind` is **ambient**. Its
enclosing registrar is `register_essential_natives_with_shims`
(`lib.rs:7084`), which sets `Bridge` at `:7139-7140` and restores it after the
single temporary `Intrinsic` window for regex (`:7666-7680`). Line 14049 is
outside that window, so the kind is `Bridge` — and that is load-bearing, because
`NativeKind::allowed_in` drops `SyntheticStub` under `JdkOnly`. Had this
registration ever drifted into a `SyntheticStub` window, the no-op would be
dropped in strict mode, real `NativeLibraries` bytecode would run in its place,
and the JDK's native-library lock this short-circuit exists to avoid during
Linux boot-class `<clinit>` would be back — as a **hang**, in strict mode only,
on the platform this host cannot test.

**Why arming is not done here.** It is one line, but it can only ever turn a
success into an `UnsatisfiedLinkError`, and the first library it would claim for
the boot loader is `net` — which `is_vm_provided_jdk_library` deliberately still
carries *because* the dynamic rule cannot fire (`lang_system.rs:1939-1949`).
Arming without measuring risks flipping `RJdkJni`'s `net` probe. The run is in
`README.md` §2.6, and it must be taken with and without the arming, with
`--java-home`.

### 5.2 `W6-12` — the confinement is real, and provable more cheaply than the record proves it

The record argues the `Collections` fidelity residual is confined to
`synthetic-jdk` by registration **order**. Order is not what confines it, and
relying on order here would have been wrong: `phases_early`'s five identity
bindings (`native-builtins/src/phases_early.rs:100`, `native_return_first_arg`)
sit in an ambient **`Intrinsic`** window, and `Intrinsic` is *not* dropped under
`JdkOnly`.

What actually confines them is a `cfg`. Their only entry point is
`lib::register_synthetic_overrides` (`native-builtins/src/lib.rs:21412`, called
at `:41086`), and in a non-feature build that symbol is replaced by a
`#[cfg(not(feature = "synthetic-jdk"))]` **no-op shim** at
`vm/src/native/builtins.rs:29`. The identity bindings are not out-voted in the
shipping binaries; they are **not compiled into them**. What is left is
`native-collections`' genuine wrappers (`native-collections/src/lib.rs:50688-50727`
in their own `set_category(SyntheticStub)` window, plus `unmodifiableList` at
`:15381`), which being `SyntheticStub` are themselves dropped under `--jdk-only`
— leaving real `java.util.Collections` bytecode. That is the mechanism behind the
37 byte-identical probe rows, and it is a stronger statement than the record's.

**One index claim is now stale.** "No `--features synthetic-jdk` binary has ever
been built" is false: W7-50-synthetic-jdk-strict-six.md built one and measured
**63 passed / 7 failed** under `--jdk-only`, correcting a tracked 48/6. What has
still never been run is that binary in the `--synthetic-jdk` **runtime mode**,
which is the only configuration this residual exists in. Feature and mode are
different things; W7-50 §0 exists because conflating them produced all six of its
defects.

The repair is identified precisely (delete the eight identity registrations in
`phases_early`, in two registrars) and deliberately not applied:
`phases_early.rs`'s collections registrars are inside W7-50's blast radius, and
an uncompiled edit to the one arm nobody can build is how the 48/6 figure went
stale in the first place.

### 5.3 `L15` — the missing vector, written and HotSpot-verified

`L15` landed a narrowing in `check_field_access` — nestmate, same-package and
subclass callers admitted in place of a same-class-only predicate — and recorded
that the probe meant to exercise it was never added. Every field assertion in
`RJdkReflect` called `setAccessible(true)` first, so the *old* predicate read
green too: **the fix was landed and unexercised.**

Four checks now sit immediately before the first `setAccessible` on a field,
exactly where the record asked for them:

| check | HotSpot 25.0.3.9, measured |
|---|---|
| nestmate private instance field **get**, no `setAccessible` | succeeds |
| nestmate private instance field **set**, no `setAccessible` (value restored) | succeeds |
| nestmate private **static final** field read, no `setAccessible` | succeeds |
| **non**-nestmate private field read, no `setAccessible` | `IllegalAccessException` |

The fourth is what makes this a real instrument rather than three assertions
that pass on anything permissive. Under a gate that admits too much — or none at
all, which is what `Constructor.newInstance` has today — the three positives all
pass and only that one goes red. Its subject is `RJdkReflectOutsider`, a
package-private top-level class in the same compilation unit: **same package,
different nest**, which separates the `private` rule from the package rule. It is
deliberately not a separate `src/*.java` file, because `run.sh`'s list hygiene
requires every one of those to be a listed vector or named in
`UNREGISTERED_CLASSES`.

The vector goes **60 → 64** and passes on HotSpot. **It has never been run on
CratonVM.** If `RJdkReflect` goes red at the next suite run, the reading is that
the landed narrowing is inert — which is precisely the question nothing had ever
asked, and a red is the useful outcome, not a regression.

`L15`'s two other residuals stand and are not closable from here:
`Constructor.newInstance` (`native-builtins/src/lang_class.rs:11017`) has **no**
member-modifier gate, so closing it is a narrowing whose blast radius is every
reflective instantiation in the corpus; and hidden classes are never nestmates
because `NativeContext::is_hidden_class` does not exist, which fails **closed**.

---

## 6. What else was found while doing this

* **`README.md` §2.0 was corrupted by a bad merge** and was, until this pass, the
  most dangerous section in the directory: it was headed *"Records that are FULLY
  CLOSED and should be retired"* and its table listed `L8`, `L15`, `L16`,
  `W7-53`, `W4-2`, `W5-1`/`W6-6`, `W4-3`, `W6-12`, `W7-46`, `W7-61` with a "Why
  it is closed" column that actually described each one's **live residual**.
  `W6-2` appeared twice, five lines apart, once as open and once as closed. The
  file also carried three conflicting `**Status:**` lines (21, 22 and 61
  records). Rebuilt.
* **Nineteen records filed on 2026-08-12 were in no index row at all.** The
  README warns that an index listing closed work as open costs runs; an index
  silently *missing* open work costs the same runs in the other direction. They
  are now in §2.3.
* **Three more line-citation rots**, all in rows a reader would act on: `W6-6`'s
  `lib.rs:13813` → `:14049`, `W4-3`'s ratchet at `provider_chain.rs:4043` →
  `every_advertised_sunjce_cipher_is_serviceable`, and `W3-6`'s "still private"
  claim about `signal_pid`, which is routed. The standing rule to anchor on a
  marker tag rather than a number keeps being right.
* **`W7-20`'s "build-blocking" baseline rows are already fixed** — the nine
  `LinkedListSnapshotListItr` rows in
  `scripts/baselines/jdk-only-kind-map-25-linux.tsv` are `bridge`, not
  `synthetic-stub`. W7-55 §8 flagged them as pending.

---

## 7. The rule this pass adds

W7-55 ended with *"`git log -S` before you believe a record."* This pass adds the
sibling for the writers rather than the readers:

**Put the DEAD marker at the patch block, not in the status line.**

A reconciliation that records a superseded prescription in a status block 400
lines above the patch, or only in the index, has documented the problem without
removing it. Nobody reads a 650-line record top to bottom before applying a patch
out of its middle; they search for the code fence. Four of nine markers were in
the right place, and those four are safe. The other five each sat between 88 and
480 lines from the text they were correcting — and one sat nowhere at all.

And, for anyone adjudicating a record for retirement:

**Ask who calls the fix, and count the answers against the number of paths the
feature has.** `W6-2`'s subtype check had one caller and two provider paths. No
amount of reading the record would have surfaced that; one `grep -n` did.
