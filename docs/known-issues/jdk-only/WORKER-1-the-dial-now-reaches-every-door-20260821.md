# WORKER-1 — the dial now reaches every door, and the hybrid it used to price was WORSE than the uniform state, not better

**Status: LANDED, with three armed failures newly priced and OPEN.** 2026-08-21,
branch `fix/enforcement-dial-all-doors-20260821`, cut from `origin/dev` at
`1fcc241e0`. Every number below is **MEASURED** on Azure host 2
(`azureuser@20.80.105.49`), oracle HotSpot 25.0.3+9 from
`/data/toolchain/jdk-25`, unless the line says ARGUED.

Answers `H17-3` §6's specification and the four deliverables of
`WORKER-1-the-enforcement-dial-reaches-one-door-of-four-20260821.md`.

**Where those live.** `H17-1`, `H17-2`, `H17-3` and the `WORKER-1` brief are on
`claude/jdk-only-mode-handoff-09b48c` and are **not on `dev`** — that branch is
159 commits off `dev` and has not been merged. Every claim of theirs this record
depends on is therefore quoted inline rather than cited, so this page stands on
its own until H0 lands them.

---

## 0. The one-paragraph version

`CRATONVM_ENFORCE_NATIVE_SHADOW` had one live call site of the fourteen dispatch
doors that can run a `Bridge` in front of real bytes. **MEASURED: 890 of 947
armed `Bridge` dispatches never asked it** on the cheapest possible probe, and
**299 431 of 299 469** on a hot one — so a suite, which is a hot workload, was
very nearly unarmed. Three doors carried the whole leak. They now ask; the
per-door leak is **zero** in every case measured. The unarmed arms did not move,
class-for-class. And `H17-1`'s prediction that armed numbers would go DOWN is
**FALSIFIED by its own stated falsifier**: `HashMap` went **81/104 → 86/104**,
because the half-armed state was a corrupted hybrid — a map reporting `size()=4`
over four occupied buckets that iterated **one** key — and no retirement can
produce that.

---

## 1. MEASURED — the instrumented build, which is `H17-3` §6's first instruction

Fourteen call sites of `resolve_native_dispatch_wave1` now carry a
`DispatchDoor` tag, and the resolver counts armed-class `Bridge` arrivals and
whether the door sent them to bytecode. `rg -n 'resolve_native_dispatch_wave1\($'`
is the check that the enum is still the complete set.

`DialWitness direct`, armed for `java/util/HashMap`, **before any behavioural
change** (instrumentation only, no dispatch edit):

```text
[DIAL_DOOR_CENSUS] armed=true reached=947 yielded=57 leaked=890
[DIAL_DOOR] step1             reached=15   yielded=15   leaked=0
[DIAL_DOOR] invoke_or_native  reached=115  yielded=0    leaked=115
[DIAL_DOOR] force_intercept   reached=28   yielded=28   leaked=0
[DIAL_DOOR] cache_revalidate  reached=774  yielded=0    leaked=774
[DIAL_DOOR] cache_populate    reached=1    yielded=0    leaked=1
[DIAL_DOOR] jit_fast_native   reached=3    yielded=3    leaked=0
[DIAL_DOOR] stackless_force   reached=11   yielded=11   leaked=0
```

`DialWitness jit` (60 000 iterations, hot enough to compile the caller):

```text
[DIAL_DOOR_CENSUS] armed=true reached=299469 yielded=38 leaked=299431
[DIAL_DOOR] cache_revalidate  reached=299292 yielded=0  leaked=299292
```

**This converts `H17-2` §5 from ARGUED to MEASURED, and it corrects it in one
place.** `H17-2` named four doors and put the reflective one first. The census
says the reflective path does not have a door of its own for this class — it
arrives through `invoke_or_native` — and that the door carrying 82% of the leak
is the **warm invoke cache** (`revalidate_cached_native`), which `H17-2` could
only reach by inference and explicitly listed under "what I did NOT verify".

**Also MEASURED, and not previously stated anywhere:** four doors —
`force_intercept`, `stackless_force`, `jit_fast_native`, `elidable_ctor` — pass
`bytecode_available: true` unconditionally under `--jdk-only` and were therefore
**already stricter than the dial**. Arming changed nothing there because nothing
was left to change. That is why the fix is three doors and not fourteen.

---

## 2. The fix

One shared predicate, `jdk_only_dial_yields_to_bytecode`
(`vm/src/runtime/interpreter/native_override.rs`), asking the same three-way
question step 1 asks, cheapest test first: `Bridge` only → `--jdk-only` only →
the dial must cover *this receiver class* → and only then the hierarchy walk.
Every edit is inside that conjunction, so an unarmed run cannot observe that any
of it exists.

| door | was | now |
|---|---|---|
| `invoke_or_native` | `bytecode_available: has_real`, and `has_real` is only ever true for a `SyntheticStub` on an allow-listed class — so `false` for every `Bridge` | `has_real \|\| dial` |
| `revalidate_cached_native` | hard-coded `false` | asks the dial **on every warm hit** |
| `populate_invoke_cache` | hard-coded `false` | asks before deciding what to cache |
| `invoke_or_native`'s three bare `find(..)` calls — the array-type alias retry and both arms of the superclass walk | **no §7 routing at all**, no policy, no census | `find_with_kind` + the dial |

Those last three were not in `H17-3`'s specification and are worth naming
separately: they are dispatch sites that reach `safe_native_call` without ever
passing through `resolve_native_dispatch_wave1`, so no resolver-side census can
see them, and the §7 routing note in `vm_exec.rs` does not list them. **Only the
dial is wired to them here.** Routing them properly is larger than this lane and
is N2 below.

### The 2026-08-04 hazard, and why this does not repeat it

A `java/lang/String` force-native arm was deleted because *"a method's behaviour
started depending on how many times its call site had run"*, and
`no_dispatch_path_mentions_java_lang_string_by_name` is the gate that keeps it
deleted. Consulting a dial at a **memoized** door recreates exactly that.

It is handled by asking **at dispatch, not at publication**:
`revalidate_cached_native` asks on every warm hit and answers `None` — the
eviction signal every caller already implements — so the site re-resolves
through a path that arbitrates; `populate_invoke_cache` refuses to publish an
armed class's bridge to a call site at all, so the resolution below caches the
bytecode target instead. Cold and warm therefore give the same answer for the
same triple, which is the property whose absence was the 2026-08-04 bug. The
`grow` case is the direct evidence: `threshold` is `24` on the first execution
and `24` on the four-hundredth.

### One trap that would have been a silent release-only hang

`invoke_or_native`'s superclass walk **holds the class-manager read guard**
across the whole walk, and `class_manager` is an `OrderedPlRwLock` whose plain
`read()` is not reentrant. A second `read()` there deadlocks against a queued
writer. Under lock-order enforcement that is a panic; **enforcement is compiled
out of a release build**, where the same code is a silent hang instead. The
dial's bytecode probe therefore takes `read_recursive()`, and
`step1_dispatch_has_code` keeps the plain `read()` it has always taken.

---

## 3. MEASURED — after

Same probe, same arming, fixed binary:

```text
[DIAL_DOOR_CENSUS] armed=true reached=2608  yielded=2608  leaked=0     (direct)
[DIAL_DOOR_CENSUS] armed=true reached=11785 yielded=11785 leaked=0     (warm)
[DIAL_DOOR_CENSUS] armed=true reached=1378032 yielded=1378032 leaked=0 (jit)
```

`reached` rises because real bytecode calls more `HashMap` methods than the
native did; `leaked` is zero at **every door in every case**.

The object model follows. Armed, four of six cases are now **byte-identical to
HotSpot on all nine witnesses**, including the two the unarmed VM gets wrong:

```text
                     tableCls        headCls              threshold(grow)  iteration
HOTSPOT              HashMap$Node[]  HashMap$Node         24               k0..k19
UNARMED              Object[]        AnonymousObject$4    12               k0..k19
ARMED, one door      HashMap$Node[]  HashMap$Node         12               k0        <- HYBRID
ARMED, every door    HashMap$Node[]  HashMap$Node         24               k0..k19
```

Read the third row. `size()=4`, four occupied buckets, real `HashMap$Node`
heads, a correctly-typed `Node[]` table — **and the map iterates one key**.
That is `H16-3`'s hybrid photograph with a number on it, and it is a map that
silently loses data. Neither a fully-native nor a fully-real `HashMap` can
produce it, which is why "`size()` disagrees with the iteration length" is a
better hybrid detector than any of the six witnesses this directory has reached
for.

---

## 4. MEASURED — acceptance: the unarmed arms did not move

**First, a correction to the brief.** `WORKER-1`'s acceptance line quotes
`105/105`, `104/105`, `65/65`. Those are `claude/jdk-only-mode-handoff-09b48c`'s
numbers. **On `origin/dev` the corpus is 104 and 64 classes and the baseline is
different**, so a lane cutting from `dev` that took the brief's numbers as its
acceptance bar would have reported a regression it did not cause. Control is a
**pristine rebuild of the same base commit** (`1fcc241e0`), not the brief.

| arm | scheduled | pristine `1fcc241e0` | with the fix | moved? |
|---|---|---|---|---|
| `CRATONVM_ARGS=--jdk-only` | 104 | **103 / 104** | **103 / 104** | no |
| `SUITE=all` | 104 | **98 / 104** | **98 / 104** | no |
| `SUITE=core` | 64 | **62 / 64** | **62 / 64** | no |

Identical **class-for-class**, not merely in the totals — the scalar alone
cannot tell "no change" from "one fixed, one broken":

```text
--jdk-only  both: RJdkOptionalShape
SUITE=all   both: RImmutableFactoryTypes RJdkOptionalShape RJdkProxyIface
                  RJdkFunctionCombinators RJdkEnumerations RServiceLoaderDoubleSource
SUITE=core  both: RImmutableFactoryTypes RJdkOptionalShape
```

**Two samples of every cell on both binaries**, and all four sets are identical
to the pair above. `TIMEOUT=600`, one `run.sh` at a time, control before fix so
a mid-run host change would show up as the control moving rather than as a
silent credit to the fix.

---

## 5. MEASURED — `H17-1`'s prediction is FALSIFIED, by its own falsifier

> `H17-1` predicts they go **down**, falsified if a fixed dial leaves `HashMap`
> at or above 81/104.

`CRATONVM_ARGS=--jdk-only CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap`,
104 scheduled:

| | pass | fail |
|---|---|---|
| pristine `1fcc241e0` (dial reaches one door) | **81** | 23 |
| with the fix (dial reaches every door) | **86** | 18 |

The pristine cell **reproduces `H0-4`'s `HashMap` 81/104 exactly**, which is what
makes the comparison a comparison rather than two unrelated numbers. **Three
samples of each cell**, run B-A-A-B interleaved so a drift in host load lands on
both binaries rather than on one: all three pristine runs are 81/104 with the
same 23 classes, all three fixed runs are 86/104 with the same 18. Zero rotating
flakes in six runs, so the sets below are the stable sets and not one sample.

**Eight vectors that the half-armed dial FAILED now pass**: `RFileTimes`,
`RSimpleTimeZoneRaw`, `RJdkStrict`, `RJdkCollections`, `RJdkForkJoin`,
`RJdkProcess`, `RJdkSecurity`, `RJdkEnvMap`.
**Three fail that did not**: `RMapResizeGc`, `RJdkBridge1`,
`RServiceLoaderDoubleSource`.

### What this means, and it is the point of the whole exercise

`H17-2` §7 argued the armed cells were **optimistic** — that a real retirement
misses at every door, so arming today leaves the warm doors on the native and
under-counts the damage. That reasoning is correct about the *mechanism* and
wrong about the *sign*, and `H17-2` said so itself: *"the direction of the error
is not knowable."* It is now known, for `HashMap`, and the direction is the
other one.

**A hybrid is worse than either uniform state.** Real `HashMap` bytecode and the
CratonVM native each maintain a self-consistent map; a run that mixes them per
dispatch maintains neither. Eight vectors were failing on the *mixture*, not on
the retirement.

So: **`H0-4`'s `HashMap` 81/104 is a floor on the retirement price, not an
estimate of it, and the correction goes the direction nobody predicted.** The
honest headline for `HashMap` is **86/104 against an unarmed 103/104 — retiring
`HashMap` costs 17 vectors.** ARGUED, and the reason this is not generalised
below: one family measured is one family measured.

---

## 6. Deliverable 3 — the witnesses

`H17` measured four of six blind and warned the fifth might be blind by the time
anyone read it. **On `dev`'s binary, three of those four discriminate.** The
blindness table is a property of the BINARY, not of the witness — one of them
went blind on another branch *because the VM was fixed there* — so it has to be
re-measured, not quoted.

MEASURED, `dev` at `1fcc241e0`:

| witness | HotSpot | unarmed | discriminates? |
|---|---|---|---|
| `table` array class | `HashMap$Node[]` | `Object[]` | **yes** |
| bucket head class | `HashMap$Node` | `cratonvm.synthetic.AnonymousObject$4` | **yes** (blind on `r8`) |
| `threshold` after a resize | 24 | **12** | **yes** — the native never rewrites it |
| `size()` vs iteration length | 4 vs 4 | 4 vs 4 | **only under a hybrid**, and then decisively |
| key / entry iteration order | `k0..k19` | `k0..k19` | only under a hybrid |
| `table.length` · occupied buckets · `modCount` · `size` field · `loadFactor` | — | — | no |

The probe is filed at **`regression-suite/probes/DialWitness.java`**, one case
per process, with the table and the two `H0-8` rules in its header (`H17-3` N1).

**The witness that cannot go blind is the VM-side one.** `[DIAL_DOOR]` on stderr
and `enforcement_dial.doors[]` in `--jdk-only-report` count armed `Bridge`
arrivals per door; `reached - yielded` is the price the dial is not charging,
and it does not depend on any Java-observable difference surviving.

---

## 7. Deliverable 4 — the census

> The census is a **deduplicated presence set with no counts**, and under
> `enforce` it records only the bytecode-won half. Say whether that should
> change.

**The "no counts" half: yes, and it is done — but not in the sink.** Making the
observation sink count would mean a class-manager read lock and a hierarchy walk
on every strict `Bridge` dispatch forever, which is exactly the cost its own doc
comment refuses, and it would still not answer the question anyone was asking.
The counts that were missing were **per door**, and those are a fixed-size array
of relaxed atomics with no dedup, no cap and no lock. They now ship in
`--jdk-only-report`:

```json
"enforcement_dial": {
  "scope": "java/util/HashMap",
  "reached": 2599, "yielded": 2599, "leaked": 0,
  "doors": [ { "door": "step1", "reached": 602, "yielded": 602 }, … ]
}
```

**The "only the bytecode-won half under `enforce`" half: no, that must NOT
change.** Recording a `bridge-ran-over-bytecode` row for a dispatch where the
bridge did not run would be a false statement, and this report's whole design is
about not making those.

**What was actually wrong is a third thing, and it was a false green.** An armed
report and an unarmed one were **byte-identical in every field a reader could
use to tell them apart** — and the armed one's emptier `violations[]` reads as
the better result. `"scope"` fixes that; it is written as `"off"` rather than
omitted, because an absent field is ambiguous between "unarmed" and "this binary
cannot answer".

**And a fourth, MEASURED here and not previously recorded:**
`record_native_shadow_ran_over_bytecode` — the recorder for the *native-won*
half of §1.4 — **also has exactly one call site, the same one**
(`resolve_step1_native`). So `refusals.interpreter_shadow_unenforced` read `0` on
an armed run while 890 dispatches ran the native over bytecode, and the
`native-shadows-bytecode` presence set is a **step-1-only census**. The per-door
`leaked` column is what makes that visible; extending the recorder to the other
doors is N1.

---

## 8. What I did NOT verify

* **One family.** Every armed number here is `java/util/HashMap`. `all`, a
  non-`java/util` prefix, and `H14-3`'s thirteen arms are unmeasured. `H0-4`'s
  other five prefixes and `H0-3`'s CHM eleven are **not** re-priced by this
  record — they need re-running on a fixed dial, and the sign of the correction
  may well differ per family, because the sign depends on whether that family's
  hybrid is self-inconsistent the way `HashMap`'s is.
* **The two remaining armed probe failures are real and unfixed.** Armed
  `DialWitness reflect` dies inside `entrySet()` iteration with a GC
  root-collection gap (`in_published_snapshot=false`, `site=checkcast`); armed
  `DialWitness jit` reports `size()=4` and iterates **zero** keys. Both are
  armed FAILURES, so both are real. They are `java.util` object-model defects
  (WORKER 2's subject) that the dial newly *exposes*; neither is caused by it,
  and neither is fixed here.
* **The three newly-red vectors** (`RMapResizeGc`, `RJdkBridge1`,
  `RServiceLoaderDoubleSource`) are counted, and only one of them is named
  (`RJdkBridge1`, §9). Same classification for all three: an armed failure is
  real, and it is the price of the retirement rather than a defect in the dial.
* **No throughput measurement.** The added cost on an unarmed path is one `Copy`
  policy read, one enum compare and one memoised bool per door; that is ARGUED
  from the code, not measured. Suite wall-clock was 1h42m–1h45m per three-arm
  set for both binaries, which is not a benchmark.
* **`--real-jdk` was not run separately.** `SUITE=all` and `SUITE=core` are the
  default policy, so the default configuration is covered by §4, but no arm ran
  with `--real-jdk` spelled explicitly.

---

## 9. MEASURED — re-verified on the merged state, against a NEW control

`dev` moved eleven commits while this lane ran (`1fcc241e0` → `ee4cdf528`), so
`origin/dev` was merged in and **every arm was re-run from scratch against a
pristine rebuild of the NEW tip**. A fix verified only against the base it was
cut from is a fix verified against a tree nobody will run.

| arm | pristine `ee4cdf528` | merged, with the fix | moved? |
|---|---|---|---|
| `CRATONVM_ARGS=--jdk-only` | 103 / 104 | 103 / 104 | no |
| `SUITE=all` | 98 / 104 | 98 / 104 | no |
| `SUITE=core` | 62 / 64 | 62 / 64 | no |
| armed `java/util/HashMap` | **81 / 104** | **86 / 104** | +5, as before |

Two samples of every cell, armed cells interleaved, all sets identical
class-for-class to the pre-merge measurement in §4 and §5. Ten suite runs in
total across the two rounds, and **no rotating flake appeared in any of them** —
so the numbers above are the stable sets, not one sample of a noisy one.

### The one newly-red vector, named rather than counted

`RJdkBridge1` — the `NativeKind::Bridge` census vector — fails armed at
`props()`, in the `Properties.stringPropertyNames()` / `propertyNames()` checks
(`src/RJdkBridge1.java:346-354`), with an `AssertionError`. `Properties` extends
`Hashtable`, but `stringPropertyNames()`'s own body builds a `HashMap` and a
`HashSet`, so a retired `HashMap` reaches it. That is a `java.util` object-model
divergence the dial newly *exposes*; it is an armed failure and therefore real,
and it is not caused by anything in this change. `RMapResizeGc` and
`RServiceLoaderDoubleSource` are the other two, undiagnosed.

### Trap 4 — no registration moved, checked rather than asserted

162 triples are registered more than once and only the `owns_slot: true` one is
reachable, so retiring the winner promotes the loser. **This change retires
nothing** — the dial declines at dispatch, it does not touch the registry — but
"my change cannot have done that" is the kind of claim this directory exists to
correct, so it was checked. `--dump-native-registry` on both binaries:

```text
natives rows                                    10 442  ==  10 442
(class, method, descriptor, kind, owns_slot)    identical
```

The dumps are **not** byte-identical: `invocations` differs. That is the
instrument, not the change. Three runs of each binary on the identical workload:

```text
p1 (pristine)   bridge invocations  2796  2794  2794
f3 (fixed)      bridge invocations  2794  2845  2798
```

The ranges overlap and both binaries hit 2794, so the per-slot invocation
counters are run-to-run noisy on this workload and a single-sample diff of them
means nothing. The identity set is the part that had to hold, and it does.
---

## 10. What actually landed, and on which tip

Landed on `dev` as `ee7bb87d4`. `dev` moved under this lane four times while it
was verifying, so the honest statement is not one pair of numbers but which
control each pair was measured against:

| base | control (pristine) | with the fix | armed `HashMap` |
|---|---|---|---|
| `1fcc241e0` | 103/104 · 98/104 · 62/64 | identical | 81 → 86 |
| `ee4cdf528` | 103/104 · 98/104 · 62/64 | identical | 81 → 86 |
| `20fcda31e` | 102/104 · 97/104 · 61/64 | identical but one flake | 80 → 85 |
| `07c705309` (landed) | — | 103/105 · 103/105 · 63/65 | 86/105 |

The corpus grew by one vector and `dev` closed five `SUITE=all` failures during
the same window, which is why the landed row's denominators and its `SUITE=all`
cell do not line up with the rows above it. **The delta is the claim, not the
absolute**, and the delta was controlled three times: unarmed identical
class-for-class, armed `+5`.

The landing tip was built, its report parsed rather than read
(`enforcement_dial.leaked == 0` over 2 617 armed reached dispatches), and
`cratonvm-types`, `enforcement_dial_door_tests` and `jdk_only_dispatch` are
green on it.

### Three gates were red on `dev` itself and are fixed in the same push

Not this lane's files, and not this lane's doing — MEASURED on a pristine
`origin/dev` checkout at `07c705309`, same failures, same names. A branch
landing on `dev` inherits them, so they are fixed here rather than left:

* `doc_citation_paths` — six lines in three published records carried a path
  into the unpublished internal tree (a link a public reader cannot follow),
  and one citation
  in `jit/src/x64/tests.rs` named a page that moved when it was retired out of
  `fixed-suite-bugs/tomcat/`. That one is **wrapped across two `//` lines**,
  which is why a single-line replace reported success and changed nothing.
* `flag_declaration_guard` — `CRATONVM_GPU_DUMP_PTX` and
  `CRATONVM_WAIT_SPURIOUS_MS` were read by code and declared nowhere, so each
  was served by a live `getenv` rather than the latched `VmFlags` snapshot.
  `WAIT_SPURIOUS_MS` is filed under `Group::THREADS` and **not** `Group::DBG`
  on purpose: DBG's own doc comment promises that no token in it changes a
  program's result save three named exceptions, and making an untimed
  `Object.wait` return without a `notify` plainly does.

### Not done, and why

`claude/jdk-only-mode-handoff-09b48c` could not take a `dev` merge from this
lane. The single conflict is in `native-io/src/nio_selector.rs`, between `dev`'s
2026-08-20 fix (*stop `Selector.open()` nulling `SelectorImpl.selectedKeys`* —
`SI_OPEN_FLAG` aliases a reference-typed field, so do not write an `Int` there)
and that branch's 2026-08-22 `concrete_base` rework. Both sides are plausibly
right on their own path, the older of the two is what closed a netty stall, and
the file belongs to WORKER 4. **Resolving it is a semantic call for that lane,
not a merge this lane should guess**, so it was aborted rather than pushed. The
dial work reaches that branch with any routine `dev` merge.
---

## 11. The nominations, closed — and the defect closing one of them found

### N3 — the two spellings are NOT interchangeable, and the VM's own advice is the trap

`H17-3` N3 asked for the two spellings to be "confirmed equivalent once". They
are not. MEASURED with `enforcement_dial.scope`, which is the VM's own report of
what it resolved rather than an inference from pass counts:

| spelling | resolved scope | reached |
|---|---|---:|
| `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap` | `java/util/HashMap` | 2 570 |
| `CRATONVM_LOADER=enforce-native-shadow=java/util/HashMap` | `java/util/HashMap` | 2 569 |
| `CRATONVM_LOADER=enforce-native-shadow` **(bare)** | **`all`** | **19 931** |
| `CRATONVM_ENFORCE_NATIVE_SHADOW=1` | `all` | 19 931 |
| `…=java/util/HashMap,java/util/Hashtable` (old) | both prefixes | 2 575 |
| `CRATONVM_LOADER=enforce-native-shadow=java/util/HashMap,java/util/Hashtable` | **refused to start** | — |

Three findings, in order of how much they can cost you:

1. **The bare token the VM's own warning prints means `all`.** Every armed
   measurement in this directory is prefix-scoped. A reader who follows the
   warning literally converts a one-family arming into a whole-VM arming — the
   3/46 collapse the scoping exists to avoid — and nothing in the output says
   the scope changed. That is not a migration; it is a different experiment.
2. **The new spelling cannot express a multi-prefix arming at all.** The group
   parser splits on `,` before the token's value is read, so the second prefix
   becomes an unknown `CRATONVM_LOADER` token. The VM **refuses to start**,
   which is the right failure — but it means `H17-1` §5's `HashMap`+`Hashtable`
   probe has no new-spelling form, and N3's "then update the pages together"
   cannot be carried out for the two-family pages.
3. Value-carrying single prefixes **are** exactly equivalent, so a page quoting
   one prefix can be migrated safely.

### The defect that experiment found in this lane's own work

The last row of that table is `all`, and it read **reached 19 931, yielded
19 437** — a gap of 494. The field this lane had named `leaked` was therefore
494, after §4 reported it as 0 and this record generalised that to "zero in
every case".

**A leak cannot appear when the scope widens** — every door consults the dial
unconditionally. So the number was never a leak. `reached − yielded` is the dial
being *asked* and answering *no*, and after the three cheap guards the only
remaining reason to answer no is that the shadowed method has no `Code` to yield
to. Armed for `java/util/HashMap` every reached triple has bytecode, which is
why `0` survived its first checks — a constant that is right for one scope and
wrong for the general case, read as a global invariant.

The field is now `declined_no_bytecode` and its comment says what it is and what
it is **not**. The question the wrong name implied — *does some door serve a
`Bridge` without asking?* — this counter cannot answer at all, because a door
that never calls `note_dial_door` is invisible to it. That is answered
statically, by the three source-witness tests, and the report now says so
instead of implying it has measured it.

### N5 — the contract, written at the flag

`H16-3` N2 and `H17-2` N5 both asked for the dial's contract to be written down.
It is now a doc comment on `jdk_only_enforce_shadow` in `env_cache.rs` — beside
the flag, where the reader who needs it is the one about to type it, not in a
record they must first find. It states what an armed run entitles you to say,
and the **three differences from a retirement that survive the fix**: no-bytecode
triples still run their native (the one direction that really is a floor), the
registration is still present so a retirement's loser-promotion hazard is
invisible, and any scope narrower than `all` produces a mixed heap by design.

### H17-2 N3 and H17-3 N1 — the instruments

`scripts/jdk-only-blast-radius.sh` carried a fourth caveat saying the dial reads
at one site and *"The number is a floor"*. Both halves were wrong: the one-site
half is fixed, and the direction was backwards — half-armed measured **more**
damage than fully-armed. It now carries the correction, and a **fifth** caveat
that is `H17-2` N3: the table of which witnesses are blind, so a reader
spot-checking a cell cannot pick one at random and get a false green.

`ChmConsistencyProbe.java` was already split one-case-per-process, so N1's first
half was done by another lane. Its *explanation* still named the one-call-site
dial as the live latch. Corrected — with the rule kept and re-justified, because
the dial was never the only per-process latch and a design that is correct only
while one named bug is absent is not a correct design.

---

## 12. `H0-4`'s six families, re-priced with a dial that reaches every door

This is the measurement the brief exists to make possible. `H0-4` swept six
collection prefixes on 2026-08-20 with a dial that reached one of fourteen
doors. Re-run with `scripts/jdk-only-blast-radius.sh` on `3e6d56ac4`,
`TIMEOUT=600`, one arm per prefix, armed alone:

| prefix | `H0-4` passed / 104 | now passed / 107 | failing set now (net of control) |
|---|---:|---:|---|
| `java/util/HashSet` | 103 | **105** | `RMapGcStress` |
| `java/util/Hashtable` | 101 | **104** | `RJdkBridge1` `RJdkEnumerations` |
| `java/util/LinkedHashMap` | 97 | **102** | `RForeignLayoutCollections` `RJdkServices` `ROverlaySystemGcStress` `RServiceLoaderDoubleSource` |
| `java/util/TreeMap` | 97 | **104** | `RJdkCollections` `RJdkJmx` `RJdkViews` |
| `java/util/concurrent/ConcurrentHashMap` | 93 | **104** | `RChmKeySetView` `RMapGcStress` `RMapResizeGc` |
| `java/util/HashMap` | 81 | **88** | 18 vectors, list in `blast.log` |

**Every family is cheaper, and the migration order changes.** `H0-4`'s net costs
ran `HashSet` 0 < `Hashtable` 2 < `LinkedHashMap` 6 < `TreeMap` 7 <
`ConcurrentHashMap` 10 < `HashMap` 22. The current failing-set sizes run
`HashSet` 1 ≈ `Hashtable` 2 < `TreeMap` 3 ≈ `ConcurrentHashMap` 3 <
`LinkedHashMap` 4 ≪ `HashMap` 18. **The middle of the order inverts**:
`ConcurrentHashMap`, which `H0-4` priced as the second most entangled family and
which `H0-3` gave a record of its own, is now among the cheapest — and
`LinkedHashMap` is the most expensive of the five non-`HashMap` families.

### What this delta may and may not be attributed to

**Three things changed between the two tables**, and honesty about that is worth
more than a clean-looking story:

1. the dial now reaches every door;
2. `TIMEOUT=600`, without which `RMapGcStress` is `rc=124` at a 120 s budget and
   scores as a failure (`H14-3` trap 7) — it drops out of the `Hashtable` and
   `LinkedHashMap` sets here while remaining in three others, which is itself
   evidence that it was two different things wearing one name;
3. the corpus grew from 104 to 107 vectors, and two of the new ones
   (`RChmKeySetView`, `RMapResizeGc`) land in these sets.

**So the row-to-row deltas above are NOT attributable to the dial alone**, and
this record does not claim they are. What IS a clean attribution is §4's
controlled pair: the same corpus, the same timeout, two binaries differing only
in the dial fix, `java/util/HashMap` **81/104 → 86/104**. The direction of that
one is the dial's; the size of the others is a joint effect.

The new table is nonetheless the number to quote going forward, because it is
the only one taken with an instrument whose premise holds.

### The instrument printed the stale caveat under the fixed table

Worth recording as a process finding rather than a footnote. The header comment
of `jdk-only-blast-radius.sh` was corrected first — one-site claim removed,
direction corrected, witness table added. Running it then printed, underneath
the corrected table:

```text
* The dial is read at ONE dispatch site (H7-1 N2). A real retirement acts
  at all of them, so every cell is a FLOOR.
```

A **second copy** of the same claim, in the `CAVEATS` heredoc the script emits.
Fixed. The reason it is called out: this is the one-fix-one-call-site shape, in
a file whose entire purpose is to stop people quoting stale numbers, found only
by **running** the instrument rather than reading it. A reviewer of the header
diff would have signed it off.
---

## 13. Two corrections against this record, both found by looking harder at its own claims

### 13a. "All fourteen doors" was an enumeration, not a proof — and it was short by two

This record says the dial reaches every dispatch door and that the census shows
no leak. The census cannot show a leak it cannot see, and §11 already says so:
*a door that never calls `note_dial_door` is invisible to it*. That was written
as a caveat. It was live.

MEASURED on `origin/dev` at `d702ecca5`, by grep rather than by counter:

```text
vm/src/runtime/interpreter/dispatch_virtual.rs   dial=0  census=0   forces natives
vm/src/runtime/interpreter/jit_bridge.rs         dial=0  census=0   forces natives
```

Both call `force_native_over_real_jdk_bytecode` and neither has ever asked the
dial. `dispatch_virtual.rs` additionally memoizes the decision into a per-entry
`force_native_cache` `OnceLock` — the 2026-08-04 drift hazard, in the one place
this lane did not look. **Lane WORKER 2 reached the same two files from the
other end** (`089329af7`, handoff branch): an armed `ConcurrentHashMap` taking
six `put`s of distinct keys and reporting `size=1`, calling five dropped stores
fresh inserts.

The armed report for that same shape on `dev` reads `reached=734 yielded=734
declined_no_bytecode=0` across five doors — **perfectly clean, and blind**.

**The fix this lane owes is not another door, it is the gate that finds doors.**
`every_force_native_file_asks_the_dial_or_is_exempt` (in `native_override.rs`)
scans every file under `vm/src` that calls the force helper and requires it to
either consult the dial or carry a written exemption. Counting cannot find a
missing counter; only a source scan can. The two files above are its initial
exemption rows, each with the reason and, for `dispatch_virtual.rs`, the commit
that retires the row. The list is asserted non-rotting: a row naming a file that
no longer forces anything fails the test, because a stale exemption reads as
coverage.

### 13b. `RTreeRangeGc` is not "a flake", and I called it one on one observation

§9 recorded it as *"a flake, and the proof is same-binary"* — the fixed binary
passed it in a `--jdk-only` arm and failed it in a `SUITE=all` arm nine minutes
later. The observation is real. The inference was wrong, and 18 single-vector
runs say why:

```text
--jdk-only            9 pass / 3 fail      a ~25% FLAKE
compatible (default)  0 pass / 6 fail      DETERMINISTIC
```

**The two arms run different modes.** What looked like one vector flipping was a
flake and a deterministic compatible-mode defect, with the arm boundary sitting
exactly between them. **A single same-binary flip is not evidence of flakiness
when the two observations differ in a mode flag** — and every "flake" attribution
in this record that rests on that one flip should be read against `WORKER-1-NOTE-1`.

What survives: the unarmed comparisons in §9 and §10 are unaffected, because
control and fix were compared *within* the same arm each time, and the vector
behaves the same way for both binaries.

### 13c. The `25-linux` blast-radius baseline is deliberately still not taken

The weekly job has been measuring a table and scoring nothing since it was
added, and the obvious close is to take the baseline it asks for. **Not yet, and
for a measured reason.** The sweep arms each prefix with `--jdk-only`, which is
the 25%-flake half; the baseline is keyed on the failing SET; so control and arm
would disagree about `RTreeRangeGc` roughly three runs in eight by chance alone,
and the job would report `REGRESSION`/`REPAIRED` on a vector nobody touched.

The workflow's own header cites `G89-1` — a ratchet red in blocking CI for five
days that "adjudicated nothing" — as its reason for being non-blocking. A gate
that cries wolf three weeks in eight fails identically. **A flaky vector must be
quarantined before a baseline exists, never after**, and the harness has no
quarantine mechanism today (`harness-uncounted.txt` is about check counts). That
is `WORKER-1-NOTE-1` N1 and it belongs to whoever owns `regression-suite/`.
### 13d. `jit_bridge.rs` was never a hole, and the list that said it was is now empty

§13a listed two undialled force files and implied both were holes. One was.
The other I had inferred from a grep, and reading the bind path says otherwise.

**MEASURED, by reading `jit::direct_native_helper`.** Under `--jdk-only` it
refuses to bind ANY native whose registry `NativeKind` is not `Intrinsic` —
§1.4's reviewed exception — and records the refusal. `HashMap.put`,
`HashMap.get` and `ConcurrentMap.get` are `Bridge`, and are refused today.

The dial's entire domain is **`Bridge` natives under `--jdk-only`**. That is a
strict *subset* of what the JIT bind path already refuses. So there is no
configuration in which the dial would yield a native the JIT would otherwise
have bound: **the hole cannot exist.**

What `jit_bridge.rs` actually does with the force helper is decide whether to
SEAL a caller out of tier-up. Missing the dial there makes an armed run seal
call sites it need not — a tier-up cost, in the safe direction, not a
correctness bug. Wiring it is an optimisation; and the natural place,
`registered_native_will_run`, also feeds interpreter dispatch, so it is not the
free edit it looks like. The row is now `permanent: true` with that reasoning,
and the exemption list carries a third field so a reasoned non-hole cannot be
counted as outstanding work. **A list that files both under one heading reads
as twice the remaining problem** — which is the exact failure this lane spent
its time finding in other people's tables.

**And the hole count is now zero.** `089329af7` reached `dev` while this was
being written, so `dispatch_virtual.rs` consults the dial and its row went. The
gate said so by name, unprompted, on the first run after the merge — the second
time it has done that on a different branch, which is the argument for a source
scan over a counter in one sentence. The bound is now `holes == 0`, so a new
unwired force site is a red test on its own rather than something a reader has
to notice.

Three of the four checks are proven falsifiable by running them: removing an
exemption goes red naming the file, pointing a row at a file that forces nothing
goes red, and flipping the surviving row's `permanent` flag to `false` trips the
zero bound. The assertion text says explicitly not to fix that by flipping the
flag back, because that is how a to-do becomes a permanent approval.
---

## NOMINATIONS

* **N1 — the native-won recorder has the same one-door defect the dial had.**
  `record_native_shadow_ran_over_bytecode` is called only from
  `resolve_step1_native`, so `interpreter_shadow_unenforced` and the
  `bridge-ran-over-bytecode` rows describe step-1 dispatches only. The cheap
  version costs a triple hash on `invoke_or_native`, which is the VM's hottest
  native path — measure before wiring it.
* **N2 — `invoke_or_native` has three dispatch sites with no §7 routing.** The
  array-type alias retry and both arms of the superclass walk call
  `safe_native_call` on a bare `find` result: no `dispatch_policy`, no
  `resolve_native_dispatch_wave1`, no `record_invocation`. This lane wired the
  dial to them and nothing else. The parent-has-BOTH-bytecode-and-a-native arm
  is a §1.4 inversion with no policy check at all.
* **N3 — re-price every armed cell in this directory.** `H0-4`'s six prefixes,
  `H0-3`'s eleven, `H14-3`'s thirteen and every `H15`/`H22` armed measurement
  were taken with a dial reaching 6% of dispatches. §5 shows the correction can
  go **either** way, so they cannot be adjusted on paper — they have to be
  re-run.
* **N4 — `WORKER-1`'s acceptance numbers are branch-local.** `105/105`,
  `104/105`, `65/65` are the handoff branch's; `dev` at `1fcc241e0` is
  `103/104`, `98/104`, `62/64`. Any lane cutting from `dev` needs a pristine
  rebuild of its own base as its control.
* **N5 — move to the supported flag spelling.** The VM still warns that
  `CRATONVM_ENFORCE_NATIVE_SHADOW` is superseded by
  `CRATONVM_LOADER=enforce-native-shadow`. Every measurement in this directory,
  including every one above, uses the old spelling. They should be confirmed
  equivalent once and then the pages updated together. (Restated from `H17-3`
  N3, still outstanding.)

---

## INDEX rows (for H0 to move)

```text
| WORKER-1 | the dial now reaches every door, and the hybrid was worse than uniform | LANDED |
| WORKER-1 §1 | MEASURED: 890 of 947 armed Bridge dispatches never asked the dial; 299 431 of 299 469 on a hot workload | |
| WORKER-1 §5 | H17-1's prediction FALSIFIED by its own falsifier: HashMap 81/104 -> 86/104 | |
| WORKER-1 §7 | record_native_shadow_ran_over_bytecode has the same one-call-site defect the dial had | OPEN |
```

---

## INDEX ROWS

`H17-3` N2: `INDEX.md` is shared and several lanes write concurrently, so these
are staged here for H0 to move rather than edited in directly.

- [WORKER-1](WORKER-1-the-dial-now-reaches-every-door-20260821.md) — `LANDED` ·
  **MEASURED, instrumented first — the enforcement dial reached 1 of 14 dispatch
  doors, and the hybrid it was pricing was WORSE than uniform.** A build with one
  counter per door, taken before any decision was changed, put a number on every
  ARGUED claim in `H17-2`: **890 of 947 armed `Bridge` dispatches never asked the
  dial** (299 431 of 299 469 on a hot workload), and three doors carried all of
  it — `invoke_or_native`, the warm invoke cache, and `populate_invoke_cache`.
  The reflective door `H17-2` §4 named is not one of them; it reaches the native
  *through* `invoke_or_native`, which is why one fix closed both. All fourteen
  doors now consult the dial and three source-witness tests keep them consulting
  it. **`H17-1`'s prediction is FALSIFIED by its own stated falsifier**: armed
  `java/util/HashMap` went **81/104 → 86/104**, not down. Half-armed is not a
  partial retirement but a state no configuration can otherwise reach — the
  witness is a `HashMap` answering `size()==4` over four occupied buckets while
  iterating one key. So every armed cell published before 2026-08-21 is void
  **in the pessimistic direction**, and `scripts/jdk-only-blast-radius.sh`'s
  "the number is a floor" caveat was backwards. Unarmed arms unchanged
  class-for-class against pristine rebuilds at three successive `dev` tips.
- [WORKER-1](WORKER-1-the-dial-now-reaches-every-door-20260821.md) §11 —
  `LANDED` · **the supported flag spelling is a trap, and closing that
  nomination found a defect in this lane's own instrument.** `H17-3` N3 asked for
  the two dial spellings to be confirmed equivalent; they are not.
  `CRATONVM_LOADER=enforce-native-shadow` **bare** resolves to scope `all`
  (19 931 reached) where every armed page in this directory means one prefix
  (2 570) — so following the VM's own deprecation warning literally converts a
  one-family arming into the whole-VM 3/46 collapse, silently. The multi-prefix
  form has **no** new-spelling equivalent at all: the group parser splits on `,`
  first, so the VM refuses to start. Measuring that also exposed
  `enforcement_dial.leaked`, added by this lane, as mislabelled: it read 0 armed
  for `HashMap` and **494** armed for `all`, and a leak cannot grow when the
  scope widens. It counts the dial answering *no* for want of concrete bytecode,
  is now `declined_no_bytecode`, and the report says what it is not. The dial's
  **contract** is now written at the flag (`H16-3` N2 / `H17-2` N5), including
  the three differences from a retirement that survive the fix.
