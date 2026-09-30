# `--jdk-only`: which `Bridge` families still run over concrete bytecode, and at which doors (HT-2 census)

Status: OPEN (census and method; each family is its own retirement decision)
Area: `vm/src/vm/vm_exec.rs` (`resolve_dispatch`, `resolve_native_dispatch_wave1`, `invoke_or_native`), `vm/src/runtime/interpreter/native_override.rs` (`resolve_step1_native`, `revalidate_cached_native`), `vm/src/runtime/interpreter/dispatch_virtual.rs` (`populate_virtual_invoke_cache`), `vm/src/jit/helpers.rs` (`admit_jit_fast_native`, `resolve_native_site`, `direct_helper_refusal`), `native-api/src/retired_shadow.rs`
Severity: MEDIUM (index page; the defects are per family, like `HashSet`'s)
Found by: round 13 wave 7 lane shadow (proposal HT-2 of `r13w6-hashtree-jdk-only-hashset-bridges-win-from-the-second-call-FIXED-20260928.md`)

## 1. The premise, corrected: not "from the second call", from the first

HT-2 asked for every family "this lets win from the second call". Read on this tree there is no
second-call door that differs from the first. Under `--jdk-only` with the enforcement dial
unarmed, a registered `Bridge` over a method that has `Code` is admitted at EVERY door:

| door | where | `bytecode_available` it passes for a `Bridge` |
|---|---|---|
| `Step1` (cold interpreter call) | `native_override.rs` `resolve_step1_native` | `shadows_bytecode && enforce` (dial only) |
| `CacheRevalidate` (warm hit) | `native_override.rs` `revalidate_cached_native` | dial only |
| virtual cache populate | `dispatch_virtual.rs` `populate_virtual_invoke_cache` | nothing asked before wave 7; dial since |
| `CachePopulate` (static) | `dispatch_static.rs` `populate_invoke_cache` | dial only |
| `InvokeOrNative` + alias + both parent arms | `vm_exec.rs` `invoke_or_native` | `has_real` (stubs only) or dial |
| `JitFastNative`, JIT site cache | `helpers.rs` `admit_jit_fast_native`, `resolve_native_site` | `has_real` or dial |
| JIT thin helpers | `helpers.rs` `direct_helper_refusal` | `jit_fast_native_bytecode_terms(..).1` = `has_real` or dial |

`resolve_dispatch` step 3 ("concrete bytecode beats a registered `Bridge`") has ONE caller,
`invoke.rs` (~5646), and it is inside `if is_native`: an `ACC_NATIVE` method returns at step 1,
so no live door evaluates step 3 for a method with `Code`. AGENTS.md's "real class bytes are
authoritative over registered natives" is therefore enforced by exactly two things:
`retired_shadow.rs` (per family, permanent) and `CRATONVM_ENFORCE_NATIVE_SHADOW` (per prefix,
diagnostic). That is the measured design (`retired_shadow.rs` header: whole-VM step 3 took the
strict corpus from 32/17 to 3/46), not a leak; but it means the population below is served by
natives from the FIRST call, cold and warm, interpreted and compiled.

## 2. The population

From the frozen 25/linux census (`scripts/baselines/jdk-only-bridge-ratchet.json`,
`25/linux/jdk-only`, image-adjudicated): **3,626 `Bridge` registrations shadow bytecode
somewhere** (2,774 over a declared `Code` method, 852 over an inherited one), of 7,915 bridges
registered under `--jdk-only`. The ratchet is frozen at that figure with slack 0, so it can only
go down; the `HashSet` family (this wave) takes up to 63 of them.

The kind map (`scripts/baselines/jdk-only-kind-map-25-linux.tsv`) has the rows but not the
`Code` adjudication, so a per-class cut from it over-counts `ACC_NATIVE` and abstract targets.
Classes with 20+ `Bridge` rows and NO retired row, split by what the reading says they are:

* **State-holding concrete classes, bytecode certainly present (retirement candidates, same
  question as `HashSet`: is the state real?):** `java/util/Locale` 40,
  `java/util/concurrent/ForkJoinTask` 38, `ForkJoinPool` 27, `RecursiveTask` 18,
  `RecursiveAction` 17, `java/net/URI` 30, `java/net/InetAddress` 34,
  `java/net/DatagramSocket` 35, `java/lang/Thread` 63, `java/lang/Module` 29,
  `javax/management/ObjectName` 24, `java/security/Signature` 22, `java/security/Provider` 22,
  `java/lang/invoke/MethodHandles` 31, `MethodHandles$Lookup` 26, `java/lang/invoke/VarHandle`
  37, `java/lang/reflect/Field` 31 (6 retired), `java/lang/Class` 80 (15 retired),
  `java/lang/System$1` 42 / `System$2` 39 (the `JavaLangAccess` anonymous classes: real code
  in every image), `sun/nio/ch/SocketChannelImpl` 47, `DatagramChannelImpl` 81,
  `ServerSocketChannelImpl` 31, `sun/security/ssl/SSLEngineImpl` 36,
  `sun/net/www/protocol/https/HttpsURLConnectionImpl` 72,
  `sun/net/www/protocol/http/HttpURLConnection` 31.
* **Mostly `ACC_NATIVE` targets (not shadows; the census's `acc_native` 781):**
  `jdk/internal/misc/Unsafe`, `jdk/jfr/internal/JVM`, `sun/management/VMManagementImpl`,
  `sun/nio/ch/Net`, `sun/nio/ch/*FileDispatcherImpl`, `sun/nio/fs/UnixNativeDispatcher`,
  `java/lang/reflect/Array`.
* **Interfaces / abstract API types (abstract targets, the census's `abstract_method` 1,402;
  §7's documented no-`Code` deviation):** `java/util/stream/{Stream,IntStream,LongStream,DoubleStream}`,
  `java/nio/channels/{DatagramChannel,SocketChannel,ServerSocketChannel}`,
  `javax/net/ssl/{SSLSocket,SSLSession,SSLServerSocket,SSLContext}`, `java/sql/*`,
  `javax/xml/stream/XMLStreamReader`, `javax/management/MBeanServer`,
  `java/lang/foreign/{MemorySegment,Arena}` (the FFM carriers are this VM's own allocation shape
  by decision, `retired_shadow.rs` `jdk/internal/foreign/layout/` note).
* `java/util/concurrent/CopyOnWriteArraySet` 24: rows kept for the fabricated stub;
  `cow_set_route` sends a real receiver to its own bytecode. Not a shadow in effect.

## 3. How to get the exact per-row list (orchestrator, Linux host)

The per-row "which of these actually RAN over bytecode on a workload" list is a runtime census,
not a source fact:

```text
CRATONVM_ARGS="--jdk-only --explain-jdk-only --jdk-only-report" on the probe battery and the
strict corpus; collect rows of kind `native-shadows-bytecode` / `bridge-ran-over-bytecode`
(`record_native_shadow_ran_over_bytecode`, recorded by step 1 once per triple).
```

Group the taken rows by class, drop the `ACC_NATIVE` and abstract ones (the census already
adjudicates them), and rank by (taken count) x (does the class keep its state in its real
fields). The `HashSet` retirement's doc comment (`RETIRED_SHADOW_L1_HS_TRIPLES`) is the
checklist for the second factor: real backing store, real markers, producers that go through
`<init>`, neighbours already retired, mint sites narrowed (`unconstructed_carrier_gate.rs`).

## 4. Side findings

* `native_override.rs` `every_force_native_file_asks_the_dial_or_is_exempt` exempts
  `jit_bridge.rs` on the ground that "`jit::direct_native_helper` refuses to bind any native
  whose registry kind is not `Intrinsic`". Since the 2026-09-22 fix it admits a `Bridge` unless
  the dial covers it (`direct_helper_refusal` -> `jit_fast_native_bytecode_terms`). The
  exemption's conclusion (the dial there only affects tier-up sealing) still holds; its stated
  reason does not. Rewrite the reason when that test is next touched.
* The virtual cache populate has no `DispatchDoor` of its own, so an armed run's
  `[DIAL_DOOR]` census cannot show it (wave 7 made it ask the dial, without a door row, because
  the enum lives in `vm_exec.rs`). Proposal in `jit-r13-shadow-proposals-RETIRED-20260929.md`.

## How to confirm

`rg -n "resolve_dispatch\(" vm/src` shows the single call; read `invoke.rs` around it for the
`if is_native` guard. Arm-free probe: `R13ShadowHashSetFamily` under `--jdk-only` with
`CRATONVM_UNRETIRE_NATIVE_SHADOW=java/util/HashSet,java/util/LinkedHashSet` differs from HotSpot
on its colliding count lines from the FIRST run (the probe prints first-run values), with
`drift 0`: the native ran cold and warm alike.

## Round 13 wave 8 (lane shadow2)

**Landed (from reading, pending the paired measurement):** the `java.beans` change-support
family, `RETIRED_SHADOW_BEANS_CHANGE_SUPPORT_TRIPLES` in `native-api/src/retired_shadow.rs`,
25 rows: `java/beans/PropertyChangeEvent` 6, `PropertyChangeSupport` 12,
`VetoableChangeSupport` 7, each registered once by `register_p72_beans`
(`native-builtins/src/phases_late/beans_jndi.rs`). Three class prefixes were added to
`RETIRED_SHADOW_PREFIXES` (not `java/beans/`: the `Introspector` / `PropertyDescriptor` /
`FeatureDescriptor` natives are forced for Spring by `vm_exec.rs` and stay `Bridge`; the test
`the_beans_change_support_family_is_retired_alone` holds that line). Kill switch and paired
control: `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/beans/`. Expected census movement: the
25/linux/jdk-only `bridge_shadows_bytecode` 3,626 -> 3,601 (24 declared + 1 inherited,
`PropertyChangeEvent.getSource` from `EventObject`); stub ratchet +25 stubs / +0 rows per arm;
kind map 25 rows `bridge` -> `synthetic-stub`.

Why this family (risk x benefit): the natives were written for the synthetic JDK's 2- and
4-slot layouts and run over the REAL classes, so they diverge visibly (null/equal-value
firing, named listeners, no `PropertyChangeListenerProxy`, swallowed listener exceptions, no
NPE/IAE on a null source, a non-JDK `toString`, a veto revert sent to every listener) and they
write the bean into the slot real bytecode reads as `map`, so the members left unregistered
(`fireIndexedPropertyChange`, `getVetoableChangeListeners`,
`fireVetoableChange(PropertyChangeEvent)`, `writeObject`) break. The real bytecode needs only
already-real classes (`HashMap`, `ArrayList`, `StringBuilder`) and natives every boot uses; the
three classes are not on any boot path; the only mint sites are the retired natives
themselves. Probes: `R13Shadow2PropertyChange`, `R13Shadow2VetoableChange`
(`C:\craton\jitr13-probes\src\`). The same natives still corrupt real-layout objects in
`--compatible`: `r13w8-shadow2-beans-change-support-natives-corrupt-real-layout-in-compatible-FIXED-20260928.md`.

**Candidates read and NOT taken this wave** (the §2 list plus the small `java/util` /
`java/lang` remainder; each is a reason, not a verdict):

| family (unretired Bridge rows) | why not now |
|---|---|
| `java/util/Random` (10) | every triple is also registered `Intrinsic` (kind-map ordinals 0 and 2), which wins last-write and survives `--jdk-only`; a table cannot retire it. The intrinsic is kept by a measured throughput decision (`CRATONVM_JDK_RANDOM`, `native-collections/src/lib.rs` `register_random_natives`). Side finding filed: `r13w8-shadow2-random-draw-natives-ignore-an-overridden-next-FIXED-20260928.md`. |
| `java/util/AbstractMap$SimpleEntry` (6) | VM invariant: `alloc_live_entry` mints it with a third, undeclared write-through `sourceMap` slot that `native_entry_set_value` maintains; real `setValue` would stop writing through to the map. |
| `java/net/URI` (30) | twelve `try_alloc_concurrent_synthetic(.., "java/net/URI", n)` mint sites with 1, 2, 5, 6, 7 and 18 slots (`http2.rs`, `http_client.rs`, `net_phase_e.rs`, `net_uri_inet.rs`, `phases_early.rs`, `nio_file.rs`) bypass `<init>`; real getters would read unwritten fields. Mint sites first. |
| `java/lang/Integer` / `Long` / `Double` / `Float` text (4 + 3 + 1 + 1) | `parseInt` / `parseLong` also have intrinsic ids (`native-builtins/src/intrinsics/mod.rs`); retiring only the registry row could leave the interpreter on bytecode and a compiled site on the intrinsic. Decide the intrinsic first. |
| `java/lang/StringBuilder` (5) | inherited-only rows (`append(AbstractStringBuilder)`, `appendNull`, `getCoder`, `getValue`, `repeat(CI)`); `RETIRED_SHADOW_STRINGBUILDER_TRIPLES`' doc kept them out by rule. Worth a measured follow-up: an inherited-row `Bridge` is still reachable by virtual dispatch on a `StringBuilder` receiver. |
| `java/util/TreeMap$KeySet` (6) | `<init>()V`, `clone`, `readObject`, `writeObject`: `TreeSet` methods registered on the key-set view; no image declares them there, so no shadow in effect. |
| `java/util/LinkedHashMap` (4), `HashSet`/`LinkedHashSet.comparator` (2) | held on purpose by their own retirement docs (`putFirst`/`putLast`/`poll*Entry` mirror into real fields; `comparator` has no bytecode). |
| `java/util/AbstractCollection` (4) | generic `contains` / `toArray` bridges over every non-overriding subclass, including VM-fabricated carriers; a blast-radius question, not a family. |
| `java/util/Locale` (40), `ForkJoinTask` family (38 + 27 + 18 + 17) | boot-path / VM-invariant candidates (locale providers with measured blockers; the FJ side table the GC remaps). Not low-risk. |

## Round 13 wave 9 (lane shadow3)

No family retired this wave (by assignment). Read while fixing the two wave-8 side pages:

* `java/util/Random` (the table row "every triple is also registered `Intrinsic`"): fixed
  from inside the natives instead of by a table. A proper-subclass receiver now runs
  `Random`'s own bytecode at every door (`securerandom.rs` `random_native_key` /
  `random_ctor_runs_bytecode`, `CRATONVM_RANDOM_SUBCLASS_YIELD`), which is exactly AGENTS.md's
  "real bytecode beats the native" for the receivers where the native is wrong; exact
  receivers keep the measured fast path. Page: `r13w8-shadow2-random-draw-natives-ignore-an-overridden-next-FIXED-20260928.md`.
* `java/security/SecureRandom` inherited draws (`Intrinsic`, not in the Bridge census, but
  the same shape): a `nextBytes`-overriding subclass was handed back with
  `invoke_virtual_bytecode_only`, which loops forever through an override that calls
  `super.nextInt()`. Fixed (`secure_random_subclass_draw`, invokespecial on `Random`'s body).
* The `java/beans` change-support rows are now also absent from `--compatible` on a real JDK
  (`beans_jndi.rs` `beans_change_support_left_to_bytecode`); `--jdk-only` still retires them
  through the table.
* Filed: `r13w9-shadow3-random-serialization-ignores-the-side-table-FIXED-20260928.md` (exact
  `Random`: `writeObject` NPEs on the null real `seed`; a deserialized one draws entropy).
* Measurement note for the next retirement: `native-builtins/tests/common/vm_init_boot_path.rs`
  (the stub ratchet's "real-JDK boot path") never calls `set_real_jdk(true)`, so every
  registrar that branches on `registry.real_jdk()` (`securerandom.rs`, `native-collections`'
  `register_random_natives` and delegating-`CompletableFuture` skip, `native-io`'s scanner,
  `reflect_invoke.rs`'s StackWalker, and now `beans_jndi.rs`) is counted in its
  synthetic-JDK shape there. A paired stub-ratchet measurement of any of them measures
  nothing; use `--dump-native-registry` on a real VM instead.

## Round 13 wave 12 (lane shadow4)

**Landed (from reading, pending the paired measurement): the throwable trace pair,**
`RETIRED_SHADOW_THROWABLE_TRACE_PAIR_TRIPLES` in `native-api/src/retired_shadow.rs`, 123 rows:
`printStackTrace(Ljava/io/PrintWriter;)V` on all 62 `THROWABLE_FAMILY_CLASSES` and
`setStackTrace([Ljava/lang/StackTraceElement;)V` on 61 of them (`InvocationTargetException`
registers none). Each is registered once, by `register_throwable_subclass_natives`
(`native-builtins/src/lang_misc.rs:4011`, one `set_category(Bridge)` scope, no `real_jdk()`
branch), so the registrar needed no change: the central re-tag does it. No new prefix.

Why this family. It is the only residue on lane T's class set: lane T retired all 906 rows on
2026-09-10 and pulled these 123 back on 2026-09-11 (lane-T record §8) on a one-class
(`ParseException`) measurement that printed the full captured trace after
`setStackTrace(new StackTraceElement[0])`. Read on this tree, that trace came from the
INHERITED NATIVE printer on `Exception` / `Throwable`, still live under a one-class arm, which
never reads the `stackTrace` field (`throwable_frame_text` reads only the capture store). With
both methods retired on every class including `java/lang/Throwable`, real `setStackTrace` writes
the field and real `printStackTrace(PrintStreamOrWriter)` reads it through `getOurStackTrace()`.
The screen:

| criterion | reading |
|---|---|
| pure-Java bytecode | both declared with `Code` on `java/lang/Throwable` in 17/21/25; 2 declared + 121 inherited rows |
| unconstructed carrier | none new: `printStackTrace(PrintWriter)` runs the SAME private body the `PrintStream` overload has run since lane T; `java/io/PrintWriter` has no live `Bridge` row and no native mint site; `Throwable$WrappedPrintWriter` has no natives |
| VM invariant on the natives | none: `jni.rs` `ExceptionDescribe` calls `printStackTrace()V` (bytecode already), nothing in `vm/` or `native-*/` calls the two bodies directly, no `real_protected_stub_class` / real-layout drop arm names a throwable |
| HotSpot difference today (`--jdk-only`) | yes, three: a set trace is printed by the `PrintStream` sink and ignored by the `PrintWriter` sink (JUnit `readStackTrace`, JUL `SimpleFormatter`); `printStackTrace((PrintWriter) null)` does not throw; `setStackTrace` ignores the immutable-stack protocol (`len=1` for 0) |

Probe: `C:\craton\jitr13-probes\src\R13Shadow4ThrowableTracePair.java` (S0-S8, `bad`, `drift`).
Paired control and kill switch (a `Class` rule would also un-retire lane T's rows, so name the
triples; bash, repo root):

```text
export CRATONVM_UNRETIRE_NATIVE_SHADOW=$(awk -F'\t' '!/^#/ && (($2=="printStackTrace" && $3=="(Ljava/io/PrintWriter;)V") || $2=="setStackTrace") {printf "%s%s.%s%s", s, $1, $2, $3; s=","}' scripts/baselines/jdk-only-kind-map-25-linux.tsv)
# the arm announcement must list 123 rules at 1 table row each
```

Expected movement (orchestrator re-freezes):

* `native-builtins/tests/stub_ratchet.rs`: **+123 stubs, +0 rows in every arm on both
  platforms** (the registrar is universal and not `real_jdk()`-gated, so the synthetic-jdk arm
  moves too). From the current freeze: linux management 5128 -> 5251, no-management 5101 -> 5224,
  synthetic-jdk 5101 -> 5224; windows 5168 -> 5291, 5141 -> 5264, 5141 -> 5264; totals unchanged
  (linux 14249 / 13873 / 13884, windows 14276 / 13900 / 13911). Measure paired with the control
  above; the unretired arm must land on the frozen constants.
* `scripts/baselines/jdk-only-kind-map-25-linux.tsv`: amended by hand, 123 rows `bridge 0 1` ->
  `synthetic-stub 1 1`, all ordinal 0.
* `tools/scripts/baselines/jdk-only-kind-map-25-linux.tsv`: NO row edit. That copy was frozen
  while lane T still held the 123 and already reads `synthetic-stub 1 1` for them; it was stale
  from the 2026-09-11 pull-back until now. An `# amended:` note says so.
* `scripts/baselines/jdk-only-bridge-ratchet.json` (not edited): `25/linux` and
  `25/linux/jdk-only` `bridge` rows -123, `shadows_bytecode` -2 (the two `java/lang/Throwable`
  rows), `inherited_shadows_bytecode` -121, `shadows_bytecode_anywhere` -123, `synthetic-stub`
  registrations +123.

Test changes in `retired_shadow.rs`: new `the_throwable_trace_pair_is_retired_on_lane_t_s_class_set`
(the pair on exactly lane T's `printStackTrace(PrintStream)` class set, `Throwable` included,
`fillInStackTrace(I)` / `getStackTraceDepth` still out); the two `ParseException` rows left
`wave_four_is_six_classes_and_refuses_jarfile_and_dateformat`'s must-not-retire list, with the
reason. `--compatible` is unchanged, and its version of the same divergence (all three
`printStackTrace` natives ignore a set trace) is filed:
`r13w12-shadow4-throwable-native-print-ignores-set-stack-trace-FIXED-20260929.md`.

**One family, not two.** No second candidate cleared all three criteria from reading; each
failure is named below.

### The next families, ranked (value x safety), with the hazard that holds each

Mint-site counts are `try_alloc_concurrent_synthetic(.., "<class>", n)` sites in `native-*/src`
(objects built without `<init>`, the unconstructed-carrier hazard).

| rank | family (unretired `Bridge` rows) | HotSpot difference today | hazard, and the first step |
|---|---|---|---|
| 1 | `java/lang/StringBuilder` inherited five (5) | none known | low: real layout, `AbstractStringBuilder`'s own rows retired. First: `CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/StringBuilder` on the probe tree; `reached == 0` means delete the registrations rather than retire them |
| 2 | `java/io/ByteArrayInputStream` `read()I`, `read([BII)I`, `close()V` (6) | none: the natives are right | a coupling, not a carrier: the three are the only `BaisEvent` sites (`native-io`), the https drain instant (`RSslLiveSession`). First: move the observer to a stream the HTTP layer owns |
| 3 | `javax/management/ObjectName` (24) | likely (canonical key order, quoting, patterns); unmeasured | 1 mint site (`jmx.rs:1305`, 1 slot) plus `class_manager.rs:16630` `instance_fields(1)`; `MBeanServer` natives hand out native names. First: mint through `<init>(String)` |
| 4 | `java/lang/reflect/Parameter` (4) | unmeasured (`isNamePresent`, `isImplicit`) | minted by `shared_secrets_bridge.rs:2497` `new_object` without `<init>`; `getAnnotatedType` is forced by `is_typeuse_annotation_native_override` (`native_override.rs:65`) |
| 5 | `java/lang/Runtime$Version` (4) | unmeasured | `lang_system.rs:3782` (4 slots) plus forced arms `native_override.rs:4524` / `vm_exec.rs:35229` (a class-name allow-list that must go with it) |
| 6 | `java/util/Currency` (2) | unmeasured | `phases_early.rs:9161` (2 slots); locale-provider data behind `getSymbol(Locale)` |
| 7 | `java/lang/SecurityManager` (14) | likely on 25 (JEP 486 changed what the check methods do), unmeasured; 17 differs from 25 | a VM-modelled installable manager (`security_manager.rs`), image-dependent semantics; must move with `System.get/setSecurityManager` |
| 8 | `java/util/TimeZone` (7) | unmeasured | measured load-bearing (`getID`, L1 wave 1) and +2 WORSE on the dial; needs its own binary |
| 9 | `java/lang/invoke/MethodType` (6) | unmeasured | 3 mint sites (`lang_invoke.rs:12804`, `:25610`, `:27849`, 6 slots); method-handle core |
| 10 | `java/net/URI` (30) | unmeasured | 12 mint sites (1-18 slots) |
| 11 | `java/util/ResourceBundle` (17 + `$1` 3 + `$Control` 1) | unmeasured | the locale-provider chain; `LocaleResources` blockers measured (`RETIRED_SHADOW_L1_LP_TRIPLES`) |
| 12 | `java/util/Locale` (40, 7 mint sites), `java/lang/Module` (29, 8), `java/net/InetAddress` (34, 3), `java/lang/Thread` (63), FJ family (100) | varies | boot path / VM invariants |
| 13 | `java/lang/Integer` / `Long` / `Double` / `Float` text (4 + 3 + 1 + 1) | unmeasured | intrinsic ids and hot paths; decide the intrinsic first |
| 14 | `java/util/AbstractMap$SimpleEntry` (6) | none | VM invariant: `alloc_live_entry`'s write-through `sourceMap` slot |

Not shadows at all, and worth deleting rather than retiring (they inflate the kind map's
`bridge` count): field-shaped registrations with a field descriptor
(`java/util/jar/Attributes$Name.MAIN_CLASS` / `MANIFEST_VERSION`, `java/util/Locale.CANADA` ...,
`java/lang/ProcessBuilder$Redirect.INHERIT` / `PIPE`), and the `Collector`-shaped rows on
`java/lang/Object` (`supplier`, `accumulator`, `combiner`, `finisher`). No image declares any of
them, so neither door can dispatch them. Proposal SH4-2 in `jit-r13-shadow4-proposals-RETIRED-20260929.md`.

## Round 13 wave 13 (lane shadow5)

**SH4-1 landed: the ancestor gate.** `native-builtins/tests/r13_shadow5_retired_ancestor_gate.rs`
(`no_retired_triple_has_a_live_bridge_on_an_ancestor`) walks every retired row of the compatible
boot registry up its JDK 25 superclass chain, the way `invoke_or_native` and the virtual-invoke
cache populate do (only when the class does not declare the method; the first live `Bridge` on an
ancestor answers; the first declaring ancestor stops the walk), and fails on a retired row whose
ancestor still has a live `Bridge` in the `--jdk-only` boot registry. The image facts it needs
(superclass of every retired class and its chain, and the declarations that stop a walk) are two
tables in the test, from the JDK 25 sources. Read against the 25/linux kind map it found 20 pairs:

* **Fixed here:** `VirtualMachineError.getMessage` / `toString` above lane T's retired
  `InternalError` / `OutOfMemoryError` / `StackOverflowError` rows (6 pairs), the SH4-1 species on a
  second family. Retired as `RETIRED_SHADOW_VME_MESSAGE_PAIR_TRIPLES` (2 rows).
* **Open, frozen in the gate's `KNOWN_HALF_RETIREMENTS`:** 16 collection-view `toArray` rows under
  `AbstractCollection`'s live bridges, `CharBuffer.session` / `checkSession` under `Buffer`,
  `ByteArrayOutputStream.write([B)` under `OutputStream`:
  `r13w13-shadow5-ancestor-bridges-over-retired-rows-FIXED-20260929.md`.

**Family retired: `StringBuilder`'s five inherited rows (rank 1),**
`RETIRED_SHADOW_SB_INHERITED_FIVE_TRIPLES`. The hazard held: the virtual-invoke cache populate looks
the native up on the RECEIVER's class (`dispatch_virtual.rs`, `lookup_name`), so a `StringBuilder`
receiver found these five live rows although their `AbstractStringBuilder` declarers were retired
on 2026-09-21. No HotSpot difference is known (the natives read the compact layout through
`sb_view`); the probe `R13Shadow5StringBuilderInherited` is the witness. Rank 2
(`ByteArrayInputStream`, the `BaisEvent` coupling) and every later rank still have the hazards
their rows name; none was taken.

**SH4-2 landed in part:** 24 dead registrations deleted at their registrars (20 field-shaped
`java/util/Locale` constants, the `lib.rs` half of the `Attributes$Name` pair, and the
method-shaped `ProcessBuilder$Redirect.PIPE()` / `INHERIT()`). Held, with exact patches:
`Locale.US` / `UK` (a synthetic-jdk vm test calls them), the `jar_manifest.rs` `Attributes$Name`
pair (the unconstructed-carrier baseline), `ByteOrder.BIG_ENDIAN` / `LITTLE_ENDIAN` (registrar-drift
rows), the FFM `ValueLayout` constants (E40-1's test list):
`r13w13-shadow5-field-shaped-rows-held-by-tests-patch-FIXED-20260929.md`. The four `Collector` rows on
`java/lang/Object` are NOT dead (the superclass walks reach `Object` for any receiver that leaves
`supplier()` undeclared, and the collapsed-to-`Object` synthetic collector is served by them);
SH4-2's premise was wrong for them.

Paired control and kill switch for the two retirements (seven rules, one table row each):

```text
export CRATONVM_UNRETIRE_NATIVE_SHADOW='java/lang/StringBuilder.append(Ljava/lang/AbstractStringBuilder;)Ljava/lang/AbstractStringBuilder;,java/lang/StringBuilder.appendNull,java/lang/StringBuilder.getCoder,java/lang/StringBuilder.getValue,java/lang/StringBuilder.repeat(CI)Ljava/lang/AbstractStringBuilder;,java/lang/VirtualMachineError.getMessage,java/lang/VirtualMachineError.toString'
```

Expected movement (from reading; orchestrator re-freezes): `stub_ratchet.rs` **+7 stubs in every
arm on both platforms** (each triple registered once, by essentials-path registrars with no
`real_jdk()` branch) and **-24 total registrations** (the deletions; stubs unchanged). Windows
5291 -> 5298, 5264 -> 5271, 5264 -> 5271, totals 14276 -> 14252, 13900 -> 13876, 13911 -> 13887;
linux 5251 -> 5258, 5224 -> 5231, 5224 -> 5231, totals 14249 -> 14225, 13873 -> 13849,
13884 -> 13860. The control arm above lands on the wave-12 stub constants with totals -24. Kind map
(both copies) amended by hand: 7 rows `bridge 0 1` -> `synthetic-stub 1 1`, 24 rows deleted.
`jdk-only-bridge-ratchet.json` (not edited): `bridge` -31, `synthetic-stub` +7,
`inherited_shadows_bytecode` down by up to 7 (all seven rows are on classes that do not declare
the method; `appendNull` and `repeat(CI)` are private on `AbstractStringBuilder`, so the census may
bucket those two as undeclared instead), and the undeclared-method bucket down by the 24 deletions.

## Round 14 wave 2 (lane shadow)

**Family retired: `java/io/OutputStream.write([B)V` (1 row),**
`RETIRED_SHADOW_OUTPUT_STREAM_WRITE_ALL_TRIPLES` (item 2 of the ancestor page). The screen:
the native (`native_output_stream_write_all`, `native-builtins/src/lib.rs`) is exactly the
JDK body -- NPE on null, then `write(b, 0, b.length)` virtually -- so the only HotSpot
difference was the missing `java.io.OutputStream.write` frame in a trace thrown from a
`write([BII)` override; `OutputStream` has no instance fields and its only mint sites are in
`#[cfg(test)]` modules; nothing in the VM calls the native by name; `--compatible` keeps a
`SyntheticStub` that dispatches as the `Bridge` did. Kill switch / paired control:
`CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/OutputStream.write([B)V`. Probe:
`C:\craton\jitr14-probes\src\R14ShadowOutputStreamWriteAll.java`. Expected movement:
`stub_ratchet.rs` **+1 stub, +0 rows in every arm on both platforms** (one registration,
ordinal 0, no `real_jdk()` branch); kind map (both copies) 1 row `bridge 0 1` ->
`synthetic-stub 1 1`; `jdk-only-bridge-ratchet.json` (not edited): `bridge` -1,
`shadows_bytecode` -1.

**Structural change: the ancestor-walk mask** (`r13w13-shadow5-ancestor-bridges-over-retired-rows`,
now FIXED pending verification). The census's §1 table said a `Bridge` over bytecode is
admitted at every door; for an INHERITED method whose receiver class's row is retired that
is no longer true at the four walks the mask reaches (virtual populate, vtable probe,
stackless step 1, JIT site cache; `invoke_or_native` by patch page). A future retirement of
a non-declaring subclass row therefore takes effect without retiring its ancestor.

**Read and NOT taken this wave** (reasons, from reading):

| family | why not now |
|---|---|
| `java/io/ByteArrayInputStream` `read()I`, `read([BII)I`, `close()V` (rank 2) | still the three `BaisEvent` observation sites (`native-io/src/lib.rs` ~4146 / ~4409 / ~4566), which `http_url_connection.rs`'s drain instant depends on; moving the observer is a `native-io` + HTTP-layer change, not a table row. |
| `java/lang/Runtime$Version` `feature()I` / `build()` (rank 5, 4 registrations) | the natives answer from the real fields when `native_runtime_version` managed to fill them and fall back to the host version when it did not ("best effort during very early bootstrap"); the real `feature()` is `version.get(0)`, an NPE on that fallback object, and `JarFile.<clinit>` calls `Runtime.version().feature()`. No HotSpot difference is known on a populated object. Retire only together with making the mint fill `version` unconditionally (or minting through the real parser off the boot path), and with removing the name arms in `native_override.rs` (~4520) / `vm_exec.rs` (~35446). |
| `java/lang/reflect/Parameter` `isImplicit` / `isSynthetic` / `isNamePresent` / `getAnnotatedType` (rank 4) | `isNamePresent`'s real body asks `executable.hasRealParameterData()`, which runs `Executable.privateGetParameters()` -> the `ACC_NATIVE` `getParameters0()`; this VM serves `Method.getParameters` from `build_parameter_array` and leaves `Executable.parameters` / `hasRealParameterData` unwritten. Spring's name discovery branches on it. `getAnnotatedType` is still on `is_typeuse_annotation_native_override`'s name list. |
| `java/util/Currency` (rank 6) | the locale-provider chain behind `getSymbol(Locale)`; unchanged. |
| `AbstractCollection.toArray` x2, `Buffer.session` / `checkSession` (the ancestor rows) | no longer needed for the half-retirements (masked); retiring them would move every OTHER receiver, including VM-fabricated carriers, and still needs the blast-radius run the ancestor page describes. |

## Round 14 wave 5 (lane compat5)

**Family retired: the `java.sql` date/time family (11 rows),**
`RETIRED_SHADOW_SQL_DATETIME_TRIPLES` in `native-api/src/retired_shadow.rs`, with three class
prefixes (`java/sql/Date`, `java/sql/Time`, `java/sql/Timestamp`; not `java/sql/`, whose JDBC
interfaces are abstract targets with natives of their own). Rows: `Date` / `Time` `<init>(J)V`,
`getTime()J` (inherited rows over `java.util.Date.getTime`), `toString()`; `Timestamp`
`<init>(J)V`, `<init>(IIIIIII)V`, `getTime()J`, `toLocalDateTime()`, `toString()`. All registered
once by `register_sql_datetime_natives` (`native-builtins/src/jdbc.rs`, inside
`register_jdbc_driver_natives`' explicit `Bridge` scope, no `real_jdk()` branch), so the registrar
needs no change: the central re-tag does it.

The screen:

| criterion | reading |
|---|---|
| pure-Java bytecode | every declared row has `Code` on 17/21/25; the two `getTime` rows are inherited and land on `java.util.Date.getTime()` (no native registered there). The real bodies need `java.util.Date`'s constructors and deprecated getters (retired to bytecode since L1 wave 9, `RETIRED_SHADOW_L1_DATE_TRIPLES`) and `LocalDateTime.of` |
| unconstructed carrier | none: no `try_alloc_concurrent_synthetic(.., "java/sql/..")` outside `#[cfg(test)]`; the natives already wrote `fastTime` (slot 0) and `Timestamp.nanos` (last slot) where the real constructors write them. The retired `toLocalDateTime` native was itself a mint site (`LocalDate` / `LocalTime` / `LocalDateTime` without `<init>`) |
| VM invariant on the natives | none: no `vm/` or `native-*/` code names the triples; the Liquibase column reader reads the fields and is unaffected |
| HotSpot difference today (`--jdk-only`) | yes: the natives read `fastTime` and never the `cdate` real `java.util.Date` keeps after a deprecated setter, which runs real bytecode since L1 wave 9. `new Timestamp(0L)` then `setHours(5)`: `getTime()` answered `0` and `toString()` the unset time (HotSpot normalizes: 5 h local). Same for `Time.setHours` and `java.sql.Date.setYear/setMonth/setDate`. The natives also formatted with the VM's own calendar rather than `Date`'s getters |

`--compatible` is unchanged (a `SyntheticStub` registers and dispatches as the `Bridge` did; no
`java/sql/` class is on a real-layout drop arm). Kill switch / paired control:
`CRATONVM_UNRETIRE_NATIVE_SHADOW=java/sql/Date,java/sql/Time,java/sql/Timestamp` (the arm
announcement must list 11 table rows). Probe: `C:\craton\jitr14-probes\src\R14Compat5SqlDatetime.java`.
Ancestor gate: `native-builtins/tests/r13_shadow5_retired_ancestor_gate.rs` got the three
superclass rows (`java/util/Date`) and the six declarations its walk needs.

Expected movement (from reading; the orchestrator re-freezes):

* `native-builtins/tests/stub_ratchet.rs`: **+11 stubs, +0 rows in every arm on both platforms**
  (one registration per triple, ordinal 0, universal registrar).
* `scripts/baselines/jdk-only-kind-map-25-linux.tsv` and its `tools/scripts/` copy: 11 rows
  `bridge 0 1` -> `synthetic-stub 1 1` (the `java/sql/{Date,Time,Timestamp}` rows; this lane does
  not own the baselines, so they are NOT amended here).
* `scripts/baselines/jdk-only-bridge-ratchet.json` (not edited): `bridge` -11, `synthetic-stub`
  +11, `shadows_bytecode` -9, `inherited_shadows_bytecode` -2 (the two `getTime` rows).

**Read and NOT taken this wave** (a verdict each, from reading):

| family | verdict |
|---|---|
| `java/util/logging/Formatter.formatMessage` (1) | `jul_formatter_format_message` is a full transcription of the JDK body (bundle lookup, `MissingResourceException` drop-through, the fenced `{digit` scan, `MessageFormat`), measured against HotSpot twice (W7-43, G21-1). No HotSpot difference is known; retiring it moves the log-formatting hot path of every Spring Boot run onto `MessageFormat` bytecode for fidelity only. Candidate for a measured wave, not a reading one. |
| `java/util/logging/Logger.log(Level,Supplier,Throwable)` (1) | not a JDK signature (the real overload takes the `Throwable` second): a dead row, held by `retired_shadow.rs`' own test. Delete, not retire (proposal CP5-2). |
| `java/lang/management/MemoryUsage.<init>()V` (1) | no image declares a no-arg constructor: a dead row. Delete after checking no VM mint calls it by that descriptor (proposal CP5-2). |
| `java/util/PrimitiveIterator$OfInt/OfLong/OfDouble` (9) | VM invariant: `make_primitive_iterator` (`native-collections`) mints carriers whose class is the INTERFACE; `hasNext` / `nextInt` are abstract there, so retiring them is an `AbstractMethodError`. |
| `java/security/KeyPair`, `CodeSource`, `ProtectionDomain` (2 + 2 + 2) | `java/security/` is a measured NOT-admitted prefix (`the_lane_l6_security_and_tls_prefixes_are_not_admitted`), and `classloader_real.rs` mints `CodeSource` / `ProtectionDomain` without `<init>`. |
| `java/util/concurrent/ConcurrentHashMap.reduceEntries(J,BiFunction)Ljava/lang/Object;` (1) | a dead row: the real erasure returns `Ljava/util/Map$Entry;`, so no image declares this descriptor (held out by `the_phase3_wave_excludes_the_registration_with_no_image_target`). Delete, not retire (proposal CP5-2). |
| `java/lang/StackTraceElement.computeFormat` / `initStackTraceElements(.., Throwable)` (2) | trace-frame internals in lane trace4's area this wave; the real `computeFormat` reads `declaringClassObject`, which a VM-built element would have to carry first. Not read further. |
| `java/lang/ClassValue.get` / `remove` (2) | the real body runs over `Class.classValueMap` and `ClassValue$ClassValueMap`; whether the VM keeps that state is not established from reading. Not taken. |
| `java/util/TimeZone`, `Currency`, `ResourceBundle`, `MethodType`, `URI`, `Locale`, ... | unchanged from the round-13 wave-12 ranking and the round-14 wave-2 table above. |

## Round 14 wave 6 (lane compat6)

**Landed (from reading):** `java/lang/reflect/Array.newInstance`, both overloads,
`RETIRED_SHADOW_REFLECT_ARRAY_NEW_INSTANCE_TRIPLES` (2 rows). Correction to §2: `java/lang/reflect/Array`
is not wholly an `ACC_NATIVE` target -- `newInstance(Class,int)` and `newInstance(Class,int...)` are Java
on 17/21/25 over the natives `newArray` / `multiNewArray` (those, and every accessor, stay `Bridge`).
Control: `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/lang/reflect/Array.newInstance`. Expected movement:
`bridge_shadows_bytecode` -2, stub ratchet +2 per shipping arm, 2 kind-map rows `bridge 0 1` ->
`synthetic-stub 1 1` (patch page `r14w6-compat6-array-new-instance-kind-map-patch-FIXED-20260929.md`).
Record: `r14w5-trace4-array-new-instance-bridge-over-bytecode-FIXED-20260929.md`.
