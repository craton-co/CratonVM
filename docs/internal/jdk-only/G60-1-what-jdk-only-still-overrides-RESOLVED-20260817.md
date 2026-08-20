# G60-1 — what `--jdk-only` still overrides, counted by the mode itself

**Status:** **RESOLVED 2026-08-17.** All four nominations answered by
measurement; two of the record's own readings corrected; three code defects the
record was standing on fixed. Retired here from
`docs/known-issues/jdk-only/`.

> **Read §0.1 first if you have read the original.** Two numbers in it were
> right and one reading of them was wrong, and the wrong reading is the one N4
> was built on.

**Provenance.** Linux, Azure `vm1`, JDK 25.0.4+7
(`/data/toolchain/jdk-25`, Temurin). Branch
`fix/jdk-only-g60-residuals-20260817`, worktree `/data/cvm-g601-20260817`, from
`0010e134d`. Three binaries, all `cargo build --release -p cratonvm-cli`,
`CARGO_TARGET_DIR=/data/g601-target`:

| binary | tree | used for |
|---|---|---|
| `/data/cvm-g601.bin` | `0010e134d` + the report fields only | the BEFORE arm |
| `/data/cvm-g601-trial.bin` | + `ArrayList.get`/`size` **and** `Properties.getProperty` ×2 in the retirement table | the two per-triple trials |
| `/data/cvm-g601-cand.bin` | the landing change | the AFTER arm |

Probes added to the tree by this lane, all three run in four arms (HotSpot,
`--real-jdk`, `--jdk-only`, `--jdk-only` + instrument):
`probes/JdkOnlyValuesViewProbe.java`, `probes/JdkOnlyPropsShadowProbe.java`,
`probes/JdkOnlyTomcatCensusProbe.java`.

---

## 0. What the original said, and what survives

The original's counts all reproduce, on a different commit and a different
platform. One `RJdkReflBox` run, `--jdk-only --jdk-only-report`:

```text
violations                        1422      reproduced exactly
  synthetic-native-registered     1341      reproduced exactly
  native-shadows-bytecode           81      reproduced exactly
    bridge-ran-over-bytecode        58      reproduced exactly
    bridge                          21      reproduced exactly
    check-override-name              2      reproduced exactly
compatibility_classes                0      reproduced
synthetic_stub_invocations           0      reproduced
```

The two zeros are still the headline and are still good. What changed is what
the 81 means.

### 0.1 The correction — 23 of the 81 are §1.4 WORKING, not §1.4 left over

The original wrote:

```text
  21  bridge                     registered over bytecode; not observed running
```

and nominated all 21 in N4 as *"neither safe nor unsafe today — they are
unmeasured, which is a different and worse state than either."*

**A `bridge` row in this sink is recorded on the YIELD path.** It is written by
`record_native_shadows_bytecode`, whose two call sites in
`vm/src/vm/vm_exec.rs` are step 3 (`method.code().is_some()` → return
`DispatchDecision::Bytecode`) and the `NativeKind::Bridge if bytecode_available`
arm, whose own comment reads *"The bridge lost, so this is the other half of the
§1.4 observation."* Those 21 rows are the identities behind
`refusals.interpreter_bytecode_preferred`, which the same run reports as **92
events**. They are the most measured rows in the file: each is a dispatch where
strict mode sent the call to the real JDK bytecode.

The row said `bridge` because that field is `NativeKind::as_str()`, and nothing
in the row said which side won. Both outcomes wore
`kind: "native-shadows-bytecode"`, and the row's own `summary` said *"native
shadows bytecode of …"* for both — so the report's human-readable text asserted
the violation on rows where the violation had been prevented. **That is a defect
in the report, not a misreading by the reader**, and it is fixed (§1.1).

With outcomes emitted, the same run splits:

```text
  81 native-shadows-bytecode rows
     58  outcome=native-won      the residue -- these are what is left
     23  outcome=bytecode-won    21 `bridge` + 2 `check-override-name`
```

So the original's *"The 81 are what is left"* over-counts by 23. **58 is what is
left.**

### 0.2 The second correction — the two populations are not disjoint

Eleven triples appear under BOTH outcomes in the SAME run:

```text
  java/lang/Module.getDescriptor()Ljava/lang/module/ModuleDescriptor;
  java/lang/foreign/Arena.allocate(J)Ljava/lang/foreign/MemorySegment;
  java/lang/invoke/MethodHandles.byteArrayViewVarHandle(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;
  java/lang/reflect/Field.get(Ljava/lang/Object;)Ljava/lang/Object;
  java/util/ArrayList.get(I)Ljava/lang/Object;
  java/util/ArrayList.size()I
  java/util/Collections.emptyList()Ljava/util/List;
  java/util/HashMap$KeyIterator.hasNext()Z
  java/util/HashMap$KeyIterator.next()Ljava/lang/Object;
  java/util/concurrent/ConcurrentHashMap.get(Ljava/lang/Object;)Ljava/lang/Object;
  java/util/concurrent/ConcurrentHashMap.putIfAbsent(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;
```

The digest is keyed on the tag string precisely so this can be recorded, and the
fact it records is worth more than either row alone: **for these eleven, the real
JDK bytecode already ran, in this process, and the vector still passed 107 of
107 checks.** That is not proof a retirement is safe — the yielding path and the
winning path can have different receivers, which is exactly what N2 turned out
to be about — but it is a much better starting point than "unmeasured", and it
is where two of the eleven were retired from.

That leaves **10 `bridge`-only** triples plus the 2 `check-override-name` rows:
12 triples this vector observed ONLY on the yield path. Nine of the ten are
`java/lang/foreign` and `java/lang/Object.clone`.

---

## 1. Three code defects, fixed

### 1.1 A violation row could not say which side won

`types/src/error.rs`. `JdkOnlyViolation::NativeShadowsBytecode` is produced by
four sites with two opposite outcomes and only a free-text `native_kind` to tell
them apart, and every piece of text it generated was written for one of them:

* `reason()` ended *"concrete bytecode wins under --jdk-only"* — the policy's
  promise, asserted on the rows where the policy had not held.
* `summary()` said *"… native shadows bytecode of …"* with no outcome at all.
* `to_json()` emitted no outcome, so a consumer had to hard-code the tag string.

Now: `NATIVE_SHADOW_RAN_TAG` moves to `types` (the crate that writes the text),
`shadow_outcome()` derives `native-won` / `bytecode-won` from it, `to_json()`
emits `"outcome"`, and `summary()` appends `[native-won]` / `[bytecode-won]`
after the existing prefix so no existing grep or sort key breaks. The polarity is
deliberate: `native-won` is the single closed spelling, so a new producer that
does not think about outcomes is classified `bytecode-won` — understating a
violation rather than inventing one.

### 1.2 A truncated violation list was indistinguishable from a complete one

`vm/src/vm/vm_init.rs`, `vm/src/vm/vm_exec.rs`, `vm-cli/src/main.rs`. §4 of the
original stated the hazard and left it as an instruction to the reader: *"a
saturated buffer looks exactly like a complete one from the JSON. Anyone running
this on an application should check the count against the cap."*

The report now carries a sibling object:

```json
  "observation_sink": { "recorded": 81, "cap": 256, "saturated": false }
```

`saturated` is **not** `recorded == cap` — a run whose last distinct observation
is the 256th fills the sink and drops nothing. It is the flag
`offer_native_shadow_observation` sets when an offer finds no room, so it means
*something was dropped*. It also condemns a counter, not just a list:
`refusals.interpreter_shadow_unenforced` stops advancing at saturation, because
the hierarchy walk that discovers a shadow is skipped once the sink cannot learn
one. The launcher prints a warning on the same line as the violation count, since
the whole failure mode is a reader who stops at that number.

**This fired on the first application workload it was pointed at.** See §4.

### 1.3 The cap made N3 unanswerable, not just unreliable

`CRATONVM_NATIVE_SHADOW_SINK_CAP` (default 256, ceiling 65,536, ignored if
unparseable or zero) raises the sink for a census run. Not a new default: the
sink is a process-global `Vec` every strict dispatch can push to, and raising it
for every strict run would be a change to the shipping configuration rather than
to the instrument. The 512-slot dedup filter is deliberately NOT resized with it —
above 512 each new triple costs one extra cold-path mutex acquisition, and
`Vec::contains` keeps the buffer correct regardless.

**The name is not the first one tried, and the guard that rejected the first one
was right.** It was `CRATONVM_JDK_ONLY_SHADOW_CAP`, and
`flag_groups::tests::jdk_only_adds_no_environment_variable` refuses any DECLARED
name containing `JDK_ONLY`: `--jdk-only` is a per-invocation policy (contract §9)
and a second env-var spelling of it would be a way to half-enable strict mode
from a parent shell. `CRATONVM_ENFORCE_NATIVE_SHADOW` was renamed out of exactly
that trap on 2026-08-06 and its `INVENTORY` comment records why, so this is the
second knob in the same area to take the same rename — worth noting because the
guard fires at `cargo test -p cratonvm-types`, not at the point of writing.

Declared rather than read through bare `getenv`, and that distinction is load
bearing rather than tidy: `flags::runtime_var` serves declared names from the
latched snapshot and falls through to a live `getenv` for undeclared ones, so an
undeclared knob is unreachable from `CRATONVM_DBG=native-shadow-sink-cap` and
invisible to `with_thread_overrides`. `types/tests/flag_declaration_guard.rs`
scans for the literal and would have failed the build otherwise. The generated
`docs/config/flag-inventory.md` and `docs/flag-tokens.md` were re-rendered by
their own script, not by hand.

---

## 2. N1 — `ArrayList.get` / `size`: RETIRED, and the recorded hold was wrong

N1 asked for these two and said the table "records no reason". It did record
one — `Map.values()` is answered as a `java/util/ArrayList` with its source map
stashed in a trailing capacity slot, so `size`/`get` are the view's
implementation and retiring them freezes it, citing H2
`TestAlter.testAlterTableDropIdentityColumn`. **The reason is stale and was also
structurally misapplied.**

**Structurally.** A retirement re-tags a registration `SyntheticStub`, and a
`SyntheticStub` registers and dispatches normally in `Compatible`. This table
cannot change Compatible behaviour. The H2 vector it cited is a JDBC test and
`java.sql` is unloadable under `--jdk-only` (APP-READINESS-20260812.md §0, family
A), so that vector cannot reach strict mode at all. The hold imported a hazard
from the one mode the table provably does not touch.

**By measurement.** No map family answers `values()` with a
`java/util/ArrayList` on this tree, in either mode. Eleven carriers — every
receiver `native_map_values` is registered on, plus the `java/util/Map` interface
door — agree with HotSpot in all four arms:

```text
  HashMap             java.util.HashMap$Values
  ConcurrentHashMap   java.util.concurrent.ConcurrentHashMap$ValuesView
  TreeMap             java.util.TreeMap$Values
  LinkedHashMap, Hashtable, Properties, EnumMap, IdentityHashMap,
  WeakHashMap, values() through java.util.Map    all the real JDK view class
```

`values` is not on `force_native_over_real_jdk_bytecode`'s list, so real
bytecode answers it and the stashed-carrier path is unreachable through it.

### 2.1 The instrument mattered, and the class dial got it wrong

`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/ArrayList` — the dial the 2026-08-12
wave was accepted with — turns the probe **RED**:

```text
  Exception in thread "main" java/util/ConcurrentModificationException
    at java/util/ArrayList$Itr.checkForComodification(ArrayList.java:1096)
    at java/util/ArrayList$Itr.next(ArrayList.java:1050)
    at JdkOnlyValuesViewProbe.valuesViewIteration(JdkOnlyValuesViewProbe.java:133)
```

That is the dial yielding `iterator()` as well, because it cannot go finer than a
class name — the *"read arm A as an UPPER BOUND"* warning in
P2-COLLECTIONS-SHADOWS-20260812.md §3.5 coming due. The per-triple instrument is
the table itself, so a trial binary was built with exactly these two entries:

```text
  probes/JdkOnlyValuesViewProbe.java, 42 checks
    HotSpot                             42 pass
    --real-jdk                          IDENTICAL
    --jdk-only  (before)                IDENTICAL
    --jdk-only  + these two retired     IDENTICAL
    --jdk-only  + class dial armed      DIED: ConcurrentModificationException
```

The probe includes H2's own shape deliberately: a `ConcurrentHashMap.values()`
captured before any entry exists, read back with `size()` as the **first** view
method called after the mutation. Every other section touches `contains` or
`iterator` first, and if those re-sync a stashed view then a later `size()` reads
an already-correct field and cannot tell a retired native from a live one.

### 2.2 CORRECTION, same day: those four were held on a misread instrument

The paragraph that stood here said `iterator`, `toArray`, `isEmpty` and
`contains` were held because "the `ConcurrentModificationException` above" showed
at least one of them load-bearing. **That was wrong, and §2's own evidence
contradicted it** — the CME requires the values view to BE an `ArrayList`, and §2
says, measured over eleven carriers, that it is not.

Re-running the carrier lines UNDER THE DIAL (they did not exist when the dial arm
was first run) settles it: the carriers are identical in `--real-jdk`,
`--jdk-only` and `--jdk-only` + dial. The real `ArrayList$Itr` came from
somewhere else:

```text
                        HotSpot                          CratonVM, EVERY mode
  values()              java.util.HashMap$Values         java.util.HashMap$Values
  values().iterator()   java.util.HashMap$ValueIterator  java.util.ArrayList$Itr
  keySet().iterator()   java.util.HashMap$KeyIterator    java.util.HashMap$KeyIterator
```

`register_interface_natives` answers `java/util/Collection.iterator` with a
snapshot `ArrayList`, and that snapshot's `modCount` is not the map's. Arm the
dial and real `ArrayList$Itr.next()` starts checking it. The dial was measuring
**the interface door** — the one `retired_shadow.rs`'s "the door this table
cannot close" section warns about — and this record attributed it to
`java/util/ArrayList.iterator`.

**All five registrations were then retired on the per-triple instrument**, which
is the only one that can answer the question:

```text
  five triples, seven registrations, all [JDK-ONLY-REFUSED] — no inert entries
  probes/JdkOnlyValuesViewProbe.java, 67 checks   IDENTICAL to HotSpot
  --jdk-only  corpus   98 passed / 2 failed of 100   verdict-neutral
  SUITE=all   corpus   93 passed / 7 failed of 100   verdict-neutral
  registry census      7 rows bridge -> synthetic-stub, 0 added, 0 removed
```

So N1 closes on **seven** `java/util/ArrayList` triples, not two. What is still
held is the ITERATOR CLASS (`ArrayList$Itr`), which is a different receiver with
its own state and has had no trial.

**And the door defect is real.** `map.values().iterator()` is not fail-fast in
either mode — a structural modification mid-iteration throws
`ConcurrentModificationException` on HotSpot and nothing here. Filed as
`jdk-only/G63-1-the-values-view-iterator-is-not-fail-fast-20260817.md` with the
three-arm transcript; retiring these five neither causes nor fixes it, measured
both ways. It is the one thing in this lane that a 100-vector corpus could not
see, and it was found only by disbelieving a measurement in this file.

## 3. N2 — `Properties.getProperty`: ASKED, ANSWERED, and NOT retired

N2 offered a hypothesis — that the side table backs `System.getProperties()`
interop — and asked for it to be settled either way. It is correct, and the
mechanism is specific. JDK 9 moved `Properties`' storage to a
`ConcurrentHashMap` field named `map`; JDK 25's `getProperty` reads it directly.
The `Properties` object `System.getProperties()` returns here is VM-built and
never gets that field. With both overloads retired in the trial binary:

```text
  new Properties() + setProperty / load / put / remove / defaults chain
                                            36 checks, HotSpot-identical
  System.getProperties().getProperty("java.home")
      NullPointerException: Cannot invoke
      "java.util.concurrent.ConcurrentHashMap.get(Object)" because "this.map" is null
        at java/util/Properties.getProperty(Properties.java:1145)
```

So **both halves of N2's question are answered, and they are both yes**: the real
bytecode IS already correct on every axis G55-1 fixed by hand — every ordinary
`new Properties()` line above is HotSpot-identical, because the real constructor
initialises `map` — and the native is still load-bearing, for exactly one
receiver. The precondition for retiring these two is therefore **not a property
of `getProperty` at all**: it is that the native which builds the system
`Properties` initialise the real `map` field, or construct through the real
constructor. Named in `retired_shadow.rs` and guarded by
`properties_get_property_is_held_for_the_system_receiver`, so the next lane that
reasons "the real bytecode is String-keyed JDK code and correct by construction"
— which is true — does not retire them on that basis.

Also worth recording against the class dial, because it is the arm a future lane
will reach for first: `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/Properties`
produces **three** mismatches (`setProperty` loses its previous-value return,
`remove` does not take effect on the readers) and never reaches the
`System.getProperties()` line. The class dial and the per-triple table disagree
about what is broken here, and only the table is the change anyone would land.

## 4. N3 — the census against an application: RUN, and the sink saturated

Embedded Tomcat 12, composed classpath (35 jars + `output/classes`,
APP-READINESS-20260812.md §1), booting on an ephemeral port, serving one GET
through a real `HttpURLConnection`, taking a 404, then `stop()` and `destroy()`.
**Verdict-identical to HotSpot in both arms, `--jdk-only` included** — which is
APP-READINESS §0's claim, re-measured on Linux.

```text
  observation_sink   { recorded: 256, cap: 256, saturated: TRUE }
  violations         1605   1341 synthetic-native-registered
                            5    compatibility-class-requested
                            259  native-shadows-bytecode  (188 native-won, 71 bytecode-won)
  counts             boot_image 1385  application 397  generated 44  compatibility 0
                     bridge_invocations 298,355   intrinsic 28,050   synthetic_stub 0
  refusals           interpreter_bytecode_preferred 8,070
                     interpreter_shadow_unenforced 208   <- frozen at saturation
                     jit_fastpath_admissions 4,487   jit_direct_native_binds 10
```

**188 distinct `native-won` triples against the vector's 58** — and that 188 is
itself a floor, because the sink filled. §4's first caveat is confirmed
quantitatively: one vector is not the population, it is about a third of it on
the smallest real application available.

The `native-won` distribution is the useful part, because it says where a
migration's remaining work actually is:

```text
   67  java/util                  17  java/util/concurrent      5  java/util/concurrent/atomic
   45  java/lang                  12  java/io                   5  jdk/internal/misc
    5  java/lang/ref               3  java/lang/invoke          3  java/lang/reflect
    3  java/net                    3  jdk/internal/util         3  com/sun/…/xerces/…/impl
```

`compatibility_classes: 0` still holds — nothing was fabricated — while five
`compatibility-class-requested` violations were RECORDED. That is
APP-READINESS §5's lazy-fabrication caveat visible in a report for the first
time: the classes were asked for and refused, which is the mode working, and the
bucket count is not the way to find out that they were asked for.

## 5. N4 — the 21 unobserved rows: they were never unobserved

Answered in §0.1 and §0.2, and the answer is that the nomination rested on a
misreading the report invited. Eleven of the 21 name a triple that also ran over
bytecode in the same run; the other ten, plus the two `check-override-name` rows,
are triples this vector observed only on the yield path. None of the 21 is
unmeasured. Two of the eleven — `ArrayList.get` and `ArrayList.size` — are
retired by this change, and the census having already run their real bytecode is
part of why that was worth trying.

## 5.1 The census after the change, for the arithmetic

Same vector, same command, candidate binary:

```text
                        before   after
  violations             1422     1420
  native-shadows-bytecode   81       77
    native-won              58       56
    bytecode-won            23       21
  observation_sink   recorded 81 / cap 256 / saturated false   ->   77 / 256 / false
```

Exactly −4: `ArrayList.get` and `ArrayList.size` each lose BOTH of their rows,
because each was appearing under both outcomes (§0.2). `RJdkReflBox` still passes
all 107 checks.

## 6. Gates — RUN on Linux, and what they say

All five `25/linux`-keyed gates were run on the same host and JDK image, on the
BEFORE and AFTER binaries, so every delta below is measured rather than derived.
This is the first lane to run them on Linux; `regression-suite/bridge-ratchet.sh`
and `scripts/jdk-only-kind-map.py` exit 2 ("REFUSING") off Linux, which is why
`retired_shadow.rs`'s `java/io/PrintWriter` hold says a Windows lane "cannot even
discover whether it got the numbers right".

| gate | frozen baseline | BEFORE (`0010e134d`) | AFTER | this change |
|---|---|---|---|---|
| `stub_ratchet` NO_MANAGEMENT | 1277 | 1300 | **1302** | **+2** |
| kind map `25/linux`, rows changed | 0 | **54** | **56** | **+2**, both named |
| `bridge.without_acc_native` | 8912 | 9262 | 9260 | **−2** |
| `bridge.shadows_bytecode_anywhere` | 6066 | 6243 | 6241 | **−2** |
| `bridge.stated_shadows_bytecode` | 24 | 39 | 39 | 0 |
| `superseded.kind_disagreements` | 52 | 51 | 51 | 0 |
| `superseded.stub_lost_to_admitted` | 4 | 4 | 4 | 0 |

The registry-census diff is the row-level proof of the `+2`, and it is worth
quoting because it is the check `retired_shadow.rs` asks for by name — *"it is a
diff to check, not a number to paste. A ninth kind-map row is a finding."*
11,675 rows in both censuses, **0 added, 0 removed, exactly 2 changed**:

```text
  java/util/ArrayList.get(I)Ljava/lang/Object;   native-collections/src/lib.rs:4521
      bridge  stated=false           ->  synthetic-stub  stated=true    owns_slot=true
  java/util/ArrayList.size()I                    native-collections/src/lib.rs:4519
      bridge  stated=false           ->  synthetic-stub  stated=true    owns_slot=true
```

Both `owns_slot: true`, so neither is an inert entry of the kind that made
`LogRecord.<init>(Level,String)` measure verdict-neutral for a day.

### 6.1a The second wave's deltas, measured the same way

§2.2's five extra triples were adjudicated on their own BEFORE/AFTER pair of
binaries, so their delta is separate from the first wave's:

| gate | BEFORE (`1d8e11741`) | AFTER the five | delta |
|---|---|---|---|
| `stub_ratchet` NO_MANAGEMENT | 1302 | **1308** | **+6** |
| registry census, synthetic-stub | 1323 | **1330** | **+7** |
| registry census, bridge | 10088 | 10081 | −7 |

Seven registrations for five triples, and the pair that makes the difference
visible is worth naming: `iterator` is registered TWICE
(`native-builtins/src/lib.rs:16801`, `native-collections/src/lib.rs:4667`) and
`toArray([Ljava/lang/Object;)` twice
(`native-collections/src/lib.rs:4635`, `vm/src/vm/vm_init.rs:3244`), with the
slot-owning registration in each pair moving alongside its superseded twin. That
is what rules out the `LogRecord.<init>` inert-entry trap for all five. The
hermetic ratchet counts six of the seven because
`vm/src/vm/vm_init.rs`'s registration is outside the `native-builtins` census it
replays.

0 rows added, 0 removed, in both waves.

### 6.1 The baselines are NOT re-frozen, and that is a decision

Three of them fire, and they fired **before** this change too, on the same tree
and the same host. The kind map enumerates its 54 pre-existing rows and they
belong to other lanes' decisions:

```text
  ~27  java/lang/ProcessHandle + ProcessHandle$Info + Runtime.exec   bridge -> synthetic-stub
    8  the 2026-08-12 java/util collections wave                     bridge -> synthetic-stub
   10  java/util/Set.of ×10                                          INTRINSIC -> synthetic-stub
    7  java/util/function/{Predicate,Consumer,BinaryOperator}         bridge -> synthetic-stub
    3  jdk/internal/access/SharedSecrets factories                    bridge -> synthetic-stub
```

The eight collections rows are the 2026-08-12 wave that landed its table entries
and never re-froze — which
`scripts/baselines/jdk-only-bridge-ratchet.json`'s own note predicted and
P2-COLLECTIONS-SHADOWS §4 required. `--update-baseline` regenerates the whole set
from one run, so re-freezing here would silently bless 54 rows this lane has no
measurement for, including ten `Set.of` registrations that went **intrinsic →
synthetic-stub** — the one direction the kind map exists to catch. The gate's own
header says *"Re-freezing without reading the diff defeats the gate"*, and the
bridge-ratchet baseline sets the precedent in its own body: *"A gate that fires
with a stated reason is strictly better than one re-frozen to a derived guess."*

So: **not re-frozen, fully attributed instead.** The numbers above are what a
re-freeze would need, the 54-row list reproduces with one command
(`sh regression-suite/bridge-ratchet.sh` on Linux with `JAVA_HOME` set), and a
lane that can adjudicate the `ProcessHandle`, `Set.of` and `java/util/function`
families should re-freeze all three baselines in one commit at that point.

## 7. The corpus — verdict-neutral, and one platform finding

`CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh`, `TIMEOUT=300`,
`JDK=/data/toolchain/jdk-25`, both binaries:

```text
  BEFORE   97 passed, 2 failed of 99   failing: RJdkOptionalShape RJdkIntrinsics3
  AFTER    97 passed, 2 failed of 99   failing: RJdkOptionalShape RJdkIntrinsics3
```

Identical count and identical failing SET — verdict-neutral, which is the
acceptance criterion `retired_shadow.rs` states and not "green".

**Compatible mode was measured too, and not because it was in doubt.** A
`SyntheticStub` registers and dispatches normally in `Compatible`, so the table
cannot change that arm — but §2's whole argument turns on that being true, so it
was run rather than asserted. `SUITE=all bash regression-suite/run.sh`, both
binaries:

```text
  BEFORE   92 passed, 7 failed of 99
  AFTER    92 passed, 7 failed of 99
  failing set, identical: RImmutableFactoryTypes RJdkOptionalShape
    RJdkIntrinsics3 RJdkProxyIface RJdkFunctionCombinators RJdkEnumerations
    RServiceLoaderDoubleSource
```

### 7.1 Four unit-test failures on this branch, all pre-existing

Stated because two `cargo test` targets are red at `0010e134d` and a later reader
should not attribute them here. Each was checked against this change rather than
assumed:

* **`cratonvm-native-api --test read_alias_coverage`** —
  `every_declared_slot_map_is_published` (an unpublished `METHOD_LEGACY_SLOT_MAP`
  in `native-builtins/src/lang_class.rs`) and
  `every_read_side_observation_is_gated_and_observation_only`. Run with this
  change **stashed**: 9 passed / 2 failed, byte-identically. Nothing this lane
  edited is in either test's scope.
* **`cratonvm-types --test doc_citation_paths`** — both of its tests.
  `no_source_file_links_into_docs_internal` lists **45 lines in 16 files**
  carrying a `docs/internal/` path; `every_relocatable_doc_citation_points_at_the_page`
  names **one** dead citation, `native-builtins/src/lang_math.rs:119` pointing at
  a commons-math record that moved on 2026-08-16. **None of the 46 offending
  lines is a file this change touches**, and that is a measurement: the four
  citations this lane added were written in the `docs/internal/` form first, this
  gate rejected them, and they now use the prefix-less internal-tree-relative
  form the gate's own message prescribes.

`cratonvm-types --lib` and its flag guards are **green** (563 passed / 0 failed),
which is what covers the new declared flag, and `cratonvm-native-api --lib` is
green (326 / 0), which is what covers the retirement table.

**The platform finding.** BASELINE-20260817.md records **99 of 99** at
`89e2c56f1` on Windows against HotSpot 25.0.3+9. On Linux against Temurin
25.0.4+7 the same corpus is 97 of 99, and the two extra reds are present with
this change and without it. That is a `<jdk-feature>-<os>` difference in the
corpus itself, of the kind every baseline in `scripts/baselines/` is keyed for,
and no strict-corpus verdict should be quoted without its platform again.

## 8. What this record does NOT close

* ~~**`iterator` / `toArray` / `isEmpty` / `contains` on `ArrayList`.**~~
  **CLOSED, §2.2** — all five registrations retired on a per-triple trial, both
  corpora verdict-neutral. What replaced it as open is narrower and better
  stated: `java/util/ArrayList$Itr` itself has had no trial, and the interface
  door behind the whole confusion is `G63-1`.
* **The system `Properties` receiver.** §3 names the precondition and does not
  meet it. Fixing it is a change to a boot-path native in BOTH modes, which is
  not a retirement-table change.
* **The 56 other `native-won` rows from the vector, and the ~130 more the
  application census found.** Each still needs its own answer; that was true of
  the original and is true now. What is different is that the report can now be
  read without hand-counting, and it says which rows are residue and which are
  the mode working.
* **The application census is one application.** Tomcat is the one measured to
  survive strict mode end to end. H2, Spring and Keycloak are blocked by
  APP-READINESS's three fabrication families, not by anything here.
