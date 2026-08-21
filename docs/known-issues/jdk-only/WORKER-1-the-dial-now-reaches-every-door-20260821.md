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
  `RServiceLoaderDoubleSource`) are not diagnosed, only counted. Same
  classification: an armed failure is real, and it is the price of the
  retirement rather than a defect in the dial.
* **No throughput measurement.** The added cost on an unarmed path is one `Copy`
  policy read, one enum compare and one memoised bool per door; that is ARGUED
  from the code, not measured. Suite wall-clock was 1h42m–1h45m per three-arm
  set for both binaries, which is not a benchmark.
* **`--real-jdk` was not run separately.** `SUITE=all` and `SUITE=core` are the
  default policy, so the default configuration is covered by §4, but no arm ran
  with `--real-jdk` spelled explicitly.

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
