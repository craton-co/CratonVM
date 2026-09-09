# The system `Properties`' real `map` is null, and it is what holds the `ConcurrentHashMap` retirement

**2026-09-09.** A §1.4 defect and the retirement it unblocks. The defect is one
null field; the reason it is worth a page is that it is the precondition two
separate places in the tree wrote down in August and neither could price.

---

## 1. The defect

JDK 9 moved `java.util.Properties`' storage out of the inherited `Hashtable`
slots and into a `ConcurrentHashMap` field named `map`. Every JDK 25
`Properties` body reads it directly — `getProperty` at `Properties.java:1145`,
`clone` at `:1526`, `store0` at `:920`.

The object `System.getProperties()` returns here is VM-built, and its `map` is
**permanently null**. Its entries live in the Rust side table in
`native-builtins/src/properties_sidetable.rs`, and every `Properties.*` native
in that file exists to make an object with a null `map` behave like a `Map`.

While the natives own every read, nothing is wrong. The moment any real
`Properties` bytecode runs against that receiver, it reads the null field:

```text
NullPointerException: Cannot invoke
  "java.util.concurrent.ConcurrentHashMap.get(Object)" because "this.map" is null
    at java/util/Properties.getProperty(Properties.java:1145)
    at jdk/internal/util/StaticProperty.<clinit>(StaticProperty.java:78)

NullPointerException: Cannot invoke "java.util.Map.size()" because "m" is null
    at java/util/Properties.clone(Properties.java:1526)
    at java/util/concurrent/ConcurrentHashMap.<init>(ConcurrentHashMap.java:863)
```

Two places already knew. `native-api/src/retired_shadow.rs`'s G60-1 section
holds `Properties.getProperty`'s two overloads and states the precondition in
these words — *"the native which builds the system `Properties` initialise the
real `map` field"*. The `java/lang/System.getProperties` registration in
`native-builtins/src/lib.rs` calls itself the root of the Properties/Hashtable
state-ownership cluster and names the same move: *"the cluster's first move is
to make THIS return a real `Properties` — real `<init>`, real `map` — not to
move a tag."*

Neither had a number for what it buys. That is §2.

---

## 2. What it blocks, measured

Provenance: control binary `a3855b8c7febaf7e`, built from `3e25a17f0` with a
clean tree, JDK 25.0.3+9 on Windows, `--jdk-only`. Every row is a signed
distance from HotSpot, `d(hs,armed) − d(hs,base)`; `y/r` is
`enforcement_dial` `yielded/reached`, so no row below is a dial that was never
asked.

`java/util/concurrent/ConcurrentHashMap` was adjudicated NOT retirable on
2026-08-30 (see
[`phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md`](phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md)
§3), on the strength of `MapViewsShadowSweep` dying at row 261 of 302 with 53
rows changed, because `java.util.Properties` delegates to an internal CHM and
every `Properties` view emptied out.

**That verdict is a property of arming CHM ALONE.** Armed together with its
user, it reverses:

```text
                        CHM alone                    CHM + java/util/Properties
MapViewsShadowSweep     53 diffs, DIED 261/302       0 diffs, 302/302   y/r=2386/2386
ChmShadowSweep          0 over 28 671 yields         0 over 28 654 yields
PropertiesShadowSweep   53 diffs, DIED 157/184       67 diffs, DIED 117/184
PropsOrderSweep         0                            9 diffs, DIED 15/24
```

`Properties` armed alone is LOAD-BEARING (7 failing corpus vectors, recorded in
that page's §10) because its real bytecode then delegates to a *native* CHM —
split brain one way. CHM armed alone is load-bearing because real CHM bytecode
sits under a `Properties` whose state is in a side table — split brain the other
way. Armed together both halves are real and consistent, and the probe that
rejected the retirement goes clean.

**Neither the sweep nor the adjudication could see this.** The Phase 2 sweep
armed 270 classes one at a time and then all 236 candidates at once; a pair was
never a scope it ran. Its own text warns that a union is a separate measurement
and never an inference — that warning applies in this direction too.

### What was left, and that it is one cause

The two residuals above are not 67 and 9 wrong rows. Both probes are
byte-identical to HotSpot up to their death point and then stop; `diff` renders
the missing tail as ordinary `<` lines, which is the trap
[`../../contributing/jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md)
§1 names under "check the ROW COUNT before reading the diff". They are one crash
each, and both crashes are the null `map` of §1.

### A bisect the record did not have

The dial's scope is a prefix, so `java/util/concurrent/ConcurrentHashMap$`
arms the nested view/iterator classes without the base class:

```text
scope = ConcurrentHashMap$   MapViewsShadowSweep    53, DIED 261/302   y/r=102/102
                             PropertiesShadowSweep  53, DIED 157/184   y/r=201/201
                             ChmShadowSweep        195, DIED  13/208   y/r=2/2
```

Arming MORE is clean where arming LESS is catastrophic: the whole class leaves
`ChmShadowSweep` byte-identical over 28 671 yields, the nested half alone kills
it at row 13. So **CHM is all-or-nothing**, a base-class-only retirement holding
the views is not available, and the entire 53-row `Properties` breakage sits on
the nested half at a fifth of the yields.

---

## 3. The fix

`native-builtins/src/properties_sidetable.rs` gains `replace_real_map`, and the
`java/lang/System.getProperties` registration calls it under `--jdk-only`.

* **Registration-time branch, one registration, two bodies.** §1.4's lever is
  registration; `NativeCallback` is a bare `fn` pointer that captures nothing
  and `NativeContext` exposes no policy accessor, deliberately. Same shape as
  the `Runtime.loadLibrary0` arm in `native-builtins/src/lang_system.rs`. A
  second `register` of the triple was rejected: it would add a row to the
  population `native-builtins/tests/duplicate_registration_gate.rs` freezes.
* **`--real-jdk` is untouched by construction** and keeps the null `map`.
* **Wholesale replace**, matching `replace_sidetable`: this runs on every
  `System.getProperties()` call against a cached singleton, so an additive store
  would leave a property removed by `System.clearProperty` visible to any body
  that reads the real map.

### The first cut was worse than the bug, and the probe is why that is known

It created the map, called `clear()` on it unconditionally, and returned early
if that failed — leaving the receiver holding a **non-null empty** map. That is
strictly worse than the null it replaced: real `getProperty` stops throwing and
starts answering `null`, which `StaticProperty.<clinit>` turns into
`InternalError: null property: java.home` — the 2026-07-14 regression the
cluster-root comment warns about, reached from the other direction.

`apps/probes/SysPropsRealMapProbe.java` is new here and is what caught it. It
asks only for shape — more than ten entries, a known key present, a clone
agreeing with its source — because the two VMs' property sets differ by
construction:

```text
                          HotSpot   unarmed --jdk-only   armed CHM+Properties
sys size > 10              true          true                 false
sys java.version present   true          true                 false
clone size > 10            true          true                 false
set then get               yes           yes                  null
```

Unarmed is perfect, because the natives answer. Armed, every real-bytecode read
gets an empty map, and **nothing else in the tree could see it** — a null map is
loud and an empty one is silent.

The invariant the function now keeps: **the real `map` holds the snapshot, or it
is absent.** An incomplete fill restores `None` so the failure stays loud, the
function returns the count written so "populated" and "silently empty" stop
being the same observable, and `CRATONVM_DIAG_PROPERTIES=1` reports the first
failure with its cause.

---

## 4. Status

The measurements in §2 were taken on `3e25a17f0`, a base **394 commits behind
`origin/dev`** with both edited files moved there. They are the DISCOVERY and
nothing was retired on them: §6 re-takes the question on the merged tree, as a
real retirement rather than a dial arm, and it is what the wave stands on.

No sibling lane had done this work: there is no `replace_real_map` on
`origin/dev`, and that file's own `stringPropertyNames` comment states the same
gap independently — *"our `Properties.<init>` native skips populating that CHM,
so the bytecode would yield an empty set"*.

### The binaries, so a row can be traced to one

```text
a3855b8c7febaf7e  3e25a17f0, clean tree            §2's dial arms (pre-merge)
47a38244b18dd32c  472c5c8d5, + replace_real_map    §5 and §5's Properties-alone table
5b41d60b6c2b6a81  472c5c8d5, both files at dev     §5's control (the fix removed)
d9a2a1b7d4e93fd5  + RETIRED_SHADOW_PHASE3_TRIPLES  §6's first probe-tree A/B
d241f0dc105f676b  + §7's write-back                the wave as it would land
```

`47a38244b18dd32c` is the CONTROL for both A/Bs in §6: same source, without the
table, so the retirement is the only variable.

## 5. The dial cannot see a call that starts inside a native

**MEASURED 2026-09-09 on the merged tree**, trial binary `47a38244b18dd32c` at
`472c5c8d5`, and it is the most reusable thing on this page.

`replace_real_map` fills the map with `ctx.invoke_virtual(chm, "put", ...)` —
a call that ORIGINATES IN A NATIVE. Armed two ways, the same binary and the
same function:

```text
scope = java/util/Properties          SysPropsRealMapProbe   10/10 rows match HotSpot
scope = ...ConcurrentHashMap,Properties                      6 of 10 rows FALSE
```

With `Properties` armed alone, real `Properties` bytecode reads `this.map`,
reaches the CHM **native**, and finds every entry — so the fill demonstrably
works. Add CHM to the scope and the probe's own reads become real CHM bytecode
over the real table, which is **empty**: the fill's `put` calls went to the
native the whole time. They returned `Ok`, so nothing failed and no diagnostic
fired.

**The dial declines at nine dispatch doors and a native-originated
`invoke_virtual` is not one of them.** This is a fourth difference from a real
retirement, on top of the three
[`phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md`](phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md)
§6 lists (per-prefix not per-triple; declines at dispatch rather than dropping
the registration; does not promote a superseded loser). It matters in BOTH
directions:

* it makes an armed run **understate** breakage for any class the VM calls from
  inside a native — those call sites keep the native and keep working;
* and it makes an armed run **overstate** breakage for a fix like this one,
  which is correct under a real retirement and cannot be shown to be by a dial.

A registration refused at `register` has no native to reach, so the same
`invoke_virtual` runs the real bytecode. **So this fix and the CHM retirement
have to be measured on one trial binary carrying the table, and can never be
accepted on a dial arm.** That is `../../contributing/jdk-only-lane-operations.md`
§7's *"the dial is not the retirement"* arriving from a new direction.

### What `java/util/Properties` alone now measures

Phase 2 called `java/util/Properties` LOAD-BEARING with seven failing vectors.
On the merged trial with the fix, armed alone:

```text
PropertiesShadowSweep   0 diffs, 184/184   y/r=1374/1374
PropsOrderSweep         0 diffs,   24/24
MapViewsShadowSweep     0 diffs, 302/302
ChmShadowSweep          0 diffs, 208/208
SysPropsRealMapProbe    0 diffs,   11/11
UtilCoverageSweep       ONE row: `spl Properties values` 4352 -> 64
```

The single residual is the un-retired CHM showing through: 4352 is
`CONCURRENT|NONNULL` and 64 is `SIZED`, so real `Properties.values()` is
delegating to `map.values()` and getting the VM's view object. It is a row the
union should close and the reason a Properties-only wave is not obviously right.

### The control, and it is not close

Binary `5b41d60b6c2b6a81`, built from the same merged tree with both edited
files reverted to `origin/dev` and nothing else changed, armed on the same
scope with the same driver:

```text
                        CONTROL (no fix)          TRIAL (with fix)
PropertiesShadowSweep   67 diffs, DIED 117/184    0 diffs, 184/184
PropsOrderSweep          9 diffs, DIED  15/24     0 diffs,  24/24
SysPropsRealMapProbe    NO OUTPUT AT ALL          0 diffs,  11/11
MapViewsShadowSweep      0 diffs, 302/302         0 diffs, 302/302
ChmShadowSweep           0 diffs, 208/208         0 diffs, 208/208
UtilCoverageSweep        2 diffs                  2 diffs (the same row)
```

The control dies at `PropertiesShadowSweep` row 118, `store escapes` — which is
`Properties.store0` at `Properties.java:920`, the third body §1 cites — and its
`SysPropsRealMapProbe` armed output is a ZERO-BYTE file, so that receiver is
unusable before the first row prints. **`replace_real_map` is what makes
`java/util/Properties` armable at all**, and Phase 2's LOAD-BEARING verdict on
the class was the null `map` and nothing else.

`UtilCoverageSweep` is the row to read carefully: 2 diffs on BOTH arms, so it is
the one thing the fix does not touch — the un-retired CHM showing through
`Properties.values()`, as diagnosed above.

## 6. The retirement, taken on a trial binary

The dial cannot adjudicate this (§5), so the union was built as a real
retirement — `RETIRED_SHADOW_PHASE3_TRIPLES` in
`native-api/src/retired_shadow.rs`, 185 triples over eight classes — and
measured on the binary that carries it.

### It is not inert, and that had to be checked first

`register_inner` refuses a `SyntheticStub` under `--jdk-only` **without
inserting it**, which is what makes a retired triple fall through to the real
bytecode. But a refusal is a RETIREMENT only when nothing already owns the
triple: `JdkOnlyViolation::SyntheticNativeRegistered` carries a `survivor` for
the case where an earlier registration keeps serving, and strict mode then runs
THAT native instead of the bytecode the policy asked for. **53 of these triples
are registered more than once**, so the question is live, and a green probe tree
would look identical either way.

```text
--jdk-only-report, ChmBulkSweep run, trial d9a2a1b7d4e93fd5:
  251 synthetic-native-registered refusals on the two prefixes
    0 of them carrying a survivor
```

Read that column before reading any probe row on a future wave.

### The whole probe tree, and the one thing it caught

115 probes, unarmed `--jdk-only`, both binaries built from this tree — the
control is the same source WITHOUT the table, so the only variable is the
retirement:

```text
                          d(hs,ctl)   d(hs,trl)   delta
SystemRuntimeObjectSweep     14          18        +4     <- real, and it is §7
VtHandoffProbe               14          10        -4     <- noise
the other 113                 =           =         0
```

`VtHandoffProbe` is **not** an improvement and is not claimed as one. Its rows
are thread counts — `polls that received a value |96|` against `|76|`, `threads
joined |510|` against `|512|` — and both arms are wrong against HotSpot in the
same way on every run. A probe whose noise floor is larger than the effect
cannot score a retirement in EITHER direction, and a negative delta is the shape
of the result you want, which is exactly why it is the one to distrust.

### The same tree again, with §7's fix in

Trial `d241f0dc105f676b`, same control, same 115 probes:

```text
JdkOnlyPlatformProbe          2           0        -2     <- noise
the other 114                 =           =         0
```

`SystemRuntimeObjectSweep` is back to the control's 14 and the two rows are
gone. `JdkOnlyPlatformProbe` is `VtHandoffProbe`'s family, one row, and the row
is `handoffs=64` against `handoffs=50` on a virtual-thread handoff count — it
read 0/0 on the run above, which is what says it is the probe moving and not the
change. **So the wave moves nothing across the probe tree that is not noise.**

### What the wave did NOT break, which is the load-bearing half

Every probe that rejected this retirement in August is byte-identical to
HotSpot on the trial:

```text
MapViewsShadowSweep   302/302     ChmShadowSweep         208/208
PropertiesShadowSweep 184/184     PropsOrderSweep         24/24
UtilCoverageSweep     161/161     SysPropsRealMapProbe     11/11
ChmBulkSweep           60/60
```

`ChmBulkSweep` is new here. The wave retires 185 triples and only 122 of them
were dispatched by any probe in the tree; the undispatched 63 were CHM's
bulk/parallel surface (`reduce*`, `search*`, `forEach*` with a parallelism
threshold) and most of `EntrySetView`/`EntryIterator`. Retiring a native that
nothing ever calls is a change no instrument can see, so the probe was written:
60 rows, every one chosen so the EMPTY answer and the right answer print
differently, because the failure mode of this particular retirement is a reader
left standing over a store that moved — which returns nothing, quietly, rather
than throwing. It took the measured share to 150 of 185.

Seven of those rows are Java SERIALIZATION, and they are there because the
registration comment for `native_chm_write_object` names this exact hazard:
*"the real JDK bodies walk/rebuild the `table` field that CratonVM's segmented
layout never populates, so a CHM round-tripped through ObjectOutputStream came
back EMPTY"*. The wave retires `writeObject` and `readObject`, which hands those
real bodies back the job — sound only if real `put` bytecode is what filled
`table` to begin with. Nothing else in the probe tree round-trips a CHM at all,
so the claim had no instrument until this probe.

```text
ser CHM size                8              ser CHM then put   9 9 0
ser CHM content             k0=0 .. k7=7   ser Properties     2 1 2
ser CHM class               java.util.concurrent.ConcurrentHashMap
ser keySetView              ConcurrentHashMap$KeySetView [x, y]
```

0 diffs on both binaries.

## 7. The real map was WRITE-ONLY, and the union is what exposed it

`replace_real_map` (§3) makes the field READABLE by real `Properties` bytecode.
Nothing made a write through that bytecode visible anywhere else — and once the
`Properties` natives are retired, that write is the only one there is.

```text
SystemRuntimeObjectSweep, control -> trial
  39  a write through getProperties is visible to getProperty   four -> null
  40  setProperties round trip                    yes/null/true -> null/null/true
```

Row 39: `System.getProperties().setProperty(k, v)` is real JDK bytecode running
straight into the `map`. `System.getProperty(k)` answers from the VM's Rust
store, which never hears about it.

Row 40 is the same defect one level up. `System.setProperties(p)` read its
argument's entries out of the SIDE TABLE, and a `Properties` built by real
`<init>` has no side table — its entries are in the real map. It installed an
EMPTY property set and reported nothing.

### The fix is the write half of the same bridge

`properties_sidetable.rs` gains `lookup_in_real_map`,
`store_property_in_real_map`, `remove_property_from_real_map`,
`snapshot_real_map` and `harvest_real_map`, shaped like their side-table twins
so the next mirror has an obvious place to go. All five are inert while the
field is null, so `--real-jdk` is untouched by construction. Two ordering
decisions carry the whole correctness argument:

* **Harvest before refill.** `replace_real_map` clears the map and refills it
  from the VM store, so `System.getProperties()` was itself the call that would
  erase a write made through the receiver. The harvest runs first, at the top of
  the `--jdk-only` `getProperties` body.
* **The map is the first opinion; the store is the fallback.** The reverse order
  also passes row 39 — it ADDS a key rather than changing one — while still
  masking every UPDATE through a held reference, which is the same defect with a
  quieter symptom. The tie-break is not a guess about which is rarer: on a real
  JDK there is exactly one store and it IS the `Properties` object,
  `System.getProperty` being literally `props.getProperty(key)`. This VM's store
  is the shim, so when the two disagree the object is the one telling the truth
  about what Java did.

The residual, stated rather than papered over: a property written into the VM
store from RUST (`set_system_property`, no `System.setProperty` involved) is
masked by an older value in the map until the next `System.getProperties()`
refills it, and a `remove` performed directly on the receiver is invisible to
the harvest, which is a union and not a reconciliation. Closing the second
needs a generation counter on the map.

## 8. The gate set

The probe tree is not the gate set. These are run on `d241f0dc105f676b`, the
release build of the tree as it would land.

### `--jdk-only` corpus

```text
TIMEOUT=600   REGRESSION SUITE: 132 passed, 0 failed
default       REGRESSION SUITE: 131 passed, 1 failed ( RMapGcStress rc=124 )
```

`rc=124` is the harness killing the VM, not the VM failing, and it is a KNOWN
trap: `regression-suite/harness-vmfault.sh` hardcodes the explanation —
*"RMapGcStress needs 233 s against the default 120 s budget"*. At `TIMEOUT=600`
the whole corpus is clean.

**It is also the vector this wave would be most likely to slow down** — a map
GC stress test, on a binary that has just handed every `ConcurrentHashMap`
operation back to real bytecode — so it was timed rather than waved through, and
the answer is that this host cannot measure it:

```text
              control   trial
  pass 1        146 s    179 s
  pass 2        334 s    340 s
```

The same binary moved 146 s → 334 s between passes. The within-arm spread is an
order of magnitude larger than the between-arm gap, so **no throughput claim is
available from this vector on this host**, in either direction. Pass 1 alone
would have read as a 23% regression and it is not one. Anyone wanting that
number needs a quiet host and the ABBA interleave, not two sequential runs.

### `SUITE=all`, the COMPATIBLE-mode arm

```text
REGRESSION SUITE: 132 passed, 0 failed
  COUNTS: 132 of 132 SCHEDULED vectors passed; 0 list/coverage errors
```

This arm is the one that answers a question the `--jdk-only` arm cannot: the
re-tag to `SyntheticStub` is NOT gated on compatibility mode, so it moves
compatible-mode census numbers (§8's ratchets, the kind map) even though
`register_inner` refuses only under `--jdk-only` and
`resolve_native_dispatch_wave1` throws the kind away outside it. 132/132 is what
says that reasoning is right rather than plausible: 185 triples changed KIND in
this arm's registry and not one vector noticed.

### `SUITE=core`

```text
REGRESSION SUITE: 92 passed, 0 failed
```

### `cargo test`

```text
cratonvm-types                                    606 + 10 targets, all ok
cratonvm-native-api                               356 + 10 targets, all ok
cratonvm-native-builtins --tests                 4204 + 10 targets, all ok
                         --features management     4236 + 10 targets, all ok
                         --features synthetic-jdk  4380 + 10 targets, all ok
```

### The census, which is the campaign's own scoreboard

The `--jdk-only` corpus run ends with a union census over every per-vector
report, and running it on BOTH binaries is what turns this wave into a number
against the actual goal — *remove all synthetic bridges shadowing real
bytecode*:

```text
                                  control      trial      delta
  native-won (the defect)           1496        1414        -82
  bytecode-won                       496         458        -38
  synthetic-native-registered       1645        1858       +213
  interpreter_shadow_unenforced    11385       10745       -640
```

**A retired triple leaves the shadow census entirely**, which is why both
outcome columns fall: with no registration there is nothing to win or lose, and
120 triples stop being counted either way. `synthetic-native-registered` is the
other side of the same fact — those are the refusals, and it rises by 213.

Two things this does NOT say. **It is 82, not 185.** `native-won` counts triples
the CORPUS dispatched and the native won; most of the wave is not in that
population at all, which is the same gap between corpus and probe that
precondition 4 is written about. And the control run lost `RMapGcStress` to the
harness timeout (131 reports against the trial's 132) under the concurrent load
it was measured with, so its numbers are if anything an UNDERCOUNT and the real
reduction is at least this large.

`saturation: none` on both runs is the line that makes any of it readable:
every bounded collection reported `truncated: false`, so these are totals rather
than floors.

**1414 native-won remain.** That is the scale of what is left, on one workload,
after the largest single wave this table has taken.

## 9. What this does NOT claim

* Not that the wave is landed. 115 probes is the tree, and it is NOT the gate
  set: `regression-suite/run.sh` in its three arms, the `cargo test` crates, the
  `25/linux` kind-map baseline and the stub ratchets all still have to be run and
  amended, and this page will say so when they have been.
* Not that the 185 triples are equally evidenced. 150 were dispatched by a probe
  that measured them; **35 were not**, and they are retired on the structural
  argument recorded in `RETIRED_SHADOW_PHASE3_TRIPLES` — retire CHM's writers
  and a surviving native reader answers from a store nothing fills any more.
  That argument is good, and it is not a measurement. The corpus is their
  instrument.
* Not that the object model is now coherent. §7's fix makes the real map
  READ-WRITE for the paths that go through `java/lang/System`; a `remove`
  performed straight on the receiver is still invisible to the harvest, and
  `java/util/Hashtable` — which `Properties` extends — is still a `Bridge` over
  a side store.
* Nothing about `--real-jdk`'s BEHAVIOUR, which this change cannot reach: the
  re-tag is not gated on compatibility mode, so it moves compatible-mode census
  numbers, but `register_inner` only refuses under `--jdk-only` and
  `resolve_native_dispatch_wave1` ignores the kind entirely outside it.
