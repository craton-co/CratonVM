// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Registrations RETIRED as contract §1.4 shadows: a native standing in front
//! of concrete JDK bytecode, one subsystem at a time.
//!
//! # The rule
//!
//! §1.4 says a native that shadows real bytecode should yield to it under
//! `--jdk-only`. The obvious disposition for the whole shadow population is
//! `SyntheticStub`, and that was implemented as a dispatch-time dial
//! (`CRATONVM_ENFORCE_NATIVE_SHADOW`) and MEASURED: arming it whole-VM takes
//! the strict corpus from **32 passed / 17 failed to 3 / 46**. The failures are
//! not dispatch faults — under `--jdk-only` the surviving bridges ARE the
//! object model for large parts of `java.base`, so yielding them hands real
//! code objects it cannot service.
//!
//! So the order is fixed and is not this file's choice: a class's state has to
//! become real before its shadow can be retired. What this file holds is the
//! subsystems where that has been **measured true**, triple by triple.
//!
//! # `java/util/logging` — retired 2026-08-11
//!
//! Measured on Azure linux, JDK 25.0.4+7, one binary, one workload, only the
//! dial differing:
//!
//! ```text
//!   SUITE=jdk-only CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh
//!     baseline                                       23 passed / 4 failed
//!     CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/logging/  23 passed / 4 failed
//!   failing set unchanged: RJdkHandles RJdkReflect RJdkForkJoin RJdkJmx
//! ```
//!
//! Verdict-neutral is the acceptance criterion, not green: those four fail on
//! `dev` for reasons that have nothing to do with logging, and a change that
//! left them failing for a NEW reason would be a regression this comparison
//! catches.
//!
//! The first 84 triples below are every LIVE (`owns_slot`) `Bridge` registration on a
//! `java/util/logging/` receiver whose image target carries a `Code` attribute,
//! declared or inherited — i.e. exactly the rows the dial would yield. They are
//! re-tagged [`NativeKind::SyntheticStub`](crate::registry::NativeKind::SyntheticStub)
//! at registration, so `--jdk-only` refuses them and the real class runs, and
//! `--real-jdk` is unchanged (a `SyntheticStub` registers and dispatches
//! normally in `Compatible` mode).
//!
//! **One `java/util/logging/` bridge is deliberately NOT here**, and it is why
//! this list is per-TRIPLE rather than per-class or even per-(class, method).
//! `Logger.log` has eight registered overloads; seven shadow real bytecode and
//! are retired. The eighth is
//! `log(Ljava/util/logging/Level;Ljava/util/function/Supplier;Ljava/lang/Throwable;)V`,
//! which is not a JDK 25 signature at all — the real overload takes the
//! `Throwable` SECOND — so the census resolves it nowhere in the hierarchy and
//! there is no bytecode for it to yield to. Refusing it would replace a shadow
//! with an `UnsatisfiedLinkError`, which is the shape the 2026-08-10 wave hit
//! when four of 43 re-tagged receivers had to be held back.
//!
//! # `LogRecord`'s source pair — four more, retired 2026-08-12
//!
//! The wave above left `getSourceClassName`, `getSourceMethodName` and their
//! two setters live, because they were tagged `Intrinsic` and the census scores
//! `Bridge`. Re-tagging them `Bridge` on 2026-08-11 made the census REPORT them
//! (`bridge-ran-over-bytecode`) but retired nothing — retirement is this table,
//! and a category change alone is not an entry in it.
//!
//! What they were is the reason they have to go rather than improve. JDK 25:
//!
//! ```text
//!   getSourceClassName() { if (needToInferCaller) inferCaller(); return sourceClassName; }
//!   setSourceClassName(s) { this.sourceClassName = s; needToInferCaller = false; }
//! ```
//!
//! The shadow getters are that getter with the `inferCaller()` call deleted —
//! a bare field read — and the shadow setters are that setter with the
//! `needToInferCaller` clear deleted. So under `--jdk-only` the real
//! `inferCaller()` was never reached by anybody, and every
//! `logger.warning(...)` record reached `SimpleFormatter` with a null pair,
//! which that formatter renders as the LOGGER NAME.
//!
//! Measured, one binary, three arms (probes/SrcProbe3.java):
//!
//! ```text
//!                  explicit set/get   pair during publish   StackWalker frames
//!   HotSpot        A_CLASS/a_method   SrcProbe3/main        Logger.log, doLog, log, warning
//!   --real-jdk     A_CLASS/a_method   SrcProbe3/main        (chain is native: none)
//!   --jdk-only     A_CLASS/a_method   null/null             IDENTICAL to HotSpot
//! ```
//!
//! That rules out both of the causes the handoff proposed: the setters stick,
//! and our `StackWalker` hands `LogRecord$CallerFinder` exactly the frame list
//! HotSpot's walks. Nothing was broken except that the code which would have
//! CALLED them never ran.
//!
//! ## Necessary and NOT sufficient — read this before trusting a table entry
//!
//! Retiring these four was measured to take effect —
//! `CRATONVM_DBG_DROPPED_STUBS=1` prints `[JDK-ONLY-REFUSED]` for all four —
//! and the vector still failed. With the real lazy getter running, the flag it
//! consults was false: `needToInferCaller` is `true` on HotSpot and `false`
//! here on a fresh record, because a shadow CONSTRUCTOR never wrote it.
//!
//! `LogRecord.<init>(Level,String)` was already in the table below, and had
//! been INERT since the 2026-08-11 wave. The retag in
//! `NativeMethodRegistry::register` fires only on an effective category of
//! `Bridge`, and the triple's OTHER registration
//! (`native-builtins/src/phases_early.rs`) sat under an ambient `Intrinsic`.
//! Strict refused the `Bridge` one and the `Intrinsic` one owned the slot —
//! which is why retiring the `Bridge` one measured verdict-neutral.
//!
//! **An entry in this table is not evidence that a triple has no live native.**
//! It retires the registrations whose effective category is `Bridge`, and says
//! nothing about a second registration of the same triple under `Intrinsic`.
//! `getLevel`, `getMessage` and `getSequenceNumber` are in that position today.
//! Two instruments, and they answer different questions:
//! `CRATONVM_DBG_DROPPED_STUBS=1` lists REFUSALS, not surviving natives; the
//! census kind is what distinguishes them — `synthetic-native-registered` is a
//! refusal record, `native-shadows-bytecode` is a live dispatching shadow.
//! W7-56-infercaller-strict.md
//!
//! **All four or none.** Retiring only the getters would be a NEW defect:
//! the real getter would then honour `needToInferCaller`, which the surviving
//! shadow setter never clears, so an explicit `setSourceClassName("X")` would
//! be silently overwritten by the inferred caller on the next read. The two
//! halves are one state machine and only move together.
//!
//! `Compatible` is untouched, and not by argument: a `SyntheticStub` registers
//! and dispatches normally in `Compatible`, so all four natives still answer
//! there exactly as they did, over records the JUL bridge already stamped.
//! Retiring them in BOTH modes would have been a regression, and that is
//! measured too: probes/SrcProbe4.java runs `CallerFinder` at `inferCaller`'s
//! real depth and gets `EMPTY` under `--real-jdk`, because the native chain
//! leaves no `java.util.logging.Logger` frame to trip its latch. Compatible is
//! correct only via the eager stamp. W7-56-infercaller-strict.md
//!
//! # `java/io/PrintWriter` — measured retirable, HELD, and why the hold is not a doubt
//!
//! Seven `java/io/PrintWriter` triples (`<init>(Ljava/io/OutputStream;)V`,
//! `println` ×4, `write` ×2) were measured verdict-neutral in
//! W7-22-shadow-retirement-logging-and-time.md §2 — including the arm that
//! matters, a NATIVE-built receiver meeting retired methods — because
//! `native_printwriter_init_outputstream` chains into the real
//! `PrintWriter(OutputStream, boolean)` bytecode and leaves `lock`, `out`,
//! `charOut` and `textOut` populated. Its `java/io/PrintStream` sibling does
//! not, which is the whole verdict split between §2 and §3 of that record.
//!
//! **The reinstatement check has been run and comes back clean**, which is the
//! part a future lane should not have to redo. All seven registrations sit in
//! `register_printstream_fallback_natives`
//! (`native-builtins/src/logging_shims.rs`) between its
//! `set_category(NativeKind::Bridge)` and the matching restore, so the retag
//! below would fire on them. The only other registrar holding any of the seven
//! is in `register_synthetic_overrides` under an ambient `Intrinsic` — and that
//! function is `#[cfg(feature = "synthetic-jdk")]` and reached only from
//! `register_builtins` on the `use_synthetic_jdk` arm, so it registers nothing
//! on either shipping mode and cannot hand the triple back the way
//! `phases_early.rs` handed back `LogManager.getLogManager()`
//! (W7-25-jul-getlogger-regression.md §1).
//!
//! **What holds it is arithmetic on frozen artefacts, not the verdict.**
//! `java/io/Print*` is Compatible-visible, so seven rows moving
//! `Bridge` → `SyntheticStub` move `bridge_shadows_bytecode`
//! (`scripts/baselines/jdk-only-bridge-ratchet.json`), `BASELINE_SYNTHETIC_STUBS`
//! (`native-builtins/tests/stub_ratchet.rs`, `SLACK = 0`) and the per-row kind
//! freeze (`scripts/baselines/jdk-only-kind-map-25-linux.tsv`). All three are
//! keyed `25/linux` and must be re-frozen from one real run on that platform in
//! the same commit as the seven entries. Adding the entries alone turns three
//! gates red for a change that is otherwise correct. **Land them together or
//! not at all.**
//!
//! **The ordered recipe is W7-22-shadow-retirement-logging-and-time.md §2.1** —
//! eight steps with the exact commands, including the two the arithmetic cannot
//! give you: `stub_ratchet.rs` now holds **two** baselines (`…_MANAGEMENT` and
//! `…_NO_MANAGEMENT`), each of which must be pasted from its own configuration's
//! printed recount line rather than derived as +7, and
//! `sh regression-suite/bridge-ratchet.sh --update-baseline --note "…"` re-freezes
//! the bridge ratchet AND the kind map from ONE census because they are two
//! readings of one measurement. On a non-Linux host both gate scripts exit **2**
//! ("REFUSING") rather than failing, so a Windows lane cannot even discover
//! whether it got the numbers right — which is why this is held rather than
//! attempted.
//!
//! Two edits, not one, and they only work together: the seven entries go in
//! SORTED position (`java/io/…` sorts before every `java/util/…` row, so at the
//! HEAD of the table) **and** [`triple_is_retired_shadow`]'s prefix
//! discriminator has to admit `java/io/Print`. An entry under a prefix the
//! discriminator rejects answers `false`, which reads as "not retired" and is
//! invisible; `every_entry_is_reachable_through_the_predicate` is the test that
//! catches exactly that.
//!
//! # `java/util` collections — retired 2026-08-12, EIGHT of 68 registrations
//!
//! A `--jdk-only --explain-jdk-only` run reported 226 `native-shadows-bytecode`
//! rows actually taken; 62 of them are `java.util` collections triples, 68
//! registrations by the frozen kind map. All 68 were adjudicated against the
//! rule this module's header states — *a class's state has to become real
//! before its shadow can be retired* — in
//! docs/known-issues/jdk-only/P2-COLLECTIONS-SHADOWS-20260812.md. **Eight are
//! retirable. Sixty are not, and most of them never will be by this route.**
//!
//! The eight, at the HEAD of the table — every `java/util/A…` and `C…` key
//! sorts before `java/util/logging/`:
//!
//! ```text
//!   java/util/ArrayList        <init>          ()V
//!   java/util/ArrayList        <init>          (I)V                              [2 registrations]
//!   java/util/ArrayList        <init>          (Ljava/util/Collection;)V
//!   java/util/ArrayList        add             (Ljava/lang/Object;)Z
//!   java/util/ArrayList        clear           ()V
//!   java/util/Arrays$ArrayList iterator        ()Ljava/util/Iterator;
//!   java/util/Collections      synchronizedMap (Ljava/util/Map;)Ljava/util/Map;
//! ```
//!
//! **All seven are LIVE, not paper entries.** A census on an ordinary
//! collections workload
//! (`--jdk-only --explain-jdk-only --jdk-only-report`) reports every one of
//! them as an actually-taken `native-shadows-bytecode` row — the kind that
//! distinguishes a dispatching shadow from a mere refusal record. The
//! `LogRecord.<init>` inert-entry trap does not apply to any of them.
//!
//! **What this does NOT fix, stated because the campaign brief says it does.**
//! The `Collections.synchronizedList` data loss in
//! STUB-CENSUS-20260812.md §5.2 is attributed there to a
//! `Collections.synchronized*` identity stub. That stub was removed by
//! `e8a7caba4` (2026-07-01); both live registrars build the real wrapper
//! through the real constructor. Retiring `synchronizedMap` is
//! behaviour-preserving by construction and **fixes nothing** — the measured
//! loss has an unidentified cause. See P2-COLLECTIONS-SHADOWS §7.1.
//!
//! ## Why the other sixty are a different question, not a longer list
//!
//! `TreeMap`, `TreeSet`, `ArrayDeque`, `ConcurrentHashMap`, `Hashtable` and
//! `LinkedHashSet` keep their entries in **Rust side tables or fabricated
//! slots**, so retiring their shadows does not hand real bytecode a working
//! object — it hands it an empty one. `map_buckets_slot`'s doc in
//! `native-collections/src/lib.rs` states the whole family's position in one
//! sentence: *"It does not fault today only because the natives shadow every
//! reader."* Those are collections reclassifications, and no entry in this
//! table can substitute for one.
//!
//! `ArrayList` is the exception and is worth stating precisely, because it is
//! the only family in the slice whose §1.4 precondition is ALREADY MET:
//! `al_slots`, `al_mod_count_slot` and `al_itr_slots` resolve `elementData`,
//! `size`, `modCount`, `cursor`, `lastRet`, `expectedModCount` and `this$0`
//! **by name**, and `try_alloc_synthetic` loads the real
//! `java/util/ArrayList$Itr`. Both halves already read and write the fields the
//! JDK's own bytecode does. It is still mostly not retirable, and the reason is
//! not layout: `Map.values()` returns a `java/util/ArrayList` with its source
//! map stashed in a trailing capacity slot, so `size`/`isEmpty`/`get`/
//! `iterator`/`toArray` are the implementation of `values()` and retiring them
//! freezes every such view at its creation time — measured once already, against
//! H2's `TestAlter.testAlterTableDropIdentityColumn`, and recorded at
//! `vm/src/runtime/interpreter/native_override.rs:2448-2463`. The five
//! `ArrayList` triples above are the ones that touch no view.
//!
//! **One triple in the slice must NOT be retired for the opposite reason.**
//! `java/util/Arrays.copyOf([Ljava/lang/Object;I)` is load-bearing *for* real
//! bytecode: the real body allocates through
//! `Array.newInstance(original.getClass().getComponentType(), n)`, and its
//! 3-arg sibling's registration says of that same path *"Without this native
//! the call falls through to bytecode that dereferences unsupported
//! `arrayClass` reflection internals and NPEs"*
//! (`native-builtins/src/phases_early.rs:1031-1038`). Real `ArrayList.grow`,
//! `toArray` and `ArrayList(Collection)` all funnel through it — so the five
//! `ArrayList` retirements above DEPEND on it staying a `Bridge`.
//!
//! ## The acceptance measurement — taken 2026-08-12, one binary, five arms
//!
//! `CRATONVM_ENFORCE_NATIVE_SHADOW` takes a prefix list and yields on the same
//! §1.4 predicate a retirement uses, so it simulates this exact change with no
//! rebuild — the way the `java/util/logging/` wave was accepted. Windows host,
//! JDK 25.0.3, `SUITE=jdk-only CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh`:
//!
//! ```text
//!   baseline (dial off)                         32 passed /  4 failed
//!   A  ArrayList,Arrays$ArrayList,Collections   32 passed /  4 failed   <- the eight
//!   B  List,Collection,Iterator (§2.2 doors)    32 passed /  4 failed
//!   C1 TreeMap,TreeSet                          10 passed / 51 failed   RED
//!   C2 ArrayDeque                               32 passed /  4 failed   see below
//!   C3 concurrent/ConcurrentHashMap             28 passed / 12 failed   RED
//!   failing set unchanged in A and B: RJdkProxyIface RJdkForeign RJdkEnumerations
//! ```
//!
//! Verdict-neutral is the criterion, not green: those three fail with the dial
//! off, for reasons that have nothing to do with collections.
//!
//! **C2 came back verdict-neutral, and that is the probe's reach, not a
//! refutation of §3.5.** The `RJdk*` corpus only pushes and pops a deque's
//! ENDS, and real `ArrayDeque.addLast`/`pollFirst` never call `delete(i)` — the
//! method whose two writers §3.5 names. A probe that does (40 `addLast`s, a
//! middle `remove(Object)`, `Iterator.remove`, then a drain) turns C2 red on
//! the same binary and the same dial:
//!
//! ```text
//!   size after 40 addLast  39 (want 40)      toArray length  34 (want 39)
//!   size after 4 It.remove 39 (want 35)      drained count   41 (want 35)
//!   pollFirst after reuse  null (want "z")
//! ```
//!
//! The same probe under arm A answers HotSpot-identically on every ArrayList,
//! `Arrays.asList` and `synchronizedMap` line, and turns `TreeMap.firstKey`
//! (wrong key), `TreeSet` (half its elements) and `ConcurrentHashMap.size`
//! (1 for a two-entry map) red under C1/C3.
//!
//! **The H2 corpus is the adjudicator for a Phase 2 retirement, and it moved
//! nothing.** 14 discovered classes, `--mode jdk-only`, dial off then dial at
//! arm A: every class that was adjudicated in both arms kept its verdict. The
//! one exception, `org.h2.test.db.TestBackup`, was re-run ABBA from a private
//! working directory and produced `MVStoreException: Chunk 2 not found` with
//! the dial **OFF** as well as on (OFF pass / ON fail / ON pass / OFF fail), so
//! it is a pre-existing flake in that class and not attributable to the change.
//! `TestCluster` also timed out in the corpus arm and passes in 161-183 s
//! standalone in BOTH arms — the corpus's `CV-TIMEOUT` is a wall-clock verdict
//! on a shared host, which is the one column that record's own header says is
//! non-metric.
//!
//! **Read arm A as an UPPER BOUND, not as this change.** The dial cannot go
//! finer than a class name, so it also yields `ArrayList.size`/`get`/`iterator`/
//! `stream`/`sort` — the `Map.values()`-entangled rows §3.2 holds back. A green
//! A is therefore strictly stronger evidence than these eight rows need; the
//! converse does not hold, and narrowing below a class name is the point at
//! which the table, not the dial, becomes the instrument.
//!
//! ## What the census says, and what it cannot say
//!
//! All seven triples are actually-taken `native-shadows-bytecode` rows on an
//! ordinary collections workload, so none of them is an inert entry.
//!
//! One measured consequence deserves its own line, because it confirms §3.3 by
//! experiment rather than by reading: arming arm A makes
//! `java/util/Arrays.copyOf([Ljava/lang/Object;I)` APPEAR in the taken-shadow
//! set, where the dial-off run does not reach it at all. That is real
//! `ArrayList.grow` funnelling through it the moment the native constructors
//! yield — the dependency that makes `Arrays.copyOf` a hard keep.
//!
//! Note what the per-run census does NOT distinguish: with the dial armed the
//! yielding row is still recorded `native-shadows-bytecode` (the record is
//! written on the yield path too), so the census cannot tell you whether the
//! dial engaged. The behavioural probe is the instrument for that. After this
//! table lands the rows change population for a different reason — a
//! `SyntheticStub` is refused at the door and never reaches step 1.
//!
//! ## What is NOT measured here, and must be re-frozen in the landing commit
//!
//! The `java/io/PrintWriter` hold above, verbatim: `java/util` is
//! Compatible-visible, so eight rows moving `Bridge` -> `SyntheticStub` move
//! `BASELINE_SYNTHETIC_STUBS_*` (`SLACK = 0`), `bridge_shadows_bytecode` and
//! the per-row kind freeze, all three keyed `25/linux`, all three re-frozen
//! from ONE Linux census in the SAME commit. Both baselines are ALREADY stale
//! and say so in their own bodies, so the predicted deltas (+8 on the stub
//! ratchet, -8 on four bridge counters, exactly eight kind-map rows) are deltas
//! on a base nobody has taken. The derivation is §4 of the P2 record; **it is a
//! diff to check, not a number to paste.** A ninth kind-map row is a finding.
//!
//! ## The door this table cannot close, and the inert-entry trap
//!
//! `register_interface_natives` registers the SAME native functions on
//! `java/util/List.iterator`, `java/util/Collection.iterator`,
//! `java/util/Set.iterator` and eight `java/util/Map` triples, and
//! `java/util/Iterator.{hasNext,next}` carry two `Bridge` registrations of their
//! own. This table retires TRIPLES, so retiring a concrete-class row leaves the
//! interface row registered and live. None of the eight above is an `iterator`
//! on a concrete collection, which is why they are the eight — but any later
//! wave that reaches for `HashSet.iterator` or `ArrayList.iterator` has to move
//! the interface rows in the same commit or measure as inert.
//!
//! And `Collections.synchronizedMap` is registered at FOUR sites, one of them
//! under an ambient `Intrinsic` (`native-builtins/src/phases_early.rs:158`).
//! Only the `Bridge` one is on today's boot path, so the retag fires — but the
//! retag in `NativeMethodRegistry::register`
//! fires only on an effective `Bridge`, so a boot-order change would make this
//! entry inert and silent. That is exactly what happened to
//! `LogRecord.<init>(Level,String)` for a day; see the source-pair section
//! above, and read the census kind rather than `CRATONVM_DBG_DROPPED_STUBS`.
//!
//! ## Widening the discriminator was half of it
//!
//! [`triple_is_retired_shadow`] used to answer `false` for anything outside
//! `java/util/logging/`. All seven keys are under `java/util/`, so ONE prefix
//! covers both populations, and the discriminator moved to `java/util/` in the
//! same edit — an entry added without that widening reads as "not retired" and
//! is invisible. `every_entry_is_reachable_through_the_predicate` is the test
//! that catches it, and
//! `the_held_collection_families_are_not_retired` is the one that says the
//! wider prefix admits sixty more families to a binary search without changing
//! any of their answers.
//!
//! # `ArrayList.get` / `size` — retired 2026-08-17, and why the hold was wrong
//!
//! G60-1 §5 N1 nominated these two on the grounds that the table "records no
//! reason" for holding them while retiring five of their siblings. The table did
//! record one, in `the_held_collection_families_are_not_retired` and in the §3.2
//! paragraph above: `Map.values()` is answered as a `java/util/ArrayList` with
//! its source map stashed in a trailing capacity slot, so `size`/`get`/
//! `iterator`/`toArray` ARE the view's implementation and retiring them freezes
//! every view at its creation time —
//! `vm/src/runtime/interpreter/native_override.rs`'s
//! `force_native_over_real_jdk_bytecode` still says exactly that, and cites H2
//! `TestAlter.testAlterTableDropIdentityColumn`.
//!
//! **Two things are wrong with that as a reason to hold a STRICT-mode
//! retirement, and one of them is structural.**
//!
//! *The structural one.* A retirement re-tags a registration `SyntheticStub`,
//! and a `SyntheticStub` registers and dispatches normally in `Compatible`. So
//! this table cannot change Compatible behaviour at all, and the H2 measurement
//! it cited is a Compatible-mode observation by necessity: `TestAlter` is a JDBC
//! test, `java.sql` is unloadable under `--jdk-only` today
//! (APP-READINESS-20260812.md §0, family A), so that vector cannot reach strict
//! mode to be broken by it. The hold imported a hazard from the one mode this
//! file provably does not touch.
//!
//! *The measured one.* No map family answers `values()` with a
//! `java/util/ArrayList` on this tree, in either mode. MEASURED on
//! `0010e134d` + this change, JDK 25.0.4+7 on Azure linux, four arms — HotSpot,
//! `--real-jdk`, `--jdk-only`, and `--jdk-only` with these two entries present
//! (probes/JdkOnlyValuesViewProbe.java, `carrier.*` lines). All eleven carriers
//! agree with HotSpot in all four arms:
//!
//! ```text
//!   HashMap.values()            java.util.HashMap$Values
//!   ConcurrentHashMap.values()  java.util.concurrent.ConcurrentHashMap$ValuesView
//!   TreeMap.values()            java.util.TreeMap$Values
//!   LinkedHashMap / Hashtable / Properties / EnumMap / IdentityHashMap /
//!   WeakHashMap / values() through the java.util.Map interface door
//!                               all the real JDK view class
//! ```
//!
//! Those are every receiver `native_map_values` is registered on
//! (`native-collections/src/lib.rs` ×3, `native-builtins/src/phases_early.rs`
//! ×3, plus the `java/util/Map` interface door), so this is the complete set and
//! not a sample. `values` is not on the force-native list, so real bytecode
//! answers it and the stashed-source-map carrier is unreachable through it.
//!
//! **The acceptance measurement, and note what instrument it needed.**
//! `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/ArrayList` — the dial the
//! 2026-08-12 wave was accepted with — turns this probe RED:
//! `values().iterator()` after a `put` throws `ConcurrentModificationException`
//! from real `ArrayList$Itr.checkForComodification`. That is the dial yielding
//! `iterator()` as well, because it cannot go finer than a class name, and it is
//! exactly the "read arm A as an UPPER BOUND" warning in §3.5 above coming due.
//! The per-triple instrument is this table, so the trial was built with these two
//! entries and nothing else: 42 checks, byte-identical to HotSpot, including
//! H2's own shape — a `ConcurrentHashMap.values()` captured before any entry
//! exists, read back with `size()` as the FIRST view method called after the
//! mutation, which is the one ordering that can tell a retired native from a live
//! one.
//!
//! ## The four this record first held back, and why the evidence was wrong
//!
//! `iterator`, `toArray` (both overloads), `isEmpty` and `contains` were held on
//! the strength of that `ConcurrentModificationException` — "the dial says at
//! least one of them is load-bearing". **It says no such thing, and the reason
//! is worth more than the four entries.**
//!
//! MEASURED 2026-08-17, one binary, three arms
//! (`probes/JdkOnlyValuesViewProbe.java`'s `carrier.*` lines, which did not
//! exist when the dial arm was first run): the eleven values-view carriers are
//! IDENTICAL in `--real-jdk`, `--jdk-only` and `--jdk-only` + the dial. Nothing
//! about the dial turns a view into an `ArrayList`. So where did a real
//! `ArrayList$Itr` come from?
//!
//! ```text
//!                           HotSpot                       CratonVM, EVERY mode
//!   values()                java.util.HashMap$Values      java.util.HashMap$Values
//!   values().iterator()     java.util.HashMap$ValueIterator   java.util.ArrayList$Itr
//!   keySet().iterator()     java.util.HashMap$KeyIterator     java.util.HashMap$KeyIterator
//! ```
//!
//! The view is real; its ITERATOR is not. `register_interface_natives` answers
//! `java/util/Collection.iterator` with a native that hands back an
//! `ArrayList$Itr` over a snapshot list, and that snapshot's `modCount` is not
//! the map's. Arm the dial and the real `ArrayList$Itr.next()` starts running
//! `checkForComodification` against it — hence the CME. The dial was measuring
//! **the interface door**, which this table's own "the door this table cannot
//! close" section warns about, and attributing it to
//! `java/util/ArrayList.iterator`.
//!
//! With the per-triple instrument instead — a trial binary carrying all five
//! registrations, `CRATONVM_DBG_DROPPED_STUBS=1` confirming each is
//! `[JDK-ONLY-REFUSED]` rather than inert:
//!
//! ```text
//!   probes/JdkOnlyValuesViewProbe.java, 67 checks   IDENTICAL to HotSpot
//!   --jdk-only  corpus   98 passed / 2 failed of 100   verdict-neutral
//!   SUITE=all   corpus   93 passed / 7 failed of 100   verdict-neutral
//! ```
//!
//! The probe's `four.*` section is written for exactly these five and exercises
//! them on three receiver shapes — a plain `ArrayList`, an `Arrays.asList` view
//! and the `java.util.List` interface door — including the two rows a native
//! that allocates rather than fills gets wrong without changing any length:
//! `toArray(new String[6])` on a 4-element list must be length 6 with a null
//! terminator at index 4, and a structural change during iteration must still
//! throw.
//!
//! **The interface-door defect the dial exposed is REAL and is not fixed here.**
//! `map.values().iterator()` is not fail-fast in either mode — a structural
//! modification mid-iteration throws `ConcurrentModificationException` on
//! HotSpot and nothing here. `Iterator.remove()` does write through, and
//! `keySet()`/`entrySet()` iterators are correct, so it is narrow. Filed as
//! `jdk-only/G63-1-the-values-view-iterator-is-not-fail-fast-20260817.md`;
//! retiring these five neither causes nor fixes it, measured both ways.
//!
//! # `Properties.getProperty` — asked, MEASURED, and NOT retired
//!
//! G60-1 §5 N2 asked whether this native is needed at all, and offered the
//! hypothesis that it backs `System.getProperties()` interop. It does, and this
//! is the mechanism. JDK 9 moved `Properties`' storage to a
//! `ConcurrentHashMap` field named `map`, and JDK 25's `getProperty` reads it
//! directly (`Properties.java:1145`). The `Properties` object
//! `System.getProperties()` returns here is VM-built and never gets that field,
//! so with the two overloads retired:
//!
//! ```text
//!   new Properties() + setProperty/load/put/remove/defaults   36 checks, HotSpot-identical
//!   System.getProperties().getProperty("java.home")           NullPointerException:
//!       Cannot invoke "java.util.concurrent.ConcurrentHashMap.get(Object)"
//!       because "this.map" is null      at java/util/Properties.getProperty
//! ```
//!
//! (probes/JdkOnlyPropsShadowProbe.java, trial build with both overloads in this
//! table.) So the answer to N2 is: the real bytecode IS correct on every axis
//! G55-1 fixed by hand — every ordinary `new Properties()` line above is
//! HotSpot-identical, because the real constructor initialises `map` — and it is
//! still not retirable, because one receiver in the VM is built without running
//! that constructor. **The precondition for retiring these two is that the
//! native which builds the system `Properties` initialise the real `map` field**
//! (or construct through the real constructor); it is not a property of
//! `getProperty` at all.
//!
//! Recording it here rather than fixing it: that native is on the boot path in
//! BOTH modes, and this file's rule is that a class's state has to become real
//! before its shadow can be retired. This is that precondition, named.
//!
//! # Why this is applied centrally
//!
//! Same reason as [`crate::no_image_receiver`]: the property is a MEASUREMENT
//! against an image, which no registration site can know, and the sites do not
//! reliably name themselves anyway — `registered_by` is a `#[track_caller]`
//! record, so a shared `with_category` helper attributes a `LogRecord` triple
//! to `native-io/src/nio_native.rs`. Four different files register these 84.
//!
//! A registration re-tagged here reports `kind_stated`: the kind WAS
//! adjudicated, by measurement rather than by an author.
//!
//! # What would invalidate this
//!
//! A `java/util/logging/` triple that stops carrying `Code` in a supported
//! image, or a strict-corpus run whose failing SET differs from the four above
//! with these retired. `regression-suite/bridge-ratchet.sh` scores the census
//! these rows leave, and `scripts/jdk-only-kind-map.py` freezes each row's kind,
//! so a silent drift in either direction fails a gate rather than a workload.

//! # Five STATELESS subsystems — retired 2026-08-19
//!
//! 227 triples, the largest wave so far and two and a half times the table it
//! joins. They were chosen by the dial and only then by the census, which is
//! the opposite of the order the first waves used and is the point.
//!
//! ## What was measured, and why the dial came first
//!
//! The module doc above records the whole-VM result: arming
//! `CRATONVM_ENFORCE_NATIVE_SHADOW` everywhere takes the strict corpus from 32
//! passed / 17 failed to 3 / 46, because under `--jdk-only` the surviving
//! bridges ARE the object model for large parts of `java.base`. That number is
//! a verdict on the WHOLE population, and it had been read as a verdict on
//! every part of it.
//!
//! It is not. Re-measured 2026-08-19, one prefix at a time, over the 36-vector
//! `--jdk-only` corpus, one binary, only the dial differing (baseline 36/36):
//!
//! ```text
//!   all                              1 / 36     <- the documented catastrophe
//!   jdk/internal/access/            22 / 36
//!   java/security/                  33 / 36
//!   java/net/  java/nio/channels/  sun/nio/ch/  34 / 36
//!   java/math/  javax/crypto/  javax/management/  javax/net/ssl/
//!     sun/security/ssl/  java/awt/image/  java/nio/file/   35 / 36
//!
//!   java/text/                      36 / 36  <- clean
//!   java/util/stream/               36 / 36  <- clean
//!   java/lang/module/               36 / 36  <- clean
//!   java/util/concurrent/atomic/    36 / 36  <- clean
//!   java/util/concurrent/locks/     36 / 36  <- clean
//!   java/lang/ref/                  36 / 36  <- clean HERE, and NOT clean
//!   sun/nio/fs/                     36 / 36  <- clean HERE, and NOT clean
//!
//!   all seven together              36 / 36  <- and they do not interact
//! ```
//!
//! **Running the seven together was a separate measurement, not an inference.**
//! Each prefix passing alone does not imply the union passes: a yielded
//! `Collectors` returning a real collector into a yielded `stream/` pipeline is
//! a pairing neither single run exercised.
//!
//! ## The screen passed two prefixes the ARM rejected — read that first
//!
//! `java/lang/ref/` and `sun/nio/fs/` are in the clean column above and are
//! **not** retired. All seven went into the table, `regression-suite/run.sh`
//! with `CRATONVM_ARGS=--jdk-only` was run over its 102 vectors, and it came
//! back **99 / 102** against a 102 / 102 baseline:
//!
//! ```text
//!   RFileTimes            plain.readAttributes.lastModified
//!                           HotSpot   2021-01-01T00:00:00Z
//!                           CratonVM  1601-01-02T20:42:25.920Z
//!   RClassUnloadSweep     payload.class.unloaded
//!   RClassUnloadSweepGen    HotSpot true / CratonVM false
//! ```
//!
//! Both diffs name their cause exactly. 1601 is the Windows FILETIME epoch:
//! real `WindowsFileAttributes` bytecode read its own `creationTime` /
//! `lastModifiedTime` fields and found them at zero, because the VM had been
//! answering from side state and never populated them. And a `Reference` that
//! yields to real bytecode stops reporting the clearing that
//! `RClassUnloadSweep` detects unloading by. Same shape both times, and the
//! same shape as every dirty prefix above: **the VM owns state that belongs to
//! the real object.**
//!
//! So the 36-vector screen is a filter, not a verdict — the corpus has no
//! file-attribute vector and no class-unloading vector, so for these two
//! subsystems it asked nothing and reported a pass. That is a population
//! narrower than the claim made from it, which is the same defect this session
//! found in the stub ratchet's CI wiring (`G89-1` §4) and, before that, twice
//! inside the ratchet file itself. It is cheap to make and it is caught only by
//! running the wider thing.
//!
//! The five that survived the arm are retired. `java/lang/ref/` and
//! `sun/nio/fs/` are held, with their diffs above as the reason and as the
//! precondition for revisiting them: populate the real fields first, then
//! re-run the arm.
//!
//! ## Then the census, to say WHICH rows
//!
//! The dial yields at dispatch when bytecode is available; retirement drops the
//! registration outright. Those agree only for rows that HAVE bytecode, so the
//! table is the census-eligible subset and not everything under the prefixes.
//! Of 522 registrations on receivers under the seven, 512 `Bridge` and 10
//! `Intrinsic`:
//!
//! ```text
//!   255  census-eligible  bridge, owns_slot, class loaded, image target has
//!                       Code, and NOT ACC_NATIVE. 227 of them are retired
//!                       here; the 28 under `java/lang/ref/` (19) and
//!                       `sun/nio/fs/` (9) are held on the arm result above
//!   195  held           loaded, but the image target carries no Code — dropping
//!                       these replaces a shadow with an UnsatisfiedLinkError
//!    40  held           class never loaded in 36 vectors, so the census asked
//!                       no question. `class-not-loaded` is not a verdict
//!                       (G88-1); it is the absence of one
//!     7  held           genuinely ACC_NATIVE — §1.5 bridges, correct as they are
//!    10  held           Intrinsic, out of scope for a §1.4 shadow
//! ```
//!
//! The three "held" reasons are each a trap this project has already fallen
//! into once, which is why they are counted here rather than filtered silently.
//!
//! ## Why these seven and not others — the shape, stated so it can be refuted
//!
//! Every retained prefix is a subsystem whose objects carry their state in REAL
//! Java fields: atomics (`value`, `array`), `ModuleDescriptor` and its nested
//! `Exports`/`Opens`/`Provides`/`Requires` records, `Collectors`' returned
//! collector, `LockSupport`'s parked bit, `DecimalFormatSymbols`. Every
//! rejected one is a subsystem where the VM owns state on the object's behalf
//! — which is G88-1 §5's finding arrived at from the other direction, by
//! execution rather than by inspection.
//!
//! `java/lang/ref/` and `sun/nio/fs/` are the useful part of that claim,
//! because they LOOK stateless and are not. A `Reference` is four fields; a
//! `WindowsFileAttributes` is a handful of longs. Nothing about their shape
//! says the VM is answering for them — only running a vector that reads those
//! fields does.
//!
//! So treat the pattern as a place to look and never as a rule to apply. The
//! dial is cheap; run it, and then run the arm.
//!
//! ## What would invalidate this
//!
//! A vector added to the `--jdk-only` corpus that exercises one of these
//! subsystems differently, or a JDK image where one of the 255 stops carrying
//! `Code`. Both fail a gate rather than a workload: `the_stateless_table_is_sorted_and_unique`
//! and `every_stateless_entry_is_reachable` here,
//! `regression-suite/bridge-ratchet.sh` on the census, and
//! `scripts/jdk-only-kind-map.py` on each row's frozen kind.
//!
//! The 36-vector screen is the cheap discriminator that says which prefixes
//! deserve a 20-minute run. `regression-suite/run.sh` with
//! `CRATONVM_ARGS=--jdk-only` over 102 vectors is the acceptance test, it
//! rejected two of the seven the screen passed, and the commit that lands this
//! records both numbers.
//!
//! # `sun/nio/fs/WindowsFileAttributes` — eight of nine, 2026-08-20 (H2-1)
//!
//! **Status: UNVERIFIED. No binary carrying this change has been built or
//! run.** It is here rather than behind a switch because the alternative
//! proves nothing: a default-off knob makes the next arm run measure the old
//! behaviour. The revert is these eight rows, the `"sun/nio/fs/"` prefix, and
//! the four tests that name them — one commit, and
//! `docs/known-issues/jdk-only/H2-1-*` names it.
//!
//! `RFileTimes` rejected this prefix on 2026-08-19 with a diff that named its
//! own cause:
//!
//! ```text
//!   plain.readAttributes.lastModified
//!     HotSpot   2021-01-01T00:00:00Z
//!     CratonVM  1601-01-02T20:42:25.920Z
//! ```
//!
//! 1601-01-01 is the Windows FILETIME epoch, and the arithmetic closes exactly:
//! 1609459200000 (2021-01-01 in Unix millis) read as 100ns ticks since 1601 is
//! 160945.92 seconds, i.e. 1601-01-02T20:42:25.920Z. The VM was writing
//! Unix-epoch millis into `creationTime`/`lastAccessTime`/`lastWriteTime` and
//! reading them back the same way — self-consistent, and agreeing with nothing.
//! JDK 25 `java.base/sun/nio/fs/WindowsFileAttributes.java` reads them through
//! `toFileTime`, which adds `WINDOWS_EPOCH_IN_100NS = -116444736000000000L` and
//! scales by 100ns.
//!
//! `native-builtins/src/phases_late/nio_file.rs` now writes those three fields
//! in FILETIME (taking the raw values straight off `MetadataExt` where a real
//! file backs them), writes the real DOS attribute word into `fileAttrs` so
//! `isReadOnly`/`isHidden`/`isArchive`/`isSystem` bytecode has something to
//! test, and writes `reparseTag = IO_REPARSE_TAG_SYMLINK` for a link —
//! `isSymbolicLink()` compares the TAG and never looks at
//! `FILE_ATTRIBUTE_REPARSE_POINT`, so a carrier with only the bit read back as
//! `isOther()`.
//!
//! **`fileKey()` is the ninth and is HELD**, and not out of caution: the real
//! body is `return null;`. See
//! `the_held_windows_attribute_triple_is_not_retired`.
//!
//! # `java/lang/ref/` — NOT retired, and this is the measurement
//!
//! The other prefix the 2026-08-19 arm rejected stays whole, on evidence rather
//! than on the earlier failure. Its census-eligible set includes
//! `Reference.<init>` and the three subclass constructors, and those are not
//! shadows in this table's sense — they are the VM's only mutator-side call to
//! `NativeContext::discover_reference`. Source-verified 2026-08-20: every
//! caller of `discover_reference` outside the collectors' own tests lives in
//! `native-builtins`, and nothing in `gc/` scans the heap for
//! `java.lang.ref.Reference` instances. A reference whose constructor yields to
//! real bytecode is therefore never discovered, never cleared, and
//! `RClassUnloadSweep`'s `payload.class.unloaded` reads `false`. That is what
//! the arm saw, and no amount of field population changes it.
//!
//! Retiring only the accessors is not the escape hatch it looks like: three of
//! them carry VM work the real bytecode has no equivalent for —
//! `Reference.get()`'s SATB keep-alive, `SoftReference.get()`'s LRU touch, and
//! `Reference.enqueue()`'s `mark_reference_manually_enqueued` — each with a
//! named in-tree defect behind it.
//!
//! What DID move for `java/lang/ref/` on 2026-08-20 is the state, in
//! `native-builtins/src/reference.rs`: `ReferenceQueue.<init>` now creates the
//! real `lock` (a field initialiser, so only the constructor can supply it, and
//! every one of `enqueue`/`poll`/`remove` opens with `synchronized (lock)`),
//! and `Reference.queue` now holds `ReferenceQueue.NULL_QUEUE` where the real
//! constructor puts it instead of a raw null. Those are the preconditions a
//! later retirement would need; they are not the retirement.

/// Every `(class, method, descriptor)` retired as a §1.4 shadow.
///
/// **Sorted, and binary-searched.** An out-of-order entry is not a style
/// question: it makes the predicate answer `false` for a row that is in the
/// table, which reads as "not retired" and is invisible. The test below
/// asserts the ordering.
static RETIRED_SHADOW_TRIPLES: &[(&str, &str, &str)] = &[
    // `java/util` collections, retired 2026-08-12. Seven triples / EIGHT
    // registrations — `<init>(I)V` is registered twice, in `native-builtins`
    // and in `native-collections`, and a retirement moves both. These are the
    // only rows of the 68-registration `java.util` slice whose §1.4
    // precondition is met: `al_slots` / `al_mod_count_slot` / `al_itr_slots`
    // resolve `elementData`, `size`, `modCount`, `cursor`, `lastRet`,
    // `expectedModCount` and `this$0` BY NAME, and `try_alloc_synthetic` loads
    // the real `java/util/ArrayList$Itr`. The other sixty are held: their state
    // is in Rust side tables or fabricated slots, so retiring them answers an
    // EMPTY collection over a populated one. See the module docs and
    // docs/known-issues/jdk-only/P2-COLLECTIONS-SHADOWS-20260812.md.
    //
    // NOT here, and load-bearing FOR these five: `java/util/Arrays.copyOf`.
    // Real `ArrayList.grow`/`toArray`/`ArrayList(Collection)` funnel through
    // it, so it stays a `Bridge` — these entries depend on that.
    ("java/util/ArrayList", "<init>", "()V"),
    ("java/util/ArrayList", "<init>", "(I)V"),
    ("java/util/ArrayList", "<init>", "(Ljava/util/Collection;)V"),
    ("java/util/ArrayList", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayList", "clear", "()V"),
    ("java/util/ArrayList", "contains", "(Ljava/lang/Object;)Z"),
    // `contains`, `get`, `isEmpty`, `iterator`, `size` and both `toArray`
    // overloads, retired 2026-08-17 — G60-1 §5 N1's two rows and the four its
    // resolution then held back. All seven are ONE measurement: the probe is
    // byte-identical to HotSpot with all of them refused, and the strict and
    // Compatible corpora are verdict-neutral. See the G60-1 sections of this
    // module's docs, including why the evidence that held four of them back was
    // an artefact of the instrument. They were held in the 2026-08-12 wave because `Map.values()` was
    // answered as an `ArrayList` with its source map stashed in a trailing
    // capacity slot, making these two the view's implementation. **That is no
    // longer what happens, and the hold also imported a Compatible-mode hazard
    // into a strict-only decision.** See the G60-1 section of this module's
    // docs for the four arms and the eleven carrier classes.
    ("java/util/ArrayList", "get", "(I)Ljava/lang/Object;"),
    ("java/util/ArrayList", "isEmpty", "()Z"),
    ("java/util/ArrayList", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/ArrayList", "size", "()I"),
    ("java/util/ArrayList", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/ArrayList", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/Arrays$ArrayList", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/Collections", "synchronizedMap", "(Ljava/util/Map;)Ljava/util/Map;"),
    // NOT here, and MEASURED not to be retirable: `java/util/Properties`'s two
    // `getProperty` overloads, G60-1 §5 N2. Real JDK 25 `getProperty` reads
    // `this.map`, the `ConcurrentHashMap` field added in JDK 9 — and the
    // `Properties` object `System.getProperties()` hands back is VM-built and
    // never has it, so retiring the reader turns
    // `System.getProperties().getProperty("java.home")` into
    // `NullPointerException: Cannot invoke "java.util.concurrent.ConcurrentHashMap.get(Object)"
    // because "this.map" is null` at `Properties.java:1145`. Every ordinary
    // `new Properties()` path is fine — the real constructor initialises `map` —
    // so the native is load-bearing for exactly one receiver, which is the
    // question N2 asked. See this module's G60-1 section.
    ("java/util/logging/FileHandler", "<init>", "()V"),
    ("java/util/logging/FileHandler", "<init>", "(Ljava/lang/String;)V"),
    ("java/util/logging/FileHandler", "close", "()V"),
    ("java/util/logging/FileHandler", "flush", "()V"),
    ("java/util/logging/FileHandler", "publish", "(Ljava/util/logging/LogRecord;)V"),
    ("java/util/logging/Handler", "<init>", "()V"),
    ("java/util/logging/Handler", "getFormatter", "()Ljava/util/logging/Formatter;"),
    ("java/util/logging/Handler", "getLevel", "()Ljava/util/logging/Level;"),
    ("java/util/logging/Handler", "isLoggable", "(Ljava/util/logging/LogRecord;)Z"),
    ("java/util/logging/Handler", "setFormatter", "(Ljava/util/logging/Formatter;)V"),
    ("java/util/logging/Handler", "setLevel", "(Ljava/util/logging/Level;)V"),
    ("java/util/logging/Level", "<clinit>", "()V"),
    ("java/util/logging/Level", "<init>", "(Ljava/lang/String;I)V"),
    ("java/util/logging/Level", "<init>", "(Ljava/lang/String;ILjava/lang/String;)V"),
    ("java/util/logging/Level", "findLevel", "(Ljava/lang/String;)Ljava/util/logging/Level;"),
    ("java/util/logging/Level", "getName", "()Ljava/lang/String;"),
    ("java/util/logging/Level", "intValue", "()I"),
    ("java/util/logging/Level", "parse", "(Ljava/lang/String;)Ljava/util/logging/Level;"),
    ("java/util/logging/Level", "toString", "()Ljava/lang/String;"),
    ("java/util/logging/LogManager", "<init>", "()V"),
    ("java/util/logging/LogManager", "addConfigurationListener", "(Ljava/lang/Runnable;)Ljava/util/logging/LogManager;"),
    ("java/util/logging/LogManager", "addLogger", "(Ljava/util/logging/Logger;)Z"),
    ("java/util/logging/LogManager", "checkAccess", "()V"),
    ("java/util/logging/LogManager", "getLogManager", "()Ljava/util/logging/LogManager;"),
    ("java/util/logging/LogManager", "getLogger", "(Ljava/lang/String;)Ljava/util/logging/Logger;"),
    ("java/util/logging/LogManager", "getLoggerNames", "()Ljava/util/Enumeration;"),
    ("java/util/logging/LogManager", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/util/logging/LogManager", "readConfiguration", "()V"),
    ("java/util/logging/LogManager", "readConfiguration", "(Ljava/io/InputStream;)V"),
    ("java/util/logging/LogManager", "removeConfigurationListener", "(Ljava/lang/Runnable;)V"),
    ("java/util/logging/LogManager", "reset", "()V"),
    ("java/util/logging/LogManager", "updateConfiguration", "(Ljava/io/InputStream;Ljava/util/function/Function;)V"),
    ("java/util/logging/LogManager", "updateConfiguration", "(Ljava/util/function/Function;)V"),
    ("java/util/logging/LogRecord", "<init>", "(Ljava/util/logging/Level;Ljava/lang/String;)V"),
    ("java/util/logging/LogRecord", "getLevel", "()Ljava/util/logging/Level;"),
    ("java/util/logging/LogRecord", "getMessage", "()Ljava/lang/String;"),
    ("java/util/logging/LogRecord", "getSequenceNumber", "()J"),
    // The source pair, retired 2026-08-12 as a SET. See the "the source pair"
    // section of this module's docs: the getters are the real getters with
    // `inferCaller()` deleted, and the setters are the real setters with
    // `needToInferCaller = false` deleted. Retiring either half alone is worse
    // than retiring neither.
    ("java/util/logging/LogRecord", "getSourceClassName", "()Ljava/lang/String;"),
    ("java/util/logging/LogRecord", "getSourceMethodName", "()Ljava/lang/String;"),
    ("java/util/logging/LogRecord", "setSourceClassName", "(Ljava/lang/String;)V"),
    ("java/util/logging/LogRecord", "setSourceMethodName", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "addHandler", "(Ljava/util/logging/Handler;)V"),
    ("java/util/logging/Logger", "config", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "config", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "entering", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "exiting", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "fine", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "fine", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "finer", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "finer", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "finest", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "finest", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "getFilter", "()Ljava/util/logging/Filter;"),
    ("java/util/logging/Logger", "getHandlers", "()[Ljava/util/logging/Handler;"),
    ("java/util/logging/Logger", "getLevel", "()Ljava/util/logging/Level;"),
    ("java/util/logging/Logger", "getLogger", "(Ljava/lang/String;)Ljava/util/logging/Logger;"),
    ("java/util/logging/Logger", "getLogger", "(Ljava/lang/String;Ljava/lang/String;)Ljava/util/logging/Logger;"),
    ("java/util/logging/Logger", "getName", "()Ljava/lang/String;"),
    ("java/util/logging/Logger", "getParent", "()Ljava/util/logging/Logger;"),
    ("java/util/logging/Logger", "getResourceBundle", "()Ljava/util/ResourceBundle;"),
    ("java/util/logging/Logger", "getResourceBundleName", "()Ljava/lang/String;"),
    ("java/util/logging/Logger", "getUseParentHandlers", "()Z"),
    ("java/util/logging/Logger", "info", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "info", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "isLoggable", "(Ljava/util/logging/Level;)Z"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/Object;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/Throwable;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;[Ljava/lang/Object;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/Throwable;Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/LogRecord;)V"),
    ("java/util/logging/Logger", "logp", "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "logp", "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/Throwable;)V"),
    ("java/util/logging/Logger", "removeHandler", "(Ljava/util/logging/Handler;)V"),
    ("java/util/logging/Logger", "setFilter", "(Ljava/util/logging/Filter;)V"),
    ("java/util/logging/Logger", "setLevel", "(Ljava/util/logging/Level;)V"),
    ("java/util/logging/Logger", "setParent", "(Ljava/util/logging/Logger;)V"),
    ("java/util/logging/Logger", "setUseParentHandlers", "(Z)V"),
    ("java/util/logging/Logger", "severe", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "severe", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "throwing", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Throwable;)V"),
    ("java/util/logging/Logger", "warning", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "warning", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/LoggingPermission", "<init>", "()V"),
    ("java/util/logging/LoggingPermission", "<init>", "(Ljava/lang/String;)V"),
    ("java/util/logging/LoggingPermission", "<init>", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/util/logging/LoggingPermission", "getName", "()Ljava/lang/String;"),
];

/// The 2026-08-19 stateless-subsystem wave: 255 triples over seven prefixes.
///
/// A SECOND table rather than 255 entries merged into the first, for two
/// reasons. The first table's entries carry per-block commentary explaining
/// individual holdbacks (`Logger.log`'s eighth overload, `Arrays.copyOf` being
/// load-bearing for the ArrayList five) that a global re-sort would scatter
/// away from the rows they explain. And these 255 were adjudicated by a
/// different method — the dial first, the census second — which is worth being
/// able to see at a glance rather than reconstructing from dates.
///
/// **Sorted and binary-searched, exactly like its sibling**, and for the same
/// reason: an out-of-order entry makes the predicate answer `false` for a row
/// that is present, which reads as "not retired" and is invisible.
static RETIRED_SHADOW_STATELESS_TRIPLES: &[(&str, &str, &str)] = &[
    // java/lang/module/Configuration — 1
    ("java/lang/module/Configuration", "modules", "()Ljava/util/Set;"),
    // java/lang/module/ModuleDescriptor — 13
    ("java/lang/module/ModuleDescriptor", "exports", "()Ljava/util/Set;"),
    ("java/lang/module/ModuleDescriptor", "isAutomatic", "()Z"),
    ("java/lang/module/ModuleDescriptor", "isOpen", "()Z"),
    ("java/lang/module/ModuleDescriptor", "mainClass", "()Ljava/util/Optional;"),
    ("java/lang/module/ModuleDescriptor", "modifiers", "()Ljava/util/Set;"),
    ("java/lang/module/ModuleDescriptor", "name", "()Ljava/lang/String;"),
    ("java/lang/module/ModuleDescriptor", "opens", "()Ljava/util/Set;"),
    ("java/lang/module/ModuleDescriptor", "packages", "()Ljava/util/Set;"),
    ("java/lang/module/ModuleDescriptor", "provides", "()Ljava/util/Set;"),
    ("java/lang/module/ModuleDescriptor", "rawVersion", "()Ljava/util/Optional;"),
    ("java/lang/module/ModuleDescriptor", "requires", "()Ljava/util/Set;"),
    ("java/lang/module/ModuleDescriptor", "uses", "()Ljava/util/Set;"),
    ("java/lang/module/ModuleDescriptor", "version", "()Ljava/util/Optional;"),
    // java/lang/module/ModuleDescriptor$Exports — 3
    ("java/lang/module/ModuleDescriptor$Exports", "compareTo", "(Ljava/lang/Object;)I"),
    ("java/lang/module/ModuleDescriptor$Exports", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/module/ModuleDescriptor$Exports", "hashCode", "()I"),
    // java/lang/module/ModuleDescriptor$Opens — 3
    ("java/lang/module/ModuleDescriptor$Opens", "compareTo", "(Ljava/lang/Object;)I"),
    ("java/lang/module/ModuleDescriptor$Opens", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/module/ModuleDescriptor$Opens", "hashCode", "()I"),
    // java/lang/module/ModuleDescriptor$Provides — 3
    ("java/lang/module/ModuleDescriptor$Provides", "compareTo", "(Ljava/lang/Object;)I"),
    ("java/lang/module/ModuleDescriptor$Provides", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/module/ModuleDescriptor$Provides", "hashCode", "()I"),
    // java/lang/module/ModuleDescriptor$Requires — 3
    ("java/lang/module/ModuleDescriptor$Requires", "compareTo", "(Ljava/lang/Object;)I"),
    ("java/lang/module/ModuleDescriptor$Requires", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/module/ModuleDescriptor$Requires", "hashCode", "()I"),
    // java/lang/module/ModuleFinder — 1
    ("java/lang/module/ModuleFinder", "ofSystem", "()Ljava/lang/module/ModuleFinder;"),
    // java/lang/module/ModuleReference — 1
    ("java/lang/module/ModuleReference", "descriptor", "()Ljava/lang/module/ModuleDescriptor;"),
    // java/text/DecimalFormatSymbols — 2
    ("java/text/DecimalFormatSymbols", "getInstance", "(Ljava/util/Locale;)Ljava/text/DecimalFormatSymbols;"),
    ("java/text/DecimalFormatSymbols", "initialize", "(Ljava/util/Locale;)V"),
    // java/text/ParseException — 1
    ("java/text/ParseException", "<init>", "(Ljava/lang/String;I)V"),
    // java/util/concurrent/atomic/AtomicBoolean — 1
    ("java/util/concurrent/atomic/AtomicBoolean", "<init>", "(Z)V"),
    // java/util/concurrent/atomic/AtomicInteger — 17
    ("java/util/concurrent/atomic/AtomicInteger", "<init>", "()V"),
    ("java/util/concurrent/atomic/AtomicInteger", "<init>", "(I)V"),
    ("java/util/concurrent/atomic/AtomicInteger", "addAndGet", "(I)I"),
    ("java/util/concurrent/atomic/AtomicInteger", "compareAndSet", "(II)Z"),
    ("java/util/concurrent/atomic/AtomicInteger", "decrementAndGet", "()I"),
    ("java/util/concurrent/atomic/AtomicInteger", "get", "()I"),
    ("java/util/concurrent/atomic/AtomicInteger", "getAndAdd", "(I)I"),
    ("java/util/concurrent/atomic/AtomicInteger", "getAndDecrement", "()I"),
    ("java/util/concurrent/atomic/AtomicInteger", "getAndIncrement", "()I"),
    ("java/util/concurrent/atomic/AtomicInteger", "getAndSet", "(I)I"),
    ("java/util/concurrent/atomic/AtomicInteger", "incrementAndGet", "()I"),
    ("java/util/concurrent/atomic/AtomicInteger", "intValue", "()I"),
    ("java/util/concurrent/atomic/AtomicInteger", "lazySet", "(I)V"),
    ("java/util/concurrent/atomic/AtomicInteger", "longValue", "()J"),
    ("java/util/concurrent/atomic/AtomicInteger", "set", "(I)V"),
    ("java/util/concurrent/atomic/AtomicInteger", "toString", "()Ljava/lang/String;"),
    ("java/util/concurrent/atomic/AtomicInteger", "weakCompareAndSet", "(II)Z"),
    // java/util/concurrent/atomic/AtomicIntegerArray — 26
    ("java/util/concurrent/atomic/AtomicIntegerArray", "<init>", "(I)V"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "addAndGet", "(II)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "compareAndExchange", "(III)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "compareAndExchangeAcquire", "(III)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "compareAndExchangeRelease", "(III)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "compareAndSet", "(III)Z"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "decrementAndGet", "(I)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "get", "(I)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "getAcquire", "(I)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "getAndAdd", "(II)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "getAndDecrement", "(I)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "getAndIncrement", "(I)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "getAndSet", "(II)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "getOpaque", "(I)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "getPlain", "(I)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "incrementAndGet", "(I)I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "lazySet", "(II)V"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "length", "()I"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "set", "(II)V"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "setOpaque", "(II)V"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "setPlain", "(II)V"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "setRelease", "(II)V"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "weakCompareAndSet", "(III)Z"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "weakCompareAndSetAcquire", "(III)Z"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "weakCompareAndSetPlain", "(III)Z"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "weakCompareAndSetRelease", "(III)Z"),
    // java/util/concurrent/atomic/AtomicLong — 16
    ("java/util/concurrent/atomic/AtomicLong", "<init>", "()V"),
    ("java/util/concurrent/atomic/AtomicLong", "<init>", "(J)V"),
    ("java/util/concurrent/atomic/AtomicLong", "addAndGet", "(J)J"),
    ("java/util/concurrent/atomic/AtomicLong", "compareAndSet", "(JJ)Z"),
    ("java/util/concurrent/atomic/AtomicLong", "decrementAndGet", "()J"),
    ("java/util/concurrent/atomic/AtomicLong", "get", "()J"),
    ("java/util/concurrent/atomic/AtomicLong", "getAndAdd", "(J)J"),
    ("java/util/concurrent/atomic/AtomicLong", "getAndDecrement", "()J"),
    ("java/util/concurrent/atomic/AtomicLong", "getAndIncrement", "()J"),
    ("java/util/concurrent/atomic/AtomicLong", "getAndSet", "(J)J"),
    ("java/util/concurrent/atomic/AtomicLong", "incrementAndGet", "()J"),
    ("java/util/concurrent/atomic/AtomicLong", "intValue", "()I"),
    ("java/util/concurrent/atomic/AtomicLong", "lazySet", "(J)V"),
    ("java/util/concurrent/atomic/AtomicLong", "longValue", "()J"),
    ("java/util/concurrent/atomic/AtomicLong", "set", "(J)V"),
    ("java/util/concurrent/atomic/AtomicLong", "weakCompareAndSet", "(JJ)Z"),
    // java/util/concurrent/atomic/AtomicLongArray — 26
    ("java/util/concurrent/atomic/AtomicLongArray", "<init>", "(I)V"),
    ("java/util/concurrent/atomic/AtomicLongArray", "addAndGet", "(IJ)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "compareAndExchange", "(IJJ)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "compareAndExchangeAcquire", "(IJJ)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "compareAndExchangeRelease", "(IJJ)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "compareAndSet", "(IJJ)Z"),
    ("java/util/concurrent/atomic/AtomicLongArray", "decrementAndGet", "(I)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "get", "(I)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "getAcquire", "(I)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "getAndAdd", "(IJ)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "getAndDecrement", "(I)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "getAndIncrement", "(I)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "getAndSet", "(IJ)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "getOpaque", "(I)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "getPlain", "(I)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "incrementAndGet", "(I)J"),
    ("java/util/concurrent/atomic/AtomicLongArray", "lazySet", "(IJ)V"),
    ("java/util/concurrent/atomic/AtomicLongArray", "length", "()I"),
    ("java/util/concurrent/atomic/AtomicLongArray", "set", "(IJ)V"),
    ("java/util/concurrent/atomic/AtomicLongArray", "setOpaque", "(IJ)V"),
    ("java/util/concurrent/atomic/AtomicLongArray", "setPlain", "(IJ)V"),
    ("java/util/concurrent/atomic/AtomicLongArray", "setRelease", "(IJ)V"),
    ("java/util/concurrent/atomic/AtomicLongArray", "weakCompareAndSet", "(IJJ)Z"),
    ("java/util/concurrent/atomic/AtomicLongArray", "weakCompareAndSetAcquire", "(IJJ)Z"),
    ("java/util/concurrent/atomic/AtomicLongArray", "weakCompareAndSetPlain", "(IJJ)Z"),
    ("java/util/concurrent/atomic/AtomicLongArray", "weakCompareAndSetRelease", "(IJJ)Z"),
    // java/util/concurrent/atomic/AtomicMarkableReference — 8
    ("java/util/concurrent/atomic/AtomicMarkableReference", "<init>", "(Ljava/lang/Object;Z)V"),
    ("java/util/concurrent/atomic/AtomicMarkableReference", "attemptMark", "(Ljava/lang/Object;Z)Z"),
    ("java/util/concurrent/atomic/AtomicMarkableReference", "compareAndSet", "(Ljava/lang/Object;Ljava/lang/Object;ZZ)Z"),
    ("java/util/concurrent/atomic/AtomicMarkableReference", "get", "([Z)Ljava/lang/Object;"),
    ("java/util/concurrent/atomic/AtomicMarkableReference", "getReference", "()Ljava/lang/Object;"),
    ("java/util/concurrent/atomic/AtomicMarkableReference", "isMarked", "()Z"),
    ("java/util/concurrent/atomic/AtomicMarkableReference", "set", "(Ljava/lang/Object;Z)V"),
    ("java/util/concurrent/atomic/AtomicMarkableReference", "weakCompareAndSet", "(Ljava/lang/Object;Ljava/lang/Object;ZZ)Z"),
    // java/util/concurrent/atomic/AtomicReference — 8
    ("java/util/concurrent/atomic/AtomicReference", "<init>", "()V"),
    ("java/util/concurrent/atomic/AtomicReference", "<init>", "(Ljava/lang/Object;)V"),
    ("java/util/concurrent/atomic/AtomicReference", "compareAndSet", "(Ljava/lang/Object;Ljava/lang/Object;)Z"),
    ("java/util/concurrent/atomic/AtomicReference", "get", "()Ljava/lang/Object;"),
    ("java/util/concurrent/atomic/AtomicReference", "getAndSet", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/util/concurrent/atomic/AtomicReference", "lazySet", "(Ljava/lang/Object;)V"),
    ("java/util/concurrent/atomic/AtomicReference", "set", "(Ljava/lang/Object;)V"),
    ("java/util/concurrent/atomic/AtomicReference", "toString", "()Ljava/lang/String;"),
    // java/util/concurrent/atomic/AtomicStampedReference — 8
    ("java/util/concurrent/atomic/AtomicStampedReference", "<init>", "(Ljava/lang/Object;I)V"),
    ("java/util/concurrent/atomic/AtomicStampedReference", "attemptStamp", "(Ljava/lang/Object;I)Z"),
    ("java/util/concurrent/atomic/AtomicStampedReference", "compareAndSet", "(Ljava/lang/Object;Ljava/lang/Object;II)Z"),
    ("java/util/concurrent/atomic/AtomicStampedReference", "get", "([I)Ljava/lang/Object;"),
    ("java/util/concurrent/atomic/AtomicStampedReference", "getReference", "()Ljava/lang/Object;"),
    ("java/util/concurrent/atomic/AtomicStampedReference", "getStamp", "()I"),
    ("java/util/concurrent/atomic/AtomicStampedReference", "set", "(Ljava/lang/Object;I)V"),
    ("java/util/concurrent/atomic/AtomicStampedReference", "weakCompareAndSet", "(Ljava/lang/Object;Ljava/lang/Object;II)Z"),
    // java/util/concurrent/atomic/DoubleAdder — 5
    ("java/util/concurrent/atomic/DoubleAdder", "<init>", "()V"),
    ("java/util/concurrent/atomic/DoubleAdder", "add", "(D)V"),
    ("java/util/concurrent/atomic/DoubleAdder", "doubleValue", "()D"),
    ("java/util/concurrent/atomic/DoubleAdder", "reset", "()V"),
    ("java/util/concurrent/atomic/DoubleAdder", "sum", "()D"),
    // java/util/concurrent/atomic/LongAdder — 10
    ("java/util/concurrent/atomic/LongAdder", "<init>", "()V"),
    ("java/util/concurrent/atomic/LongAdder", "add", "(J)V"),
    ("java/util/concurrent/atomic/LongAdder", "decrement", "()V"),
    ("java/util/concurrent/atomic/LongAdder", "increment", "()V"),
    ("java/util/concurrent/atomic/LongAdder", "intValue", "()I"),
    ("java/util/concurrent/atomic/LongAdder", "longValue", "()J"),
    ("java/util/concurrent/atomic/LongAdder", "reset", "()V"),
    ("java/util/concurrent/atomic/LongAdder", "sum", "()J"),
    ("java/util/concurrent/atomic/LongAdder", "sumThenReset", "()J"),
    ("java/util/concurrent/atomic/LongAdder", "toString", "()Ljava/lang/String;"),
    // java/util/concurrent/locks/AbstractOwnableSynchronizer — 1
    ("java/util/concurrent/locks/AbstractOwnableSynchronizer", "setExclusiveOwnerThread", "(Ljava/lang/Thread;)V"),
    // java/util/concurrent/locks/AbstractQueuedLongSynchronizer — 3
    ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "compareAndSetState", "(JJ)Z"),
    ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "getState", "()J"),
    ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "setState", "(J)V"),
    // java/util/concurrent/locks/LockSupport — 7
    ("java/util/concurrent/locks/LockSupport", "getBlocker", "(Ljava/lang/Thread;)Ljava/lang/Object;"),
    ("java/util/concurrent/locks/LockSupport", "park", "()V"),
    ("java/util/concurrent/locks/LockSupport", "park", "(Ljava/lang/Object;)V"),
    ("java/util/concurrent/locks/LockSupport", "parkNanos", "(J)V"),
    ("java/util/concurrent/locks/LockSupport", "parkNanos", "(Ljava/lang/Object;J)V"),
    ("java/util/concurrent/locks/LockSupport", "parkUntil", "(Ljava/lang/Object;J)V"),
    ("java/util/concurrent/locks/LockSupport", "unpark", "(Ljava/lang/Thread;)V"),
    // java/util/stream/Collectors — 34
    ("java/util/stream/Collectors", "averagingDouble", "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "averagingInt", "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "averagingLong", "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "collectingAndThen", "(Ljava/util/stream/Collector;Ljava/util/function/Function;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "counting", "()Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "filtering", "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "groupingBy", "(Ljava/util/function/Function;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "groupingBy", "(Ljava/util/function/Function;Ljava/util/function/Supplier;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "groupingBy", "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "joining", "()Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "joining", "(Ljava/lang/CharSequence;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "joining", "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "mapping", "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "maxBy", "(Ljava/util/Comparator;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "minBy", "(Ljava/util/Comparator;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "partitioningBy", "(Ljava/util/function/Predicate;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "partitioningBy", "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "summarizingDouble", "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "summarizingInt", "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "summarizingLong", "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "summingDouble", "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "summingInt", "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "summingLong", "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "teeing", "(Ljava/util/stream/Collector;Ljava/util/stream/Collector;Ljava/util/function/BiFunction;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toCollection", "(Ljava/util/function/Supplier;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toList", "()Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toMap", "(Ljava/util/function/Function;Ljava/util/function/Function;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toMap", "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toMap", "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;Ljava/util/function/Supplier;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toSet", "()Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toUnmodifiableList", "()Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toUnmodifiableMap", "(Ljava/util/function/Function;Ljava/util/function/Function;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toUnmodifiableMap", "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;"),
    ("java/util/stream/Collectors", "toUnmodifiableSet", "()Ljava/util/stream/Collector;"),
    // java/util/stream/DoubleStream — 5
    ("java/util/stream/DoubleStream", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/stream/DoubleStream", "of", "(D)Ljava/util/stream/DoubleStream;"),
    ("java/util/stream/DoubleStream", "parallel", "()Ljava/util/stream/BaseStream;"),
    ("java/util/stream/DoubleStream", "sequential", "()Ljava/util/stream/BaseStream;"),
    ("java/util/stream/DoubleStream", "spliterator", "()Ljava/util/Spliterator;"),
    // java/util/stream/IntStream — 7
    ("java/util/stream/IntStream", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/stream/IntStream", "of", "(I)Ljava/util/stream/IntStream;"),
    ("java/util/stream/IntStream", "parallel", "()Ljava/util/stream/BaseStream;"),
    ("java/util/stream/IntStream", "range", "(II)Ljava/util/stream/IntStream;"),
    ("java/util/stream/IntStream", "rangeClosed", "(II)Ljava/util/stream/IntStream;"),
    ("java/util/stream/IntStream", "sequential", "()Ljava/util/stream/BaseStream;"),
    ("java/util/stream/IntStream", "spliterator", "()Ljava/util/Spliterator;"),
    // java/util/stream/LongStream — 7
    ("java/util/stream/LongStream", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/stream/LongStream", "of", "(J)Ljava/util/stream/LongStream;"),
    ("java/util/stream/LongStream", "parallel", "()Ljava/util/stream/BaseStream;"),
    ("java/util/stream/LongStream", "range", "(JJ)Ljava/util/stream/LongStream;"),
    ("java/util/stream/LongStream", "rangeClosed", "(JJ)Ljava/util/stream/LongStream;"),
    ("java/util/stream/LongStream", "sequential", "()Ljava/util/stream/BaseStream;"),
    ("java/util/stream/LongStream", "spliterator", "()Ljava/util/Spliterator;"),
    // java/util/stream/ReferencePipeline — 2
    ("java/util/stream/ReferencePipeline", "collect", "(Ljava/util/function/Supplier;Ljava/util/function/BiConsumer;Ljava/util/function/BiConsumer;)Ljava/lang/Object;"),
    ("java/util/stream/ReferencePipeline", "collect", "(Ljava/util/stream/Collector;)Ljava/lang/Object;"),
    // java/util/stream/Stream — 5
    ("java/util/stream/Stream", "concat", "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;"),
    ("java/util/stream/Stream", "empty", "()Ljava/util/stream/Stream;"),
    ("java/util/stream/Stream", "of", "(Ljava/lang/Object;)Ljava/util/stream/Stream;"),
    ("java/util/stream/Stream", "of", "([Ljava/lang/Object;)Ljava/util/stream/Stream;"),
    ("java/util/stream/Stream", "toList", "()Ljava/util/List;"),
    // sun/nio/fs/WindowsFileAttributes — 8 of the 9 the 2026-08-19 census
    // found, added 2026-08-20 (H2-1). `fileKey` is the ninth and is HELD;
    // see the header block for why, and `the_held_windows_attribute_triple_
    // is_not_retired` for the pin.
    ("sun/nio/fs/WindowsFileAttributes", "creationTime", "()Ljava/nio/file/attribute/FileTime;"),
    ("sun/nio/fs/WindowsFileAttributes", "isDirectory", "()Z"),
    ("sun/nio/fs/WindowsFileAttributes", "isOther", "()Z"),
    ("sun/nio/fs/WindowsFileAttributes", "isRegularFile", "()Z"),
    ("sun/nio/fs/WindowsFileAttributes", "isSymbolicLink", "()Z"),
    ("sun/nio/fs/WindowsFileAttributes", "lastAccessTime", "()Ljava/nio/file/attribute/FileTime;"),
    ("sun/nio/fs/WindowsFileAttributes", "lastModifiedTime", "()Ljava/nio/file/attribute/FileTime;"),
    ("sun/nio/fs/WindowsFileAttributes", "size", "()J"),
];

/// Class-name prefixes any retired triple must fall under.
///
/// A cheap discriminator in front of two binary searches: almost no
/// registration is under any of these, so the common case costs one failed
/// prefix compare.
///
/// It is NOT the definition of what is retired — the tables are. A prefix here
/// that no table entry uses retires nothing; a table entry outside every prefix
/// here is UNREACHABLE and answers `false`, which is the silent failure
/// `every_entry_is_reachable_through_the_predicate` and its sibling exist to
/// catch.
const RETIRED_SHADOW_PREFIXES: &[&str] = &[
    // 2026-09-11, lane 4 wave 2. The NARROW prefix, for the reason the
    // `sun/nio/fs/` note below gives: `jdk/internal/foreign/` as a whole has
    // nothing retirable under it. The segment, arena and session carriers are
    // this VM's OWN allocation shape by a decision on record
    // (`docs/known-issues/jdk-only/the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`),
    // laid out deliberately unlike the JDK's, so their real bodies must never
    // run; only the LAYOUT carriers are minted on their real classes with
    // their real fields. `RETIRED_SHADOW_L4_FFM_TRIPLES` retires 137 rows over
    // the nine `ValueLayouts$Of*Impl` classes under this prefix, and nothing
    // else under it is retired -- not the group layouts beside them, and not
    // `varHandle` on these nine.
    "jdk/internal/foreign/layout/",
    // 2026-09-11, lane 4 wave 1. The two wide prefixes of the largest lane:
    // 1,398 bucket-A/B shadows over 142 classes sit under them. Same rule as
    // every prefix above -- this admits those packages to one extra binary
    // search each, and `RETIRED_SHADOW_L4_TRIPLES` decides what is retired. It
    // retires 140 rows over 10 classes; `jdk/internal/foreign` is deliberately
    // NOT admitted, because nothing under it came through the funnel.
    "java/io/",
    "java/nio/",
    "java/lang/module/",
    "java/text/",
    "java/util/",
    // 2026-08-20, H2-1. Deliberately the NARROW prefix and not `sun/nio/`:
    // `sun/nio/ch/` scored 34/36 on the 2026-08-19 dial sweep and nothing
    // under it is retirable. `java/lang/ref/` is still absent on purpose —
    // see the `sun/nio/fs/` block in this module's header.
    "sun/nio/fs/",
    // 2026-09-10, lane 2. `java/math/` cost one corpus vector of 36 on the
    // 2026-08-19 package screen, which is why it was never admitted; re-taking
    // that screen per CLASS rather than per package found the cost was
    // `BigInteger`'s and named it — `RJdkSecurity`, on `2^127-1 must be
    // prime`. See `RETIRED_SHADOW_L2_TRIPLES` for what that turned out to be.
    // `java/lang/` is the wider of the two and admits the whole package tree to
    // one extra binary search; the table decides what is retired, and it
    // retires three rows under it.
    "java/lang/",
    "java/math/",
    // 2026-08-30, Phase 2. **This narrows the note above rather than
    // overruling it.** That note is a PACKAGE verdict from a package-scoped
    // dial sweep, and it is still the right default: this lane's own
    // whole-corpus run agrees that `sun/nio/ch/` as a whole is not retirable.
    // What is retired under this prefix is ONE triple, measured on its own —
    // see `RETIRED_SHADOW_PHASE2_TRIPLES`. A prefix admits a package to the
    // binary search; the table decides what is retired, and it retires one row.
    "sun/nio/ch/",
    // 2026-09-10, the `--jdk-only` loader-and-bootstrap lane. The measurement
    // is in
    // `docs/known-issues/jdk-only/the-builtin-classloader-could-not-link-and-getname-was-never-tagged-20260910.md`;
    // the lane's own page retired with it.
    // Two NARROW prefixes, for the reason the `sun/nio/ch/` note above gives: a
    // prefix only admits a class to the binary search, and
    // `RETIRED_SHADOW_L7_TRIPLES` retires exactly two rows under them. In
    // particular `java/security/SecureClassLoader` is spelled out rather than
    // `java/security/`, which is lane 6's whole prefix set and is NOT admitted
    // here. `the_l7_prefixes_retire_only_the_two_measured_rows` is the guard.
    //
    // Lane 2's `java/lang/` above arrived in the SAME merge and subsumes
    // `java/lang/ClassLoader` entirely. The entry stays anyway: it is what
    // makes lane 7's two rows reachable on their own terms, and deleting it
    // would silently hand that reachability to a prefix another lane owns and
    // could re-narrow. A duplicate prefix costs one `starts_with` on a path
    // that has already matched.
    "java/lang/ClassLoader",
    "java/security/SecureClassLoader",
    // 2026-09-10, lane 5 (`java/util/concurrent/`, `jdk/internal/misc/`,
    // `sun/misc/`, `java/lang/Thread*`, `jdk/internal/vm/`). `java/util/`
    // above already admits `java/util/concurrent/`; these four are the rest of
    // that lane's prefix set.
    //
    // Adding a prefix while its table is empty is PROVABLY INERT: the list is
    // only an early-out in front of the binary searches, so a wider list plus
    // an empty table answers `false` for exactly the same inputs. That is what
    // makes it safe to land the prefixes and the table in one commit —
    // `the_l5_prefixes_retire_nothing_on_their_own` is the guard that says so
    // for every triple this lane declined.
    "jdk/internal/misc/",
    "jdk/internal/vm/",
    "java/lang/Thread",
    "sun/misc/",
    // 2026-09-10, L1 wave 2. **This is L0's cell and L1 edited it**, because
    // L0's skeleton commit (lane-0 §4: nine empty tables, nine chain arms,
    // every lane's prefixes pre-added) never landed, and `java/time/` is in
    // L1's declared prefix set in the ownership table. Adding a prefix is
    // provably inert for every triple no table carries — the list is an
    // early-out in front of the binary searches, so a wider list plus the
    // same tables answers `true` for exactly the two `java/time/` rows this
    // wave adds and `false` for everything else, as before.
    "java/time/",
    // 2026-09-11, L1 wave 5. NARROW, the way lane 7's two are and for the same
    // reason: `sun/util/` is a large tree whose locale-provider half this lane
    // has measured as NOT retirable (see `RETIRED_SHADOW_L1_ZI_TRIPLES`'s doc
    // comment for the five `reached == 0` rows), and admitting the package
    // would put every one of them one binary search from a future table.
    // `the_zone_info_file_prefix_retires_only_the_two_measured_rows` is the
    // guard.
    "sun/util/calendar/ZoneInfoFile",
    // 2026-09-11, L1 wave 6. Two more NARROW spellings, for the same reason
    // the line above is narrow: the locale-provider tree's other half is
    // MEASURED not retirable (`LocaleResources` +12 on 254 engagements,
    // `CalendarDataUtility` +12 on 108 -- see
    // `RETIRED_SHADOW_L1_LP_TRIPLES`), and a `sun/util/` prefix would put
    // both one binary search from a future table.
    "sun/util/locale/provider/JRELocaleProviderAdapter",
    "sun/util/resources/LocaleData",
    // 2026-09-11, lane L3 adds NO prefix of its own, and that is the merge
    // resolution rather than an omission. This branch carried
    // `java/lang/invoke/` and `java/lang/reflect/` for
    // `RETIRED_SHADOW_L3_TRIPLES`; `dev` meanwhile added the broad
    // `java/lang/`, which already admits every one of L3's 24 rows. Two
    // narrower prefixes behind a wider one are dead weight, and the rule this
    // module states is the NARROWEST list that covers the tables -- so they
    // are dropped.
    //
    // Worth flagging to whoever owns `java/lang/`: a prefix that broad makes
    // the TABLE the only guard, where a narrow list is a second one. Nothing
    // is wrong today -- the tables still decide -- but
    // `a_prefix_alone_retires_nothing` is weaker than it reads, and narrowing
    // someone else's prefix is their call, not this lane's.
    //
    // L0 adds no prefix either, and the reason CHANGED on 2026-09-11 -- the
    // sentence here used to say "its table is EMPTY (two corpus rounds
    // withdrew all 54)", which was true of the withdrawal and is not true of
    // wave 2. L0 now carries 19 rows: 15 on `java/lang/Class` and 4 on
    // `java/lang/module/ModuleDescriptor$Version`. Both are already admitted,
    // by `java/lang/` and `java/lang/module/` respectively, so the narrowest
    // covering list still does not need an L0 entry. Same conclusion,
    // different reason, and the old reason would have read as "this lane
    // retired nothing".
    // 2026-09-11, lane L6. The whole adjudication, all 1,414 rows of it, is in
    // `docs/internal/retired/lane-6-net-security-RETIRED-20260910.md`.
    //
    // ONE prefix for a lane that owns nine, and deliberately the NARROW
    // spelling: `java/net/` and not `java/`. Under it, TWO classes of the
    // fourteen the wave tried.
    //
    // `javax/security/auth/x500/` was here through the fifth of six builds and
    // is not here now: `X500Principal` alone breaks `RSslLiveSession`, because
    // this VM fills that class's one declared `X500Name` slot with a String.
    // Every other class the lane tried was measured out the same way, by a
    // corpus vector rather than by a probe -- the table's header is the
    // ladder, one row per build.
    //
    // The lane's other eight prefixes carry no table at all. Each was armed on
    // the whole 125-probe tree and each either moved a probe AWAY from HotSpot
    // (`javax/net/`, and the four security prefixes together) or never reached
    // the dial (`sun/net/`, `jdk/net/`, `jdk/internal/net/`: 123 of 125 probes
    // VACUOUS). Adding a prefix whose table is empty is provably inert, but it
    // would read to the next lane as a claim that something under it was
    // retired, and `the_lane_l6_security_and_tls_prefixes_are_not_admitted`
    // exists to keep that claim from being made by accident.
    "java/net/",
];

/// The 2026-08-30 Phase 2 wave: ONE triple, and the size is the finding.
///
/// A THIRD table rather than an entry merged into either sibling, for the
/// reason the second gives for existing: these were adjudicated by a different
/// METHOD, and the method is the part worth being able to see at a glance.
///
/// # Why one
///
/// Phase 2 armed all 270 classes of the shadow surface, one at a time, and the
/// dial called 236 of them retire-safe. Arming those 236 together fails **54 of
/// 118 corpus vectors** and breaks **35 of 78 probe families**. So the sweep
/// produces candidates, never verdicts, and each candidate has to earn its row
/// against four preconditions:
///
///  1. the dial was ASKED — `enforcement_dial.reached > 0` for that scope, not
///     a passing vector (146 of the 236 fail this: nothing on the class was
///     ever called, so arming it changed nothing and read as the best possible
///     result);
///  2. the WHOLE probe tree, armed on that class alone, gets no worse anywhere
///     — not just the family's own probe, which is the narrowest instrument in
///     the building;
///  3. the image target carries `Code` to yield to, so the retirement does not
///     trade a shadow for an `UnsatisfiedLinkError`;
///  4. the full corpus stays at its unarmed baseline.
///
/// `sun/nio/ch/FileChannelImpl` is the receiver that came through. Armed alone:
///
/// ```text
///   L4Diag                      4 diffs from HotSpot -> 0        (9/9 yields)
///   the other 77 probes         every delta exactly 0
///   full corpus --jdk-only      118 passed, 0 failed = the unarmed baseline
/// ```
///
/// The two rows it fixes are `FileChannel.truncate(-1)`'s message: this VM
/// answered `Negative size: -1` where HotSpot answers `Negative size`. Retiring
/// it lets the real JDK validation run, which is §1.4's remedy rather than
/// maintaining message parity by hand in a native that should not be in front
/// of that bytecode at all.
///
/// # The first entry written here was `open`, and it was INERT
///
/// Worth keeping, because it is precondition 4 failing in the one direction
/// nobody expects. The dial arms a PREFIX, so "armed `sun/nio/ch/FileChannelImpl`
/// fixes L4Diag" is a claim about every triple on that receiver. Choosing which
/// one to retire then fell to precondition 4 — observed as `native-won` in the
/// unarmed corpus — and the corpus offers exactly one such triple on that
/// class, `open`. So `open` was retired, built, and measured: **L4Diag
/// unchanged at 4 diffs, every other probe unchanged.** A whole build to move
/// nothing.
///
/// The registry says why, in the column precondition 4 never consults. In an
/// L4Diag run, `truncate(J)` has `invocations: 2` and `image_declaring_method.
/// has_code: true`; `open` has `invocations: 0`. **The corpus never calls
/// `FileChannel.truncate(-1)`; the probe does.** Filtering candidates by what
/// the CORPUS dispatched therefore discards precisely the triple whose
/// retirement the PROBE measured — the two preconditions were reading different
/// workloads and only one of them was the workload the evidence came from.
///
/// So precondition 4 is really: **observed by the instrument that produced the
/// improvement**, per triple, and the way to read it is `invocations > 0` in
/// that instrument's own run — not membership in a corpus census.
///
/// # What did NOT come through, and it is the more useful half
///
/// `jdk/internal/foreign/ArenaImpl` was the other candidate, and it looked
/// better: armed, `Arena.allocate()` returns the REAL
/// `jdk.internal.foreign.NativeMemorySegmentImpl` instead of this VM's carrier,
/// taking `AbstractReceiverSweep` from 12 diffs to 6. Precondition 2 killed it:
///
/// ```text
///   FfmSegmentSweep   40 -> 181 diffs, and DIED at row 18 of 199
///   FfmCarrierProbe    0 ->   6 diffs, and died at 104 of 106
///   FfmMsgProbe        2 ->   7 diffs
/// ```
///
/// The class NAME becomes right and the segment surface stops working, because
/// the real implementation needs real state this VM does not keep. That is a
/// second, independent confirmation of the FFM contract decision recorded in
/// `docs/known-issues/jdk-only/the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`:
/// the carrier is this VM's own allocation shape, and matching the JDK's class
/// name is not a thing to fix — not by fabricating a name, and not by
/// retirement either.
static RETIRED_SHADOW_PHASE2_TRIPLES: &[(&str, &str, &str)] = &[(
    "sun/nio/ch/FileChannelImpl",
    "truncate",
    "(J)Ljava/nio/channels/FileChannel;",
)];

/// Lane L0's wave, 2026-09-10: `java.lang.Class`, `Module` and
/// `ModuleDescriptor.Version` -- 54 triples of the lane's 104, with the other
/// 50 accounted for below rather than left unexamined.
///
/// # The instrument, and why the aggregate number is the wrong one to read
///
/// `apps/probes/L0ClassModuleSurface.java`, 129 rows over the whole lane
/// surface, measured three ways against HotSpot 25.0.3+9 -- unarmed, and with
/// every native declining (`CRATONVM_ENFORCE_NATIVE_SHADOW=all`):
///
/// ```text
/// unarmed   8 diff lines of 130     armed  58 diff lines of 130
/// ```
///
/// Read as an aggregate that says "do not retire anything here", and it would
/// be the wrong conclusion drawn from a true number. Per ROW:
///
/// ```text
///  96  OK -> OK    the native is right and so is the bytecode  -> RETIRE
///   4  BAD -> OK   the native is WRONG and yielding fixes it   -> RETIRE
///  29  OK -> BAD   the native is right, yielding breaks it     -> HELD
///   0  BAD -> BAD
/// ```
///
/// The four unarmed diffs are exactly the `BAD -> OK` rows, so **every
/// disagreement this VM has with HotSpot on lane 0's surface is one that
/// retirement repairs.**
///
/// # What the four repairs are, because one is not a cosmetic
///
/// ```text
/// Module.addExports("jdk.internal.misc", unnamed) on java.base
///   HotSpot  threw java.lang.IllegalCallerException
///   native   PERMITTED
/// ModuleDescriptor.Version.parse("")
///   HotSpot  IllegalArgumentException: Empty version string
///   native   returned a Version   (validation skipped entirely)
/// Version.compareTo x2
///   native   NPE in JDK bytecode: "ts1 is null" -- `parse` built a Version
///            whose internal lists were never filled
/// ```
///
/// The first is an access-control check the native does not perform: only a
/// module may widen its own exports, and this VM let an unnamed module widen
/// `java.base`'s. The rest are the JDK's own argument validation, which is the
/// surface a retirement usually buys.
///
/// # The 23 HELD triples, each with the row that held it
///
/// ```text
/// Class.descriptorString        row 2      NPE: componentType field is null
/// Class.getModifiers            rows 14-16 wrong FLAG BITS ("public
///                                          synchronized" for Object; `static`
///                                          lost on a nested interface)
/// Class.getAnnotation*, isAnnotationPresent
///                               rows 71-77 annotations come back EMPTY
/// Class.newInstance             rows 87-88  cachedConstructor is null
/// Module.getLayer               row 103     answers false where HotSpot is true
/// Module.isExported x2          rows 108,110 answers false
/// Module.isOpen x2              family of isExported -- see the note below
/// ModuleLayer.boot/findModule/modules/configuration
///                               rows 118-121 boot() yields null
/// Class.getPackage, getResource, getResourceAsStream, Module.getResourceAsStream
///                               rows 13,81-83,105  NoClassDefFoundError,
///                                          `jdk/internal/loader/ClassLoaders`
///                                          and `BuiltinClassLoader`
/// ```
///
/// **`Module.isOpen` is held on a judgement, not a measurement, and that is
/// deliberate.** Its two rows agree with HotSpot when yielded -- but they agree
/// at `false`, which is also what a blanket yield returns for everything in
/// this family, and its sibling `isExported` demonstrably breaks. An agreement
/// that cannot be distinguished from the default answer is not evidence. It
/// needs a receiver whose correct answer is `true`, which `java.base` does not
/// provide to an unnamed module; until someone builds that fixture, held.
///
/// The last group is not this lane's to fix: the builtin class loader
/// hierarchy does not link, which is lane L7's named blocker. Those four are
/// held *pending L7*, not held on their own merits.
///
/// # The 27 not retired for want of an instrument
///
/// Precondition 4 is per-instrument and these have `invocations == 0` even in
/// the probe written to reach them. Twelve cannot be called from Java at all --
/// `Class.getClassLoader0`, `getEnumConstantsShared`, `reflectionData`,
/// `newReflectionData`, `setSigners`, the three `Class$Atomic` CAS methods,
/// `Class$ReflectionData.<init>` and the five `ClassFrameInfo` accessors are
/// package-private plumbing called only from inside `java.lang.Class` and the
/// stack walker. Eight are `Module.implAdd*`, reached only through
/// `AccessibleObject` paths this probe does not take.
///
/// `ModuleDescriptor$Version.compareTo` and `ClassValue.remove` are the
/// interesting two: the probe DOES exercise both, and both still count zero.
/// The row-126 failure names `ts1`, a local in `Version.compareTo`'s own
/// bytecode, so the JDK's method served the call and the registration was
/// never dispatched -- an inert row, which is a finding rather than a
/// retirement, and it is why the count is taken per triple and not per row.
/// Lane L3 wave 1 -- core reflection's metadata accessors, 24 triples.
///
/// Measured 2026-09-10 with `apps/probes/L3ReflectInvokeSurface.java` (254
/// rows, oracle HotSpot 25.0.3+9, deterministic across two runs) against a
/// release build of the merged tree. Lane 3's population is **242** bucket-A/B
/// rows over 35 classes, re-derived from a `--dump-native-registry` taken with
/// `--explain-jdk-only` -- not the lane page's 251, which came from a different
/// tree and counted rows lane T owns.
///
/// # The aggregate said "retire nothing" and it was wrong again
///
/// ```text
/// d(hs,base) 68 diff lines   d(hs,armed) 224   delta +156
///
/// per ROW (254 rows)   OK -> OK   132   retire
///                      BAD -> OK   10   yielding FIXES it -> retire
///                      OK -> BAD   88   hold
///                      BAD -> BAD  24   investigate
/// ```
///
/// 142 of 254 rows are retirable behind a `delta` of +156. Same lesson as L0,
/// four times the scale.
///
/// **The ten `BAD -> OK` rows are the reason to do this at all.** Yielding
/// repairs the `Field`/`Method`/`Constructor` copy model
/// (`getDeclaredField("x") == getDeclaredField("x")` is `true` here and `false`
/// on HotSpot), and with it the `setAccessible` LEAK that follows from handing
/// back the same object -- one `setAccessible(true)` currently grants access to
/// every holder of that member. It also restores HotSpot's
/// `IllegalAccessException` on `privateLookupIn(java.base)`, which is the
/// documented one-directional residual in `lk_enforce_find_access`. A
/// retirement that closes an access-control gap is worth more than one that
/// deletes code.
///
/// # 24 of 242, and why the other 218 are not here
///
/// A row is not a triple, and this table only contains triples a row-to-triple
/// mapping can justify: the probe tag names one method of one class, the census
/// holds exactly ONE A/B triple for that name, `invocations > 0` in the probe's
/// own run, and **every** row touching it is `OK -> OK` or `BAD -> OK`. 48
/// (class, name) pairs were reached and rejected, each with its reason
/// recorded; the four rules that did the rejecting:
///
/// * **`invocations == 0`** -- precondition 4, per triple, from the dump.
/// * **held** -- any `OK -> BAD` row. 88 rows hold, 32 of them `Field`'s
///   primitive accessors, which the same run explains: the
///   descriptor-coercion census reports **105** field reads whose value
///   contradicted the slot's descriptor and was DESTROYED. Ten more are the
///   generic-signature family, where yielding erases
///   `Map<String,List<T>>` to `Map` and `T` to `Number`.
/// * **`BAD -> BAD`** -- the bytecode is wrong too, so "yielding is correct
///   here" is false. This is what removed `Method.invoke` and
///   `Constructor.newInstance`, which had looked like clean seven-row and
///   five-row keeps until the non-nestmate rows 247 and 248 were attributed to
///   them. Not a regression; not a justified retirement either.
/// * **agreement at a DEFAULT value** -- L0 held `Module.isOpen` for this and
///   it removed twelve entries here. The sharpest is `Field.setBoolean`, whose
///   only row sets `z` to `false` and reads it back: yielded `getBoolean`
///   answers `false` by default, and `Field.getBoolean` is PROVEN broken by
///   row 15. Retiring `setBoolean` on `false == false` would be exactly the
///   mistake the rule exists to prevent.
///
/// # What is NOT retired, structurally
///
/// The whole `java.lang.invoke` surface except three `MethodType` accessors.
/// §4 of the lane page predicted `MemberName`/`MethodHandle` would be
/// VM-coupled and the measurement agrees: 14 `MethodHandles` rows, 8
/// `MethodHandle` rows and 5 `CallSite` rows hold or are wrong both ways, and
/// the run's uninstantiable-receiver census names `MethodHandle` and
/// `VarHandle` as abstract classes a native instantiates. Those rows need the
/// reviewed-`Intrinsic` protocol or a repair, not a retirement.
static RETIRED_SHADOW_L3_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/invoke/MethodType", "parameterCount", "()I"),
    (
        "java/lang/invoke/MethodType",
        "returnType",
        "()Ljava/lang/Class;",
    ),
    (
        "java/lang/invoke/MethodType",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getGenericParameterTypes",
        "()[Ljava/lang/reflect/Type;",
    ),
    ("java/lang/reflect/Constructor", "getModifiers", "()I"),
    (
        "java/lang/reflect/Constructor",
        "getName",
        "()Ljava/lang/String;",
    ),
    ("java/lang/reflect/Constructor", "getParameterCount", "()I"),
    (
        "java/lang/reflect/Constructor",
        "getParameterTypes",
        "()[Ljava/lang/Class;",
    ),
    (
        "java/lang/reflect/Field",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
    ),
    ("java/lang/reflect/Field", "getModifiers", "()I"),
    ("java/lang/reflect/Field", "getName", "()Ljava/lang/String;"),
    ("java/lang/reflect/Field", "getType", "()Ljava/lang/Class;"),
    (
        "java/lang/reflect/Method",
        "getAnnotatedReturnType",
        "()Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/Method",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
    ),
    (
        "java/lang/reflect/Method",
        "getExceptionTypes",
        "()[Ljava/lang/Class;",
    ),
    (
        "java/lang/reflect/Method",
        "getGenericExceptionTypes",
        "()[Ljava/lang/reflect/Type;",
    ),
    ("java/lang/reflect/Method", "getModifiers", "()I"),
    (
        "java/lang/reflect/Method",
        "getName",
        "()Ljava/lang/String;",
    ),
    ("java/lang/reflect/Method", "getParameterCount", "()I"),
    (
        "java/lang/reflect/Method",
        "getParameterTypes",
        "()[Ljava/lang/Class;",
    ),
    (
        "java/lang/reflect/Method",
        "getReturnType",
        "()Ljava/lang/Class;",
    ),
    ("java/lang/reflect/Method", "setAccessible", "(Z)V"),
    (
        "java/lang/reflect/Method",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// # 54 became 38 became 29, in two corpus rounds
///
/// **Round 2.** With the first 16 withdrawn the arm went 97 -> **127 of 132**,
/// and the five survivors were all mine: `RClassUnloadSweep`,
/// `RClassUnloadSweepGen`, `RLoaderIdentity`, `RJdkModule`,
/// `RServiceLoaderDoubleSource`. Through the harness, control 5/0 against
/// current 0/5.
///
/// All nine `java/lang/Module` triples are withdrawn, and the reason is the
/// SAME default-value trap a third time. The probe validated
/// `Module.getName`, `getDescriptor`, `canRead` and `getClassLoader` only at
/// their default answers -- an UNNAMED module's name (`null`), an unnamed
/// module's descriptor (`null`), `canRead` in its TRUE direction, and
/// "java.base's loader is null". **A retirement answering `null`/`true` for
/// everything satisfies all four**, so all four read as agreements. The corpus
/// asserts the other side of each, and eight discriminating rows added
/// afterwards found `Module.getClassLoader` answering `false` for a
/// PLATFORM-loaded module where HotSpot and the control binary both say
/// `true`.
///
/// Seven of those eight new rows PASS, so `getName`, `getDescriptor`,
/// `canRead` and `getPackages` are not individually disproven -- they are
/// withdrawn as a family because the wave that admitted them cannot be trusted
/// per triple, and a later wave can re-earn them one at a time with a
/// discriminating row each. That is cheaper than shipping a third round.
///
/// **A methodological note on the A/B that nearly went wrong.** Run directly
/// with `-cp regression-suite/build`, four of the five vectors failed on the
/// CONTROL binary too and read as "not mine". They need a `--module-path` that
/// `run.sh` supplies. Re-run through the harness, control scored 5/0. **An
/// invocation that is not the harness's own is not a control** -- it would
/// have dismissed four real regressions.
///
/// # 54 became 38: the DIAL is not a faithful simulator of a RETIREMENT
///
/// **This table shipped 16 triples the corpus disproved, and the reason is
/// methodological rather than clerical.** Lane L0's wave was validated with
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` -- the shadow dial, which makes a native
/// DECLINE at a dispatch door -- and never with the retirement itself, which
/// refuses the REGISTRATION. Those are not the same experiment, and that run's
/// own census said so in a line nobody read:
///
/// ```text
/// [DIAL_DOOR_CENSUS] armed=true reached=3680 yielded=3593 leaked=87
/// ```
///
/// **87 dispatches reached the dial and were not yielded.** A leaked row
/// reports the NATIVE's answer while reading, in a three-arm diff, as "the
/// bytecode is fine here". `Class.isArray` was one of them: the armed arm
/// printed `true/false`, matching HotSpot exactly, and the real retirement
/// answers `false/false`.
///
/// Re-measured with no dial anywhere -- control binary (pre-table) against
/// retired binary against HotSpot, on the same 129-row probe:
///
/// ```text
/// 117  OK -> OK      retirement safe
///   4  BAD -> OK     retirement REPAIRS a disagreement
///   8  OK -> BAD     retirement BREAKS a correct answer   <- shipped anyway
/// ```
///
/// Those eight cost **35 of 132 vectors** on the `--jdk-only` corpus arm,
/// which had been 132/0 on four consecutive earlier binaries (`p4`, `p7`,
/// `p8`, `p9`). The arm is the only instrument that caught it, which is
/// exactly why the landing protocol lists it and why it must not be skipped
/// when a goal changes mid-wave -- this wave's arms were deferred when the
/// session moved to lane L3, and that is how the eight got in.
///
/// ## The eight, and why they are really four causes
///
/// * **The array family is ONE bad triple with a cascade.** `componentType`
///   was retired; this VM never fills the `componentType` FIELD (this table's
///   own held list records `descriptorString` NPE-ing on exactly that); and
///   JDK 22+ implements `isArray()` as `componentType != null`. So `isArray`
///   answers `false`, and `getTypeName`/`getSimpleName`/`getCanonicalName`
///   fall back to the internal form for arrays -- `[[I` where HotSpot says
///   `int[][]`. Six triples withdrawn for one root cause.
/// * **Generic and annotated signature resolution** raises
///   `TypeNotPresentException` for a type that is plainly on the class path.
///   Four triples.
/// * **The reflection-data copy model** flips: `field copies not same` goes
///   `true -> false`, so the VM stops handing out copies. Note the direction,
///   because lane L3 measured the SAME defect from the other side and found
///   that yielding REPAIRS copying for `Field`/`Method`/`Constructor`. Same
///   mechanism, opposite sign, depending on which end owns the accessor.
/// * **`Class.getClassLoader`** yields null for the application loader, and
///   **`ClassValue.get`** loses a recomputation. One triple each.
///
/// `Module.getClassLoader` is NOT withdrawn: it measured `OK -> OK` on the
/// no-dial arm and its row is not a default-value agreement.
///
/// ## What this means for the next wave, in one line
///
/// **A dial arm is a screening instrument, not evidence.** Score a retirement
/// with two BINARIES -- one without the table, one with it -- and require
/// `OK -> BAD == 0` before the corpus arm, not after. If a dial arm is used at
/// all, read its `leaked` counter first: `leaked > 0` means some rows in that
/// arm never yielded and cannot be cited.
///
/// ## Wave 2: the 19 the failing vectors never consult
///
/// The 54 were withdrawn as a block for one reason -- `OK -> BAD == 0` held on
/// the probe and the corpus still lost five vectors, and nothing said WHICH of
/// the 29 remaining rows did it. Bisecting 29 triples is a build per
/// hypothesis, 65 minutes each.
///
/// The per-vector census answers a weaker question for free: **which of the 29
/// does this vector dispatch at all?** A registration a vector never consults
/// cannot be the row that broke it. Measured on `p16` (L0 empty, so all 29
/// natives present and counted), the five vectors passing 5/0, all five
/// reports written, `saturation: none`:
///
/// ```text
/// RClassUnloadSweep           5 of 29        union = 10 triples
/// RClassUnloadSweepGen        5 of 29        desiredAssertionStatus, forName x3,
/// RJdkModule                  8 of 29        getConstructor, getDeclaredConstructor,
/// RLoaderIdentity             6 of 29        getMethod, getPackageName,
/// RServiceLoaderDoubleSource  6 of 29        isInterface, isPrimitive
/// ```
///
/// All ten are class-loading or member-lookup plumbing, which is what a
/// class-unload sweep and a two-source `ServiceLoader` lean on and what a
/// 129-row probe over `java.lang.Class` does not reach. The **19** below are
/// touched by none of the five.
///
/// ## Why that covers the corpus and not just five vectors
///
/// The round-1 arm -- binary `p14`, **38 rows**, the 19 among them -- scored
/// **127 passed, 5 failed**. So the 19 are not a hypothesis about those 127:
/// they were retired *during* that run and those 127 vectors passed anyway.
/// The five that did not pass are the five attributed above, and none of them
/// consults a row in the 19. `19 subset 29 subset 38`, so the two later
/// withdrawals only removed rows from around them. The halves close over 132:
///
/// ```text
/// 127 vectors   passed WITH these 19 retired            measured, p14
///   5 vectors   failed, and dispatch none of the 19     measured, p16 census
/// ```
///
/// That is why this wave is 19 and not a bisection: the bisection would
/// identify which of the 10 is guilty, which is a question about re-adding
/// them, not about shipping these.
///
/// What it does NOT cover, and what the arm on this binary is for: the table
/// now ships alongside L1's, L2's and L3's, and individually-safe retirements
/// can interact -- Phase 2's 236 dial-safe classes armed together broke 54 of
/// 118 vectors. Every number above was measured with L0 alone.
static RETIRED_SHADOW_L0_TRIPLES: &[(&str, &str, &str)] = &[
    // WAVE 2, 2026-09-11. The 54-row wave was withdrawn whole because no
    // instrument named the row; these 19 are the 29 that survived two
    // probe rounds, minus the 10 that the five failing corpus vectors
    // actually dispatch. Attribution is in the lane page's 7.2 and the
    // 10 are pinned out by `the_l0_attributed_triples_are_not_retired`.
    (
        "java/lang/Class",
        "asSubclass",
        "(Ljava/lang/Class;)Ljava/lang/Class;",
    ),
    (
        "java/lang/Class",
        "cast",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/Class",
        "getConstructors",
        "()[Ljava/lang/reflect/Constructor;",
    ),
    (
        "java/lang/Class",
        "getDeclaredConstructors",
        "()[Ljava/lang/reflect/Constructor;",
    ),
    (
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
    ),
    (
        "java/lang/Class",
        "getDeclaredMethods",
        "()[Ljava/lang/reflect/Method;",
    ),
    (
        "java/lang/Class",
        "getEnclosingClass",
        "()Ljava/lang/Class;",
    ),
    (
        "java/lang/Class",
        "getEnclosingConstructor",
        "()Ljava/lang/reflect/Constructor;",
    ),
    (
        "java/lang/Class",
        "getEnclosingMethod",
        "()Ljava/lang/reflect/Method;",
    ),
    (
        "java/lang/Class",
        "getEnumConstants",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/lang/Class",
        "getMethods",
        "()[Ljava/lang/reflect/Method;",
    ),
    ("java/lang/Class", "getSigners", "()[Ljava/lang/Object;"),
    (
        "java/lang/Class",
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
    ),
    ("java/lang/Class", "isAnnotation", "()Z"),
    ("java/lang/Class", "isEnum", "()Z"),
    (
        "java/lang/module/ModuleDescriptor$Version",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/module/ModuleDescriptor$Version",
        "hashCode",
        "()I",
    ),
    (
        "java/lang/module/ModuleDescriptor$Version",
        "parse",
        "(Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Version;",
    ),
    (
        "java/lang/module/ModuleDescriptor$Version",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// The 2026-09-09 Phase 3 wave: `ConcurrentHashMap` and `Properties`, as ONE
/// retirement, because neither class is retirable alone.
///
/// 185 triples over eight classes — the largest table here and the first whose
/// unit is a PAIR of classes. A fourth table rather than rows merged into
/// `RETIRED_SHADOW_PHASE2_TRIPLES` for the reason that one gives for existing:
/// these were adjudicated by a different method, and the method is the part
/// worth seeing at a glance. Phase 2's method — arm one class, read the probe
/// tree — cannot reach this verdict, and §"the dial is the wrong instrument
/// here" below is why.
///
/// # The reversal
///
/// `java/util/concurrent/ConcurrentHashMap` was adjudicated NOT retirable on
/// 2026-08-30 on `MapViewsShadowSweep` dying at row 261 of 302. `Properties`
/// was called LOAD-BEARING in the same sweep, with seven failing corpus
/// vectors. Armed as a pair on the pre-merge control binary
/// `a3855b8c7febaf7e`:
///
/// ```text
///                         CHM alone                 CHM + java/util/Properties
///   MapViewsShadowSweep   53 diffs, DIED 261/302    0 diffs, 302/302
///   ChmShadowSweep        0 over 28 671 yields      0 over 28 654 yields
/// ```
///
/// JDK 9 moved `Properties`' storage into a `ConcurrentHashMap` field named
/// `map`. So the two classes are one object graph, and arming either half is a
/// SPLIT STORE in one direction or the other: real `Properties` bytecode over a
/// native CHM, or real CHM bytecode under a `Properties` whose state is in a
/// Rust side table. Both halves real is the only configuration that is
/// consistent, and it is not a scope the 2026-08-30 sweep ever ran — it armed
/// 270 classes one at a time and then 236 at once, and a PAIR is neither.
///
/// # The precondition, and it is a code change
///
/// The object `System.getProperties()` returns is VM-built and its `map` was
/// **permanently null**, so the first real `Properties` body to run against it
/// threw. `native-builtins/src/properties_sidetable.rs`'s `replace_real_map`,
/// called from the `--jdk-only` arm of the `java/lang/System.getProperties`
/// registration, is what makes this wave possible at all. The whole defect and
/// its measurement are in
/// `docs/known-issues/jdk-only/the-system-properties-real-map-is-null-and-it-blocks-the-chm-retirement-20260909.md`.
///
/// # The dial is the wrong instrument here
///
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` declines at nine dispatch doors, and a
/// call that ORIGINATES IN A NATIVE is not one of them. `replace_real_map`
/// fills the map with `ctx.invoke_virtual(chm, "put", ..)`; armed, those `put`s
/// still reach the native and return `Ok`, so the map the real bytecode then
/// reads is EMPTY and nothing reports a failure. On one binary, one function:
///
/// ```text
///   scope = java/util/Properties           SysPropsRealMapProbe  10/10 vs HotSpot
///   scope = ..ConcurrentHashMap,Properties                        6 of 10 FALSE
/// ```
///
/// A registration REFUSED at `register` has no native to reach, so the same
/// `invoke_virtual` runs the real bytecode. That is a fourth difference from a
/// real retirement on top of the three that page's §6 lists, and it cuts both
/// ways: an armed run understates breakage for any class the VM calls into
/// from a native, and overstates it for a fix like this one. **So this wave was
/// measured on a trial binary carrying this table and could not have been
/// accepted on a dial arm.**
///
/// # Preconditions 3 and 4, and the 35 rows that are here for coherence
///
/// Every row owns its registry slot, has effective kind `Bridge`, and has image
/// `Code` to yield to. 150 of the 185 were dispatched (`invocations > 0`) by
/// the eight probes that back the measurement, read from those probes' own
/// `--dump-native-registry --explain-jdk-only` runs, which is what precondition
/// 4 asks for.
///
/// It was 122 of 185 until `apps/probes/ChmBulkSweep.java` was written for this
/// wave. The 63 undispatched rows were CHM's bulk/parallel surface —
/// `reduceKeysToLong`, `searchEntries`, `forEachEntry` and their siblings, plus
/// most of `EntrySetView`/`EntryIterator` — and no probe in the tree called any
/// of them, so retiring them would have been a change no instrument could see.
/// That probe is 60 rows chosen so the EMPTY answer and the right answer print
/// differently, and it is byte-identical to HotSpot on both sides of this
/// retirement. Seven of them are Java SERIALIZATION, because
/// `native_chm_write_object`'s own registration comment names that as the
/// hazard -- the real bodies walk the `table` field this VM's segmented layout
/// never populated -- and this wave retires both serialization hooks. Nothing
/// else in the probe tree round-trips a `ConcurrentHashMap`.
///
/// 35 rows still have no dispatch of their own and are retired on a structural
/// argument that has to be stated rather than assumed. `native_chm_put`
/// populates this VM's segmented store; the real `table` field it never touches
/// is what real CHM bytecode walks. Retire the writers, leave a native reader
/// standing, and that reader answers from a store nothing fills any more — an
/// EMPTY iteration, silently, which is strictly worse than an unmeasured
/// retirement. The 2026-08-30 bisect says the same from the other side: arming
/// `ConcurrentHashMap$` (the nested classes only) killed `ChmShadowSweep` at row
/// 13 of 208 where arming the whole class left it byte-identical, so **partial
/// is the configuration with evidence against it.** The instrument for the
/// remaining 35 is the full corpus in the landing protocol, not the probes.
///
/// # How the wave takes effect, and the way it could have been INERT
///
/// The re-tag makes `register_inner` refuse the registration under `--jdk-only`
/// without inserting it, so the force-native interception path
/// (`vm/src/runtime/interpreter/native_override.rs`, which lists
/// `ConcurrentHashMap`'s `put`/`get`/`size`/`computeIfAbsent`/... explicitly)
/// resolves no id and declines rather than raising §1.3. That is the good
/// outcome and it is not the only possible one: a refusal is a RETIREMENT only
/// when nothing already owns the triple, and
/// `JdkOnlyViolation::SyntheticNativeRegistered` carries a `survivor` for the
/// case where an earlier registration keeps serving — strict mode then runs
/// THAT native instead of the bytecode the policy asked for, and every probe
/// reads clean because nothing changed.
///
/// 53 of these triples are registered more than once (ordinals up to 3 in the
/// kind-map baseline), so the question is live. Measured on the trial binary,
/// `--jdk-only-report` carries **251 `synthetic-native-registered` refusals on
/// these two prefixes and ZERO of them has a survivor.** Check that column
/// before reading any probe row on a future wave: a green probe tree and an
/// inert retirement look identical from the outside.
///
/// One registration is EXCLUDED for want of a target:
/// `ConcurrentHashMap.reduceEntries(JLjava/util/function/BiFunction;)Ljava/lang/Object;`
/// reports `image_declaring_method.declared: false` — the real erasure returns
/// `Ljava/util/Map$Entry;`, so the descriptor matches no method in the image.
/// It can never be dispatched and retiring it would trade a dead shadow for an
/// `UnsatisfiedLinkError` if anything ever did reach it.
static RETIRED_SHADOW_PHASE3_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/Properties", "<init>", "()V"),
    (
        "java/util/Properties",
        "<init>",
        "(Ljava/util/Properties;)V",
    ),
    ("java/util/Properties", "clear", "()V"),
    ("java/util/Properties", "clone", "()Ljava/lang/Object;"),
    (
        "java/util/Properties",
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/Properties",
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
    ),
    (
        "java/util/Properties",
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    ("java/util/Properties", "contains", "(Ljava/lang/Object;)Z"),
    (
        "java/util/Properties",
        "containsKey",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Properties",
        "containsValue",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Properties",
        "elements",
        "()Ljava/util/Enumeration;",
    ),
    ("java/util/Properties", "entrySet", "()Ljava/util/Set;"),
    ("java/util/Properties", "equals", "(Ljava/lang/Object;)Z"),
    (
        "java/util/Properties",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
    ),
    (
        "java/util/Properties",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/Properties",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    ),
    ("java/util/Properties", "hashCode", "()I"),
    ("java/util/Properties", "isEmpty", "()Z"),
    ("java/util/Properties", "keySet", "()Ljava/util/Set;"),
    ("java/util/Properties", "keys", "()Ljava/util/Enumeration;"),
    ("java/util/Properties", "load", "(Ljava/io/InputStream;)V"),
    ("java/util/Properties", "load", "(Ljava/io/Reader;)V"),
    (
        "java/util/Properties",
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/Properties",
        "propertyNames",
        "()Ljava/util/Enumeration;",
    ),
    (
        "java/util/Properties",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/Properties", "putAll", "(Ljava/util/Map;)V"),
    (
        "java/util/Properties",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/Properties",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/Properties",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Properties",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/Properties",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Properties",
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
    ),
    (
        "java/util/Properties",
        "save",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
    ),
    (
        "java/util/Properties",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
    ),
    ("java/util/Properties", "size", "()I"),
    (
        "java/util/Properties",
        "store",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
    ),
    (
        "java/util/Properties",
        "store",
        "(Ljava/io/Writer;Ljava/lang/String;)V",
    ),
    (
        "java/util/Properties",
        "stringPropertyNames",
        "()Ljava/util/Set;",
    ),
    ("java/util/Properties", "toString", "()Ljava/lang/String;"),
    ("java/util/Properties", "values", "()Ljava/util/Collection;"),
    ("java/util/concurrent/ConcurrentHashMap", "<init>", "()V"),
    ("java/util/concurrent/ConcurrentHashMap", "<init>", "(I)V"),
    ("java/util/concurrent/ConcurrentHashMap", "<init>", "(IFI)V"),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "(Ljava/util/Map;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "addCount",
        "(JI)V",
    ),
    ("java/util/concurrent/ConcurrentHashMap", "clear", "()V"),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "containsKey",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "containsValue",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "elements",
        "()Ljava/util/Enumeration;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "entrySet",
        "()Ljava/util/Set;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "forEach",
        "(JLjava/util/function/BiConsumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "forEach",
        "(JLjava/util/function/BiFunction;Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "forEachEntry",
        "(JLjava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "forEachEntry",
        "(JLjava/util/function/Function;Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "forEachKey",
        "(JLjava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "forEachKey",
        "(JLjava/util/function/Function;Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "forEachValue",
        "(JLjava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "forEachValue",
        "(JLjava/util/function/Function;Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/concurrent/ConcurrentHashMap", "hashCode", "()I"),
    ("java/util/concurrent/ConcurrentHashMap", "isEmpty", "()Z"),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "keySet",
        "()Ljava/util/Set;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "keySet",
        "()Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "keySet",
        "(Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "keys",
        "()Ljava/util/Enumeration;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "mappingCount",
        "()J",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "newKeySet",
        "()Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "newKeySet",
        "(I)Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "putAll",
        "(Ljava/util/Map;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "readObject",
        "(Ljava/io/ObjectInputStream;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduce",
        "(JLjava/util/function/BiFunction;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceEntries",
        "(JLjava/util/function/Function;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceEntriesToDouble",
        "(JLjava/util/function/ToDoubleFunction;DLjava/util/function/DoubleBinaryOperator;)D",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceEntriesToInt",
        "(JLjava/util/function/ToIntFunction;ILjava/util/function/IntBinaryOperator;)I",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceEntriesToLong",
        "(JLjava/util/function/ToLongFunction;JLjava/util/function/LongBinaryOperator;)J",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceKeys",
        "(JLjava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceKeys",
        "(JLjava/util/function/Function;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceKeysToDouble",
        "(JLjava/util/function/ToDoubleFunction;DLjava/util/function/DoubleBinaryOperator;)D",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceKeysToInt",
        "(JLjava/util/function/ToIntFunction;ILjava/util/function/IntBinaryOperator;)I",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceKeysToLong",
        "(JLjava/util/function/ToLongFunction;JLjava/util/function/LongBinaryOperator;)J",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceToDouble",
        "(JLjava/util/function/ToDoubleBiFunction;DLjava/util/function/DoubleBinaryOperator;)D",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceToInt",
        "(JLjava/util/function/ToIntBiFunction;ILjava/util/function/IntBinaryOperator;)I",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceToLong",
        "(JLjava/util/function/ToLongBiFunction;JLjava/util/function/LongBinaryOperator;)J",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceValues",
        "(JLjava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceValues",
        "(JLjava/util/function/Function;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceValuesToDouble",
        "(JLjava/util/function/ToDoubleFunction;DLjava/util/function/DoubleBinaryOperator;)D",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceValuesToInt",
        "(JLjava/util/function/ToIntFunction;ILjava/util/function/IntBinaryOperator;)I",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "reduceValuesToLong",
        "(JLjava/util/function/ToLongFunction;JLjava/util/function/LongBinaryOperator;)J",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "search",
        "(JLjava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "searchEntries",
        "(JLjava/util/function/Function;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "searchKeys",
        "(JLjava/util/function/Function;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "searchValues",
        "(JLjava/util/function/Function;)Ljava/lang/Object;",
    ),
    ("java/util/concurrent/ConcurrentHashMap", "size", "()I"),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "values",
        "()Ljava/util/Collection;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap",
        "writeObject",
        "(Ljava/io/ObjectOutputStream;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntryIterator",
        "hasNext",
        "()Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntryIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntryIterator",
        "remove",
        "()V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "add",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "clear",
        "()V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "hashCode",
        "()I",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "isEmpty",
        "()Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "size",
        "()I",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeyIterator",
        "hasMoreElements",
        "()Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeyIterator",
        "hasNext",
        "()Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeyIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeyIterator",
        "nextElement",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeyIterator",
        "remove",
        "()V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "add",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "clear",
        "()V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "getMap",
        "()Ljava/util/concurrent/ConcurrentHashMap;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "getMappedValue",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "hashCode",
        "()I",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "isEmpty",
        "()Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "size",
        "()I",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$MapEntry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValueIterator",
        "hasMoreElements",
        "()Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValueIterator",
        "hasNext",
        "()Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValueIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValueIterator",
        "nextElement",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValueIterator",
        "remove",
        "()V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "clear",
        "()V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "isEmpty",
        "()Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "size",
        "()I",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// Lane 7's wave, 2026-09-10: the two triples that stop
/// `jdk/internal/loader/BuiltinClassLoader` from linking under `--jdk-only`.
///
/// # The measurement
///
/// `BuiltinClassLoader.<clinit>` is, in JDK 25 bytecode,
/// `if (!ClassLoader.registerAsParallelCapable()) throw new InternalError(...)`,
/// and it threw. `CRATONVM_DBG_CLINIT_FAIL=1` named the class and the exception
/// (the `NoClassDefFoundError` every consumer sees names only the consumer);
/// `apps/probes/L7ParallelCapableProbe.java` then named the broken link, with
/// no reflection and therefore no `--add-opens` requirement:
///
/// ```text
///                                            HotSpot  armed  unarmed
///   Direct       extends ClassLoader          true     true   true
///   UnderSecure  extends SecureClassLoader    true     FALSE  true
///   UnderUrl     extends URLClassLoader       true     FALSE  true
/// ```
///
/// `ParallelLoaders.register(c)` answers `loaderTypes.contains(c.getSuperclass())`,
/// so `BuiltinClassLoader` — whose superclass is `SecureClassLoader` — can only
/// register once `SecureClassLoader` has. It never did, and the two
/// registrations below are jointly why:
///
///  * `ClassLoader.registerAsParallelCapable()Z` was a native returning a
///    constant `Int(1)`. It never touched the real `ParallelLoaders.loaderTypes`
///    set, so every caller was told `true` while the SET stayed empty of
///    everything the JDK believed it had registered.
///  * `java/security/SecureClassLoader.<clinit>()V` was a **no-op native**
///    (S111r9, a `--real-jdk` Spring-Boot-launcher workaround). It runs during
///    the built-in loader allocation chain, i.e. from inside a native, where the
///    dispatch dial cannot see it — so no arm of
///    `CRATONVM_ENFORCE_NATIVE_SHADOW` could ever have yielded this row. Only a
///    registration-time refusal reaches it, which is what this table is.
///
/// That combination is the corrupt MIXTURE `scripts/jdk-only-blast-radius.sh`
/// caveat 4 describes rather than a partial retirement: half the chain kept the
/// native's fictional answer and half read the real set.
///
/// # Why retirement rather than a faithful native
///
/// The native's own comment said a faithful implementation was "not
/// implementable ... `NativeContext` exposes no caller-class / stack-walk
/// accessor". `NativeContext::frame_class_ids` has existed since the
/// `latestUserDefinedLoader` work, so that sentence is stale — but a native
/// mirroring `ParallelLoaders` would still have to keep a second copy of a JDK
/// set in step with the JDK's own, and §1.4's remedy for a shadow over concrete
/// bytecode is to yield to it. Both rows are bucket A (the image method
/// declares `Code`), so yielding has somewhere to go.
///
/// `Compatible` is untouched — a `SyntheticStub` registers and dispatches
/// normally there — which is what keeps the S111r9 workaround intact for the
/// `--real-jdk` fat-jar launchers it was written for.
static RETIRED_SHADOW_L7_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/ClassLoader", "registerAsParallelCapable", "()Z"),
    ("java/security/SecureClassLoader", "<clinit>", "()V"),
];

/// Lane 5 — `java/util/concurrent/`, `Thread`, `Unsafe`, retired 2026-09-10.
///
/// `docs/internal/retired/lane-5-concurrent-thread-unsafe-RETIRED-20260910.md`
/// is the page; this is the table it fills. The lane's population is
/// **405 bucket-A/B rows over 23 classes**, which is not the 516 a prefix
/// filter over `--dump-native-registry --explain-jdk-only` reports, and not
/// the 659 a binary that predates the Phase 3 wave reports. One subtraction
/// gets from 516 to 405, and it is a lane-0 rule rather than this lane's
/// choice:
///
///   * 111 rows come from a registrar whose classes span more than one lane
///     (`register_throwable_subclass_natives` and the `native-collections`
///     collection-family loops). Lane 0 §3: **the unit of work for a
///     cross-cutting registrar is the registrar, and lane T owns it whole** —
///     so `CopyOnWriteArraySet.equals`, registered by the same
///     `native-collections/src/lib.rs` line as `HashSet.equals` and
///     `LinkedHashSet.equals`, is not this lane's row to retire even though the
///     receiver is;
///
/// (The `ConcurrentHashMap` family's 99 rows are the difference between 659 and
/// 516: they were retired by the 2026-09-09 Phase 3 wave and are already in
/// [`RETIRED_SHADOW_PHASE3_TRIPLES`], so a dump from a binary that carries that
/// wave does not report them at all. **Take the dump from the binary you are
/// about to change.**)
///
/// # 98 of 405, and the other 307 are classified rather than deferred
///
/// | disposition | rows |
/// |---|---|
/// | retired here | **98** |
/// | held: the class's whole arm moves the VM AWAY from HotSpot | 127 |
/// | held: one unit with a held class | 133 |
/// | held: no instrument in this tree dispatches the row | 40 |
/// | dead registration — a door that never opens | 4 |
/// | held: a real-JDK keep arm this table would disarm | 2 |
/// | held: a partial with evidence against it | 1 |
///
/// # A retirement is mode-blind and a keep arm is not
///
/// This wave was 100 rows for most of a day. The two that came back out are
/// worth the paragraph, because the mechanism that removed them is general and
/// nothing in the four preconditions asks about it.
///
/// [`crate::registry::NativeMethodRegistry::register`] re-tags a retired triple
/// `Bridge` -> `SyntheticStub` **before** calling `register_inner`, and it does
/// so in every mode — the table is per-triple, not per-mode. Inside
/// `register_inner`, real-JDK mode (`drop_real_layout_synthetic`) keeps a small
/// number of natives it cannot safely execute as bytecode, and each of those
/// keeps is written as a predicate over `effective_category()`:
///
/// ```text
///   keep_real_scheduled_executor_bridge = effective_category() == Bridge && ...
///   keep_real_forkjoinpool_bridge       = effective_category() == Bridge && ...
///   keep_real_forkjointask_bridge       = effective_category() == Bridge && ...
/// ```
///
/// By the time those run, the re-tag has already made the answer
/// `SyntheticStub`. **A triple in this table can therefore lose its native in
/// REAL-JDK mode**, which is not what a §1.4 shadow retirement is for and is
/// not a mode this lane measured. `ScheduledThreadPoolExecutor.<init>(I,
/// ThreadFactory, RejectedExecutionHandler)` and `getCorePoolSize()I` are
/// exactly the two triples `keep_real_scheduled_executor_bridge` names — kept
/// for Spring's `ThreadPoolTaskScheduler` anonymous subclass — so they are out.
///
/// The instrument that caught it was
/// `registry::tests::real_layout_mode_drops_enumset_native_surface`, whose one
/// `Bridge`-survives control happened to BE the 3-arg constructor. That was
/// luck; `registry::tests::real_layout_bridge_keeps_are_not_retired_shadows`
/// now covers all three keep arms on purpose, by registering each protected
/// triple through the real code path rather than restating the predicate.
///
/// The other nine classes in this table are named nowhere in `registry.rs`
/// except a hash-test fixture (`jdk/internal/misc/Unsafe`) and one comment
/// (`java/util/concurrent/ThreadPoolExecutor`), so the sweep that found these
/// two found no others.
///
/// # The `Unsafe` subset that IS retired, and the line it is drawn on
///
/// Sixteen `jdk/internal/misc/Unsafe` rows are here and seventy-five are not,
/// and the line is not a judgement call: **an atomic or a fence that delegates
/// to an `ACC_NATIVE` primitive at the SAME offset** is retirable, because the
/// JDK's Java body is then a loop over calls this VM already serves correctly
/// — `getAndAddInt` is `do { v = getIntVolatile(o, offset); } while
/// (!weakCompareAndSetInt(o, offset, v, v + delta));` and every term in it is
/// one of ours. Anything that does ARITHMETIC on the offset is not, because
/// this VM's offsets are slot indices.
///
/// That is why `getAndSetReference` retires and `getAndSetByte` cannot, though
/// they are neighbours in the same file with the same shape.
///
/// The per-class record, with the measurement behind each blocker, is on the
/// lane page. Three of the blockers are properties of this VM rather than of
/// any one method, and are restated here because a future wave will re-derive
/// them otherwise:
///
///   * **`objectFieldOffset` returns a SLOT INDEX, not a byte offset.** The
///     JDK implements the whole sub-word atomic family in Java over a 4-byte
///     CAS — `long wordOffset = offset & ~3; int shift = (int)(offset & 3) << 3`
///     — so retiring `compareAndSetByte` and its relatives hands that
///     arithmetic a number it does not describe: `offset & ~3` names a
///     DIFFERENT FIELD. `unsafe_natives_ext.rs` says the same from the other
///     side and carries the HotSpot comparison that established it, and
///     `apps/probes/L5SubwordAtomics.java` scores the family at all four byte
///     positions of a word, on a field and on an array element, for
///     `byte`/`boolean`/`short`/`char`: **132 rows, 0 diffs, unarmed.** The
///     natives are right; it is the retirement that would be wrong.
///   * the `get*Unaligned` / `put*Unaligned` rows on `Unsafe` are the same
///     defect in a different family: they decompose a byte range, and a slot
///     index has no bytes. (The identically-named `ScopedMemoryAccess` rows
///     ARE retired — they take a `MemorySegment` base and a real byte offset,
///     which is a different number.)
///   * **`sun/misc/Unsafe`'s 82 rows are dispatched by nothing here.** All 121
///     probes report the dial VACUOUS on that scope, and the 132 `--jdk-only`
///     corpus reports reach 18 of the 82. Precondition 1 fails by measurement,
///     not by omission.
///
/// # The `*Internal` twin rule, which is why 16 `ScopedMemoryAccess` rows
///
/// Eight `ScopedMemoryAccess` rows were dispatched (`L4ByteBufferSweep` and
/// `L4TypedBufferSweep`, 518 dial yields between them, delta 0). Each is a
/// public wrapper whose only body calls its own `@ForceInline` `…Internal`
/// twin, and the twin is registered too — so retiring the wrapper alone would
/// produce a configuration NOBODY measured: real outer, native inner. The
/// class-wide arm that measured clean yielded both. So each retired wrapper
/// brings its twin, and the fourteen rows with no dispatch on either half stay
/// out.
///
/// # `AbstractExecutorService`'s four rows are a door that never opens
///
/// `submit` ×3 and `invokeAny` are registered on `AbstractExecutorService`,
/// which is abstract. A dispatch door asks the registry about the DECLARING
/// class of the resolved method, and every concrete executor in the image —
/// `ThreadPoolExecutor`, `ForkJoinPool` — carries its own registration of the
/// same names, so the abstract one is never the answer.
/// `apps/probes/L5ExecutorSweep.java` builds the one receiver shape that could
/// reach it (a direct subclass declaring only `execute`) and the rows still
/// read `invocations: 0`. Lane 0 §1: deleting these is worth doing and is
/// **not** a retirement, so they are not in this table.
static RETIRED_SHADOW_L5_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Thread$FieldHolder", "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;JIZ)V"),
    ("java/lang/Thread$State", "valueOf", "(Ljava/lang/String;)Ljava/lang/Thread$State;"),
    ("java/lang/Thread$State", "values", "()[Ljava/lang/Thread$State;"),
    ("java/util/concurrent/CompletableFuture", "allOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "anyOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "complete", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CompletableFuture", "completeExceptionally", "(Ljava/lang/Throwable;)Z"),
    ("java/util/concurrent/CompletableFuture", "completeValue", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CompletableFuture", "isCompletedExceptionally", "()Z"),
    ("java/util/concurrent/CompletableFuture", "thenAcceptAsync", "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "thenApplyAsync", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "thenCombine", "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "thenComposeAsync", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "thenRunAsync", "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "add", "(ILjava/lang/Object;)V"),
    ("java/util/concurrent/CopyOnWriteArrayList", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "addIfAbsent", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "bulkRemove", "(Ljava/util/function/Predicate;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "clear", "()V"),
    ("java/util/concurrent/CopyOnWriteArrayList", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "get", "(I)Ljava/lang/Object;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "indexOf", "(Ljava/lang/Object;)I"),
    ("java/util/concurrent/CopyOnWriteArrayList", "isEmpty", "()Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "remove", "(I)Ljava/lang/Object;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "remove", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "set", "(ILjava/lang/Object;)Ljava/lang/Object;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "size", "()I"),
    ("java/util/concurrent/CopyOnWriteArrayList", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/concurrent/PriorityBlockingQueue", "<init>", "()V"),
    ("java/util/concurrent/PriorityBlockingQueue", "isEmpty", "()Z"),
    ("java/util/concurrent/PriorityBlockingQueue", "offer", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/PriorityBlockingQueue", "peek", "()Ljava/lang/Object;"),
    ("java/util/concurrent/PriorityBlockingQueue", "poll", "()Ljava/lang/Object;"),
    ("java/util/concurrent/PriorityBlockingQueue", "poll", "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;"),
    ("java/util/concurrent/PriorityBlockingQueue", "put", "(Ljava/lang/Object;)V"),
    ("java/util/concurrent/PriorityBlockingQueue", "size", "()I"),
    ("java/util/concurrent/PriorityBlockingQueue", "take", "()Ljava/lang/Object;"),
    // `ScheduledThreadPoolExecutor.<init>(I,ThreadFactory,RejectedExecutionHandler)`
    // and `getCorePoolSize()I` WERE here and were REMOVED on 2026-09-10, before
    // this table ever landed. They are the two triples
    // `keep_real_scheduled_executor_bridge` (`registry.rs`) deliberately keeps
    // in REAL-JDK mode for Spring's `ThreadPoolTaskScheduler` anonymous
    // subclass — and that predicate reads `effective_category()`, which the
    // re-tag in `register` has already turned into `SyntheticStub` by the time
    // it runs. Retiring them therefore does not only retire a `--jdk-only`
    // shadow; it drops the native in real-JDK mode too, which is a mode this
    // lane never measured. See this module's header, "a retirement is
    // mode-blind and a keep arm is not".
    ("java/util/concurrent/ThreadPoolExecutor", "awaitTermination", "(JLjava/util/concurrent/TimeUnit;)Z"),
    ("java/util/concurrent/ThreadPoolExecutor", "getActiveCount", "()I"),
    ("java/util/concurrent/ThreadPoolExecutor", "getCompletedTaskCount", "()J"),
    ("java/util/concurrent/ThreadPoolExecutor", "getCorePoolSize", "()I"),
    ("java/util/concurrent/ThreadPoolExecutor", "getMaximumPoolSize", "()I"),
    ("java/util/concurrent/ThreadPoolExecutor", "getPoolSize", "()I"),
    ("java/util/concurrent/ThreadPoolExecutor", "getTaskCount", "()J"),
    ("java/util/concurrent/ThreadPoolExecutor", "invokeAny", "(Ljava/util/Collection;)Ljava/lang/Object;"),
    ("java/util/concurrent/ThreadPoolExecutor", "isShutdown", "()Z"),
    ("java/util/concurrent/ThreadPoolExecutor", "isTerminated", "()Z"),
    ("java/util/concurrent/ThreadPoolExecutor", "shutdown", "()V"),
    ("java/util/concurrent/ThreadPoolExecutor", "shutdownNow", "()Ljava/util/List;"),
    ("java/util/concurrent/ThreadPoolExecutor", "submit", "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;"),
    ("java/util/concurrent/ThreadPoolExecutor", "submit", "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;"),
    ("java/util/concurrent/ThreadPoolExecutor", "submit", "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;"),
    ("java/util/concurrent/TimeUnit", "convert", "(JLjava/util/concurrent/TimeUnit;)J"),
    ("java/util/concurrent/TimeUnit", "sleep", "(J)V"),
    ("java/util/concurrent/TimeUnit", "toDays", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toHours", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toMicros", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toMillis", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toMinutes", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toNanos", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toSeconds", "(J)J"),
    ("jdk/internal/misc/ScopedMemoryAccess", "copyMemory", "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "copyMemoryInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "getIntUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I"),
    ("jdk/internal/misc/ScopedMemoryAccess", "getIntUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I"),
    ("jdk/internal/misc/ScopedMemoryAccess", "getLongUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J"),
    ("jdk/internal/misc/ScopedMemoryAccess", "getLongUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J"),
    ("jdk/internal/misc/ScopedMemoryAccess", "getShortUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S"),
    ("jdk/internal/misc/ScopedMemoryAccess", "getShortUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putInt", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putIntInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putIntUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putIntUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putLongUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putLongUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putShortUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putShortUnalignedInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V"),
    ("jdk/internal/misc/Unsafe", "getAndAddInt", "(Ljava/lang/Object;JI)I"),
    ("jdk/internal/misc/Unsafe", "getAndAddLong", "(Ljava/lang/Object;JJ)J"),
    ("jdk/internal/misc/Unsafe", "getAndSetInt", "(Ljava/lang/Object;JI)I"),
    ("jdk/internal/misc/Unsafe", "getAndSetLong", "(Ljava/lang/Object;JJ)J"),
    ("jdk/internal/misc/Unsafe", "getAndSetReference", "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;"),
    ("jdk/internal/misc/Unsafe", "getReferenceAcquire", "(Ljava/lang/Object;J)Ljava/lang/Object;"),
    ("jdk/internal/misc/Unsafe", "loadFence", "()V"),
    ("jdk/internal/misc/Unsafe", "putIntOpaque", "(Ljava/lang/Object;JI)V"),
    ("jdk/internal/misc/Unsafe", "putReferenceOpaque", "(Ljava/lang/Object;JLjava/lang/Object;)V"),
    ("jdk/internal/misc/Unsafe", "putReferenceRelease", "(Ljava/lang/Object;JLjava/lang/Object;)V"),
    ("jdk/internal/misc/Unsafe", "storeFence", "()V"),
    ("jdk/internal/misc/Unsafe", "storeStoreFence", "()V"),
    ("jdk/internal/misc/Unsafe", "weakCompareAndSetInt", "(Ljava/lang/Object;JII)Z"),
    ("jdk/internal/misc/Unsafe", "weakCompareAndSetIntPlain", "(Ljava/lang/Object;JII)Z"),
    ("jdk/internal/misc/Unsafe", "weakCompareAndSetLong", "(Ljava/lang/Object;JJJ)Z"),
    ("jdk/internal/misc/Unsafe", "weakCompareAndSetReference", "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z"),
    ("jdk/internal/misc/VM", "getSavedProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("jdk/internal/misc/VM", "isBooted", "()Z"),
    ("jdk/internal/misc/VM", "maxDirectMemory", "()J"),
];

/// Lane 2 (`java/lang/` remainder, `java/math/`), 2026-09-10.
///
/// Lane 2's own population is **390 shadows over 57 classes**, once lane T's
/// boundary-crossing registrars are carved out of the 992 rows under its prefix
/// set. This table holds the rows that earned a retirement; every other row in
/// that population is dispositioned in
/// `docs/internal/jdk-only/lane-2-lang-values-RETIRED-20260911.md`.
///
/// # `java/lang/Character` — three deprecated statics, and nothing to argue
///
/// `isJavaLetter`, `isJavaLetterOrDigit` and `isSpace`. The first two are
/// one-line delegations to `isJavaIdentifierStart` / `isJavaIdentifierPart`,
/// which carry no native here and so already run real bytecode; `isSpace` is
/// pure arithmetic over a 64-bit constant and calls nothing at all:
///
/// ```text
///   public static boolean isSpace(char);
///        0: iload_0
///        1: bipush 32
///        3: if_icmpgt 22
///        6: ldc2_w  // long 4294981120l
///        ...
/// ```
///
/// Armed alone over the 40-vector `--jdk-only` corpus: **40 passed, 0 failed**,
/// the unarmed baseline exactly.
///
/// # `java/math/BigInteger` — 24 rows, and the blocker was a different native
///
/// The 2026-08-19 package screen recorded `java/math/` at 35/36 armed, and that
/// number is why the prefix was never admitted. Re-taken 2026-09-10 per class,
/// the cost is `BigInteger`'s and the vector is `RJdkSecurity`, asserting
/// `2^127-1 must be prime`.
///
/// It is not a `BigInteger` shadow at all. Real `BigInteger.shiftRight`
/// bytecode returned `(2^127-2) >> 1` short by exactly 2^32 — the top `mag[]`
/// limb left at zero — because JDK 25 does that shift in
/// `shiftRightImplWorker`, an `@IntrinsicCandidate` this VM registers a native
/// over in `native-builtins/src/biginteger_intrinsics.rs`. Both that worker and
/// its left-shift twin ran one iteration short of the JDK contract.
///
/// **The census could not see either of them**, because they are registered
/// `NativeKind::Intrinsic` and an `Intrinsic` is exempt from the shadow census
/// by construction. So the native blocking this lane's retirement was, by the
/// instrument's own design, invisible to the lane. Measured by calling the five
/// registered intrinsics reflectively (`apps/probes/L2IntrinsicProbe.java`):
/// **33 of 60 rows differed from HotSpot 25.0.4+7, all in the two shift
/// workers**, while `implSquareToLen`, `implMulAdd` and `mulAdd` were 0-diff in
/// the same run. Fixed in the same change as this table.
///
/// The 24 rows themselves were already probed: `apps/probes/BigIntegerSweep.java`
/// is **13253 rows, 0 diffs**, every ordered pair of a boundary corpus through
/// every binary operation.
///
/// ## Nine of the 24 are HELD BACK, and each half has its own cause
///
/// With all 24 retired, the probe-tree A/B on two binaries — control
/// `db988f6a76365f1f` without this table, trial `06307f0eca53c714` with it, same
/// tree otherwise — reads:
///
/// ```text
///   BigIntegerSweep   control (no table, JIT on)    0 differing lines
///                     trial   (table,    JIT on)   18   = 9 rows
///                     trial   (table,   --nojit)    6   = 3 rows
/// ```
///
/// A positive delta is the one result that is a reason not to retire, so the
/// nine are held per-TRIPLE — the same instrument the `java/util/logging` wave
/// used for `Logger.log`'s eighth overload — and each is named:
///
/// **`add`, `subtract`, `multiply` — a SURVIVOR.** All three are registered
/// twice. `phases_late.rs` owns the slot as a `Bridge`; `math_bignum.rs`
/// registered an `Intrinsic` first and owns no slot, so it is *dead in
/// compatible mode and never dispatched*. Refusing the `Bridge` under
/// `--jdk-only` does not reach bytecode — it wakes the dead loser, which
/// returns **null** for a null argument where the real body throws:
///
/// ```text
///   --jdk-only-report, BigIntegerSweep run:
///     27 synthetic-native-registered refusals on the retired classes
///      3 of them carrying a survivor
///        add       survivor=intrinsic@native-builtins/src/math_bignum.rs:1404
///        subtract  survivor=intrinsic@native-builtins/src/math_bignum.rs:1410
///        multiply  survivor=intrinsic@native-builtins/src/math_bignum.rs:1416
/// ```
///
/// This is the `refused is not retired` check earning its place: the probe rows
/// moved, so the wave *looked* measurable, and the retirement was inert.
/// Retiring these three means removing the dead `math_bignum.rs` registrations
/// first, which changes nothing in compatible mode because they own no slot.
///
/// **`remainder`, `mod`, `gcd`, `and`, `or`, `xor` — the JIT DROPPED the
/// message; blocker cleared 2026-09-11.** These do reach bytecode, and
/// interpreted they are HotSpot-exact. Once the real body was JIT-compiled the
/// `NullPointerException` arrived with no message at all. It was not a
/// `BigInteger` fact — `probes/L2JitNpeProbe.java` asks six null-deref shapes
/// cold and hot with no JDK class involved and every one of them lost its
/// message when hot. Fixed and retired as
/// `docs/internal/fixed-bugs/the-helpful-npe-message-is-lost-in-compiled-code-FIXED-20260911.md`,
/// pinned by `vm/tests/jit_npe_message_hot_equals_cold.rs`.
///
/// They are STILL HELD, and not by this reason: the rule below is structural —
/// every reference-argument row — and lifting it needs the `BigIntegerSweep`
/// measurement with the JIT on that put it there, which is lane 2's to take.
///
/// ## ...and the held set is every REFERENCE-argument row, not a list of six
///
/// The six above are what regressed on the 24-triple binary. On the 13-triple
/// one, `remainder` and friends were correct and `modInverse` and `modPow`
/// regressed instead — the same defect surfacing on different rows, because
/// which bodies the JIT has compiled by the time the probe's null section runs
/// is not fixed between runs. Reading one run's diff and holding exactly the
/// rows in it would be freezing a coin flip.
///
/// So the hold is structural rather than empirical: **a row is exposed if its
/// real body can dereference a null reference ARGUMENT**, and wave 1 retires
/// only signatures that take none. That is the twelve rows of `divide`,
/// `modInverse`, `modPow`, `add`, `subtract`, `multiply`, `remainder`, `mod`,
/// `gcd`, `and`, `or`, `xor` held for the JIT reason or the survivor reason,
/// plus the two `byte[]` constructors, which the sweep never asks with null and
/// so cannot vouch for either way.
///
/// What remains is ten value-shaped rows — `bitCount`, `bitLength`,
/// `intValueExact`, `isProbablePrime`, `longValueExact`, `not`, `shiftLeft`,
/// `shiftRight`, `testBit`, `toByteArray` — and they are 0-diff over the whole
/// 13253-row sweep with the JIT on.
///
/// # `java/lang/Package.getPackages()` left this table on 2026-09-11
///
/// Not un-retired: the four empty-array registrations behind it were DELETED,
/// so there is no longer a shadow for strict mode to refuse. A retirement row
/// for a triple nothing registers is rot that reads like a measurement. Record:
/// `docs/internal/jdk-only/package-getpackages-answered-empty-FIXED-20260911.md`.
static RETIRED_SHADOW_L2_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Character", "isJavaLetter", "(C)Z"),
    ("java/lang/Character", "isJavaLetterOrDigit", "(C)Z"),
    ("java/lang/Character", "isSpace", "(C)Z"),
    ("java/lang/ExceptionInInitializerError", "<init>", "()V"),
    (
        "java/lang/ExceptionInInitializerError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/IllegalThreadStateException", "<init>", "()V"),
    (
        "java/lang/IllegalThreadStateException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/NullPointerException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    ("java/lang/Object", "<init>", "()V"),
    ("java/lang/Object", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/Object", "finalize", "()V"),
    ("java/lang/Object", "toString", "()Ljava/lang/String;"),
    ("java/lang/Object", "wait", "()V"),
    ("java/lang/Object", "wait", "(J)V"),
    ("java/lang/Package", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/Package", "hashCode", "()I"),
    ("java/lang/StringUTF16", "getChars", "([BII[CI)V"),
    (
        "java/lang/Throwable",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/UnsatisfiedLinkError", "<init>", "()V"),
    (
        "java/lang/UnsatisfiedLinkError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    ("java/lang/VirtualMachineError", "<init>", "()V"),
    (
        "java/lang/VirtualMachineError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/VirtualMachineError",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/VirtualMachineError",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getClassLoadingMXBean",
        "()Ljava/lang/management/ClassLoadingMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getCompilationMXBean",
        "()Ljava/lang/management/CompilationMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getGarbageCollectorMXBeans",
        "()Ljava/util/List;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getMemoryMXBean",
        "()Ljava/lang/management/MemoryMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getOperatingSystemMXBean",
        "()Ljava/lang/management/OperatingSystemMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getPlatformMXBean",
        "(Ljava/lang/Class;)Ljava/lang/management/PlatformManagedObject;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getPlatformMXBeans",
        "(Ljava/lang/Class;)Ljava/util/List;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getRuntimeMXBean",
        "()Ljava/lang/management/RuntimeMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getThreadMXBean",
        "()Ljava/lang/management/ThreadMXBean;",
    ),
    ("java/lang/management/MemoryUsage", "<init>", "(JJJJ)V"),
    ("java/lang/management/MemoryUsage", "getCommitted", "()J"),
    ("java/lang/management/MemoryUsage", "getInit", "()J"),
    ("java/lang/management/MemoryUsage", "getMax", "()J"),
    ("java/lang/management/MemoryUsage", "getUsed", "()J"),
    ("java/math/BigInteger", "bitCount", "()I"),
    ("java/math/BigInteger", "bitLength", "()I"),
    ("java/math/BigInteger", "intValueExact", "()I"),
    ("java/math/BigInteger", "isProbablePrime", "(I)Z"),
    ("java/math/BigInteger", "longValueExact", "()J"),
    ("java/math/BigInteger", "not", "()Ljava/math/BigInteger;"),
    (
        "java/math/BigInteger",
        "shiftLeft",
        "(I)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "shiftRight",
        "(I)Ljava/math/BigInteger;",
    ),
    ("java/math/BigInteger", "testBit", "(I)Z"),
    ("java/math/BigInteger", "toByteArray", "()[B"),
];

/// Lane L1 wave 1, 2026-09-10: the six `java/util` families whose state is
/// already the REAL object — 265 triples over eighteen classes.
///
/// A fifth table, for the reason the third and fourth give for existing: a
/// different METHOD, and the method is what a later reader needs at a glance.
/// Phase 3 adjudicated a PAIR of classes on a trial binary. This wave
/// adjudicated **six armed prefixes, one at a time, against a fixed 44-probe
/// subset of the tree**, and then took the whole tree and the corpus on a
/// trial binary. The lane page is
/// `docs/known-issues/jdk-only-lanes/lane-1-util-text-time.md`.
///
/// # Why these six and not the other ten
///
/// `docs/.../lane-1-util-text-time.md` §3 states the rule this wave is an
/// application of: *if the VM keeps a collection's state in a Rust side
/// table, retiring the accessors hands reads to bytecode that looks at an
/// empty object.* So the question per family is not "is the probe green" but
/// "whose object is the authority", and the answer is in this crate's
/// siblings rather than in any measurement:
///
/// ```text
///   family        authority                                    verdict
///   ArrayList     real `elementData`/`size`                     retire
///   ArrayDeque    real `elements`/`head`/`tail` (slot 3 spare)   retire
///   LinkedList    `ll_set` writes `first`/`last`/`size` AND the
///                 real `LinkedList$Node` item/next/prev order    retire
///   Collections   stateless factories + the empty singletons     retire
///   Optional      real `value` (slot 0); the primitive trio use
///                 the real `isPresent`/`value` 2-field layout     retire
///   Arrays        stateless                                      retire
///   TreeMap/Set   `tm_array_table()` — a Rust side table         HELD
///   LinkedHashMap `lhm_overlay()` — a Rust side table            HELD
///   HashMap       real `table`, but 9 probes move                HELD
///   Hashtable     real `table`, but 4 probes move                HELD
///   Date/TimeZone 5 probes move                                  HELD
///   Locale        3 probes move, one truncates                   HELD
/// ```
///
/// The armed-one-prefix-at-a-time numbers, 44 probes each, base binary
/// `cratonvm-l1-base-20260910` (`origin/dev` at `7a8b79526`):
///
/// ```text
///   scope                       worse  better  probes that ASKED the dial
///   java/util/ArrayList,Arrays$ArrayList   0      1      44
///   java/util/ArrayDeque                   0      1      44
///   java/util/LinkedList                   0      1       9
///   java/util/Collections                  0      1      36
///   java/util/Optional                     0      0      15
///   java/util/Arrays                       0      0      44
///   java/util/TreeMap,TreeSet              9      1      19   <- held
///   java/util/LinkedHashMap               11      1      18   <- held
///   java/util/HashMap                      9      0      44   <- held
///   java/util/Hashtable                    4      0      44   <- held
///   java/util/Date,TimeZone,sun/util/cal   5      0      10   <- held
///   java/util/Locale,sun/util/locale/      3      0      12   <- held
/// ```
///
/// `NullArgMsgProbe` is the "better" row in four of the six, and it is the
/// reason to read the direction rather than the movement: armed, this VM
/// starts producing the JDK's own null-argument messages instead of its
/// invented ones. `java/util/Collections` alone takes it from 26 diffs to 14.
///
/// # Precondition 4, and the two probes written for it
///
/// 265 of the 272 rows in these six prefixes were dispatched
/// (`invocations > 0`) in a UNION over per-probe
/// `--dump-native-registry --explain-jdk-only` runs of the whole 118-probe
/// tree. It was 247 until `apps/probes/L1Wave1Sweep.java` was written for
/// this wave — 54 rows chosen so the EMPTY answer and the right answer print
/// differently, covering the `ArrayList$SubList` sequenced surface
/// (`addFirst`/`addLast`/`getFirst`/`getLast`/`removeFirst`/`removeLast`/
/// `reversed`), both `toArray(IntFunction)` overloads, `LinkedList.isEmpty`
/// and `stream`, and `Collections$EmptyListIterator.remove`. That probe found
/// a defect on the way in: `Collections.emptyList().listIterator().remove()`
/// threw `IllegalStateException: Collections.emptyIterator(): remove() before
/// next()` where HotSpot's is message-less — a row this wave retires, so
/// yielding is the fix.
///
/// # SEVEN rows were excluded, and SIX of them had to come back
///
/// They own their slot, they are `Bridge`, their image target carries `Code`
/// — and a probe written to call them could not make them fire, in either
/// mode:
///
/// ```text
///   java/util/Collections$SetFromMap  size/isEmpty/contains/iterator/toArray
///   java/util/Arrays$ArrayList        <init>([Ljava/lang/Object;)V
///   java/util/Collections             <clinit>()V
/// ```
///
/// **The first trial binary showed why that reading was wrong.** With wave 1
/// in the table and `Arrays$ArrayList.<init>` left out, `ArrayListShadowSweep`
/// row 125 — `Arrays.asList((Object[]) null)` — regressed from
/// `THREW java.lang.NullPointerException` to `no-throw`. Real
/// `Arrays.asList` is `return new ArrayList<>(a)`, real
/// `Arrays$ArrayList.<init>` is `a = Objects.requireNonNull(array)`, and the
/// constructor's counter was 0 **because the native `asList` never reached
/// it**. Retiring the producer is what makes the consumer reachable.
///
/// So: precondition 4 is measured on the UNRETIRED binary, and a zero there
/// means "nothing reaches this today", not "nothing can". When the row that
/// was serving the producer is in the same wave, the consumer has to move
/// with it. Six rows joined on that argument —
/// `Arrays$ArrayList.<init>` behind `Arrays.asList`, and the five
/// `Collections$SetFromMap` accessors behind `Collections.newSetFromMap`.
///
/// `java/util/Collections.<clinit>()V` is the one that stays out. It has no
/// producer to retire: a `<clinit>` is reached by class initialisation,
/// which this table cannot change. Recorded rather than merely absent — the
/// same disposition Phase 3 gave `reduceEntries`.
///
/// # Four entries came OFF the held list, and here is what moved them
///
/// `the_held_collection_families_are_not_retired` named
/// `ArrayDeque.addLast`, `LinkedList.add(Object)`, `ArrayList$Itr.next` and
/// `Arrays.copyOf([Object;I)` as "state is not real" / "load-bearing FOR the
/// retirements above". Three of the four are the same correction: the state
/// became real after that list was written. `ll_set` publishes `first`,
/// `last` and `size` to the receiver's own fields and `ll_alloc_node` uses
/// the real `LinkedList$Node` slot order; `ad_ensure_capacity` keeps the
/// JDK's one-spare-slot emptiness invariant on the real
/// `elements`/`head`/`tail`; `al_itr_slots` keeps `cursor`/`lastRet`/
/// `expectedModCount` where the real `ArrayList$Itr` declares them.
/// `Arrays.copyOf` is the fourth and is different: it was held because the
/// ArrayList five depended on it, and this wave retires the dependents and
/// the dependency together, which is the only configuration that is not a
/// split store.
///
/// # Wave 2, same day: the five families the probe TREE could not ask about
///
/// Wave 1's sweep read `0 worse` for `java/util/jar/`, `java/util/zip/`,
/// `java/text/`, `java/time/`, `java/util/Stack`, `java/util/Vector`,
/// `java/util/PriorityQueue` and `java/util/ResourceBundle` — and 44 of 44,
/// 43 of 44, 44 of 44 and 43 of 44 of those probe runs were **VACUOUS**:
/// `enforcement_dial.reached` was 0, so every zero was the zero of a question
/// nobody posed. `apps/probes/L1TailSweep.java` is the question — 137 rows
/// across all eight — and it changed four of the eight verdicts:
///
/// ```text
///   scope                      worse  better  L1TailSweep delta   verdict
///   java/util/zip/                 0       1   -1  (and it un-dies)  retire
///   java/util/Stack,Vector         0       1    0                    retire
///   java/util/PriorityQueue        0       0    0  (639 yields)      retire
///   java/time/                     0       0    0  (16/17)           retire
///   java/util/ResourceBundle       0       1    0  (60/759)          HELD *
///   java/util/jar/                 1       0  +28                    HELD
///   java/text/                     1       0  +11                    HELD
/// ```
///
/// **\* `java/util/ResourceBundle` is the row where the DIAL and the TRIAL
/// BINARY disagreed, and the trial binary is right.** Armed, it was clean over
/// 51 probes. Retired, it moved five probes by ten rows, all of them the same
/// value:
///
/// ```text
///   TimeZone.getDisplayName()          HotSpot "Eastern Standard Time"
///   control                                    "Eastern Standard Time"
///   trial (ResourceBundle retired)             "Coordinated Universal Time"
///   LocaleDateTzShadowSweep 98/99      "EST"/"EDT"  ->  "UTC"/"UTC"
/// ```
///
/// This is Phase 3's §"the dial is the wrong instrument here" from the other
/// side. `CRATONVM_ENFORCE_NATIVE_SHADOW` declines at nine DISPATCH doors, and
/// a call that ORIGINATES INSIDE A NATIVE is not one of them:
/// `TimeZone.getDisplayName` is served by a native that reaches
/// `ResourceBundle` through `ctx.invoke_*`, so arming the dial left that path
/// on the native and reported clean. A registration REFUSED at `register` has
/// no native to reach, and the real `TimeZoneNameUtility` lookup then finds no
/// names and falls back to the UTC display name. **So an armed-clean prefix is
/// a candidate and never a verdict — including when the prefix is clean over
/// fifty probes.** `java/util/ResourceBundle` and `$Control` (12 rows) are
/// HELD, and the blocker is named: the locale-provider lookup the real
/// `getBundle` performs does not find the JDK's own timezone name bundles on
/// this VM.
///
/// **`java/util/zip/` is a FIX, not a neutral retirement.** On the control
/// binary, under `--jdk-only`, `new ZipFile(<a path that does not exist>)`
/// does not throw `NoSuchFileException`; it raises
/// `internal error: JarFile: cannot open ...`, which is
/// `MethodCallFailed::InternalError` — uncatchable, so `catch (Throwable)`
/// never runs and the VM exits 1 mid-probe. Armed, the probe survives it.
/// Two more rows in the same family are §1.4 defects that yielding repairs:
/// `CRC32C.update(byte[], off, len)` accepts `len` past the end of the array
/// where HotSpot throws `ArrayIndexOutOfBoundsException`, and
/// `Collections.emptyList().listIterator().remove()` (wave 1) invents a
/// message HotSpot does not have.
///
/// **`java/util/jar/` and `java/text/` are HELD and they are held for a
/// reason the tree could not have told anyone**: both were `0 worse` until a
/// probe reached them. `jar` armed takes `L1TailSweep` from 11 diffs to 39
/// and truncates it; `text` armed takes it to 22. A vacuous green is not a
/// green, and these two are the worked example.
///
/// One wave-2 row is excluded for want of precondition 4, on the same terms
/// as wave 1's: `java/util/zip/ZipFile$1.getManifestName`, a shared-secret
/// accessor shim no bytecode can name. The seven `ResourceBundle` rows that
/// would have joined it are moot — the whole family is HELD.
static RETIRED_SHADOW_L1_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/time/Duration", "parse", "(Ljava/lang/CharSequence;)Ljava/time/Duration;"),
    ("java/time/ZoneId", "systemDefault", "()Ljava/time/ZoneId;"),
    ("java/util/ArrayDeque", "<init>", "()V"),
    ("java/util/ArrayDeque", "<init>", "(I)V"),
    ("java/util/ArrayDeque", "<init>", "(Ljava/util/Collection;)V"),
    ("java/util/ArrayDeque", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayDeque", "addFirst", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayDeque", "addLast", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayDeque", "clear", "()V"),
    ("java/util/ArrayDeque", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "element", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "forEach", "(Ljava/util/function/Consumer;)V"),
    ("java/util/ArrayDeque", "getFirst", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "getLast", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "isEmpty", "()Z"),
    ("java/util/ArrayDeque", "offer", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "offerFirst", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "offerLast", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "peek", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "peekFirst", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "peekLast", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "poll", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "pollFirst", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "pollLast", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "pop", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "push", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayDeque", "remove", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "remove", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "removeAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayDeque", "removeFirst", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "removeFirstOccurrence", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "removeIf", "(Ljava/util/function/Predicate;)Z"),
    ("java/util/ArrayDeque", "removeLast", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "removeLastOccurrence", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "retainAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayDeque", "size", "()I"),
    ("java/util/ArrayDeque", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "toString", "()Ljava/lang/String;"),
    ("java/util/ArrayList", "add", "(ILjava/lang/Object;)V"),
    ("java/util/ArrayList", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayList", "ensureCapacity", "(I)V"),
    ("java/util/ArrayList", "equals", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayList", "forEach", "(Ljava/util/function/Consumer;)V"),
    ("java/util/ArrayList", "hashCode", "()I"),
    ("java/util/ArrayList", "indexOf", "(Ljava/lang/Object;)I"),
    ("java/util/ArrayList", "lastIndexOf", "(Ljava/lang/Object;)I"),
    ("java/util/ArrayList", "listIterator", "()Ljava/util/ListIterator;"),
    ("java/util/ArrayList", "listIterator", "(I)Ljava/util/ListIterator;"),
    ("java/util/ArrayList", "remove", "(I)Ljava/lang/Object;"),
    ("java/util/ArrayList", "remove", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayList", "removeAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayList", "removeIf", "(Ljava/util/function/Predicate;)Z"),
    ("java/util/ArrayList", "replaceAll", "(Ljava/util/function/UnaryOperator;)V"),
    ("java/util/ArrayList", "retainAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayList", "set", "(ILjava/lang/Object;)Ljava/lang/Object;"),
    ("java/util/ArrayList", "sort", "(Ljava/util/Comparator;)V"),
    ("java/util/ArrayList", "stream", "()Ljava/util/stream/Stream;"),
    ("java/util/ArrayList", "toArray", "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;"),
    ("java/util/ArrayList", "toString", "()Ljava/lang/String;"),
    ("java/util/ArrayList", "trimToSize", "()V"),
    ("java/util/ArrayList$Itr", "remove", "()V"),
    ("java/util/ArrayList$ListItr", "add", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayList$ListItr", "hasNext", "()Z"),
    ("java/util/ArrayList$ListItr", "hasPrevious", "()Z"),
    ("java/util/ArrayList$ListItr", "next", "()Ljava/lang/Object;"),
    ("java/util/ArrayList$ListItr", "nextIndex", "()I"),
    ("java/util/ArrayList$ListItr", "previous", "()Ljava/lang/Object;"),
    ("java/util/ArrayList$ListItr", "previousIndex", "()I"),
    ("java/util/ArrayList$ListItr", "remove", "()V"),
    ("java/util/ArrayList$ListItr", "set", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayList$SubList", "add", "(ILjava/lang/Object;)V"),
    ("java/util/ArrayList$SubList", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayList$SubList", "addAll", "(ILjava/util/Collection;)Z"),
    ("java/util/ArrayList$SubList", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayList$SubList", "addFirst", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayList$SubList", "addLast", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayList$SubList", "clear", "()V"),
    ("java/util/ArrayList$SubList", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayList$SubList", "containsAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayList$SubList", "equals", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayList$SubList", "forEach", "(Ljava/util/function/Consumer;)V"),
    ("java/util/ArrayList$SubList", "get", "(I)Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "getFirst", "()Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "getLast", "()Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "hashCode", "()I"),
    ("java/util/ArrayList$SubList", "indexOf", "(Ljava/lang/Object;)I"),
    ("java/util/ArrayList$SubList", "isEmpty", "()Z"),
    ("java/util/ArrayList$SubList", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/ArrayList$SubList", "lastIndexOf", "(Ljava/lang/Object;)I"),
    ("java/util/ArrayList$SubList", "listIterator", "()Ljava/util/ListIterator;"),
    ("java/util/ArrayList$SubList", "listIterator", "(I)Ljava/util/ListIterator;"),
    ("java/util/ArrayList$SubList", "parallelStream", "()Ljava/util/stream/Stream;"),
    ("java/util/ArrayList$SubList", "remove", "(I)Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "remove", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayList$SubList", "removeAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayList$SubList", "removeFirst", "()Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "removeIf", "(Ljava/util/function/Predicate;)Z"),
    ("java/util/ArrayList$SubList", "removeLast", "()Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "replaceAll", "(Ljava/util/function/UnaryOperator;)V"),
    ("java/util/ArrayList$SubList", "retainAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayList$SubList", "reversed", "()Ljava/util/List;"),
    ("java/util/ArrayList$SubList", "set", "(ILjava/lang/Object;)Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "size", "()I"),
    ("java/util/ArrayList$SubList", "sort", "(Ljava/util/Comparator;)V"),
    ("java/util/ArrayList$SubList", "spliterator", "()Ljava/util/Spliterator;"),
    ("java/util/ArrayList$SubList", "stream", "()Ljava/util/stream/Stream;"),
    ("java/util/ArrayList$SubList", "subList", "(II)Ljava/util/List;"),
    ("java/util/ArrayList$SubList", "toArray", "()[Ljava/lang/Object;"),
    (
        "java/util/ArrayList$SubList",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    ("java/util/ArrayList$SubList", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "toString", "()Ljava/lang/String;"),
    ("java/util/ArrayList$SubList$1", "add", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayList$SubList$1", "forEachRemaining", "(Ljava/util/function/Consumer;)V"),
    ("java/util/ArrayList$SubList$1", "hasNext", "()Z"),
    ("java/util/ArrayList$SubList$1", "hasPrevious", "()Z"),
    ("java/util/ArrayList$SubList$1", "next", "()Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList$1", "nextIndex", "()I"),
    ("java/util/ArrayList$SubList$1", "previous", "()Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList$1", "previousIndex", "()I"),
    ("java/util/ArrayList$SubList$1", "remove", "()V"),
    ("java/util/ArrayList$SubList$1", "set", "(Ljava/lang/Object;)V"),
    ("java/util/Arrays", "asList", "([Ljava/lang/Object;)Ljava/util/List;"),
    ("java/util/Arrays", "binarySearch", "([II)I"),
    ("java/util/Arrays", "copyOf", "([BI)[B"),
    ("java/util/Arrays", "copyOf", "([II)[I"),
    ("java/util/Arrays", "copyOf", "([Ljava/lang/Object;I)[Ljava/lang/Object;"),
    ("java/util/Arrays", "copyOf", "([Ljava/lang/Object;ILjava/lang/Class;)[Ljava/lang/Object;"),
    ("java/util/Arrays", "copyOfRange", "([BII)[B"),
    ("java/util/Arrays", "copyOfRange", "([Ljava/lang/Object;II)[Ljava/lang/Object;"),
    ("java/util/Arrays", "equals", "([B[B)Z"),
    ("java/util/Arrays", "equals", "([I[I)Z"),
    ("java/util/Arrays", "fill", "([II)V"),
    ("java/util/Arrays", "fill", "([Ljava/lang/Object;Ljava/lang/Object;)V"),
    ("java/util/Arrays", "hashCode", "([B)I"),
    ("java/util/Arrays", "hashCode", "([Ljava/lang/Object;)I"),
    ("java/util/Arrays", "sort", "([I)V"),
    ("java/util/Arrays", "sort", "([JII)V"),
    ("java/util/Arrays", "sort", "([Ljava/lang/Object;)V"),
    ("java/util/Arrays", "stream", "([Ljava/lang/Object;)Ljava/util/stream/Stream;"),
    ("java/util/Arrays", "toString", "([Ljava/lang/Object;)Ljava/lang/String;"),
    ("java/util/Arrays$ArrayList", "<init>", "([Ljava/lang/Object;)V"),
    ("java/util/Arrays$ArrayList", "get", "(I)Ljava/lang/Object;"),
    ("java/util/Arrays$ArrayList", "isEmpty", "()Z"),
    ("java/util/Arrays$ArrayList", "size", "()I"),
    ("java/util/Arrays$ArrayList", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/Collections", "addAll", "(Ljava/util/Collection;[Ljava/lang/Object;)Z"),
    ("java/util/Collections", "emptyEnumeration", "()Ljava/util/Enumeration;"),
    ("java/util/Collections", "emptyIterator", "()Ljava/util/Iterator;"),
    ("java/util/Collections", "emptyList", "()Ljava/util/List;"),
    ("java/util/Collections", "emptyListIterator", "()Ljava/util/ListIterator;"),
    ("java/util/Collections", "emptyMap", "()Ljava/util/Map;"),
    ("java/util/Collections", "emptySet", "()Ljava/util/Set;"),
    ("java/util/Collections", "fill", "(Ljava/util/List;Ljava/lang/Object;)V"),
    ("java/util/Collections", "frequency", "(Ljava/util/Collection;Ljava/lang/Object;)I"),
    ("java/util/Collections", "max", "(Ljava/util/Collection;)Ljava/lang/Object;"),
    ("java/util/Collections", "min", "(Ljava/util/Collection;)Ljava/lang/Object;"),
    ("java/util/Collections", "nCopies", "(ILjava/lang/Object;)Ljava/util/List;"),
    ("java/util/Collections", "newSetFromMap", "(Ljava/util/Map;)Ljava/util/Set;"),
    ("java/util/Collections", "reverse", "(Ljava/util/List;)V"),
    ("java/util/Collections", "shuffle", "(Ljava/util/List;)V"),
    ("java/util/Collections", "singleton", "(Ljava/lang/Object;)Ljava/util/Set;"),
    ("java/util/Collections", "singletonList", "(Ljava/lang/Object;)Ljava/util/List;"),
    (
        "java/util/Collections",
        "singletonMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
    ),
    ("java/util/Collections", "sort", "(Ljava/util/List;)V"),
    ("java/util/Collections", "sort", "(Ljava/util/List;Ljava/util/Comparator;)V"),
    ("java/util/Collections", "swap", "(Ljava/util/List;II)V"),
    (
        "java/util/Collections",
        "synchronizedCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
    ),
    ("java/util/Collections", "synchronizedList", "(Ljava/util/List;)Ljava/util/List;"),
    ("java/util/Collections", "synchronizedSet", "(Ljava/util/Set;)Ljava/util/Set;"),
    ("java/util/Collections$EmptyEnumeration", "hasMoreElements", "()Z"),
    ("java/util/Collections$EmptyEnumeration", "nextElement", "()Ljava/lang/Object;"),
    ("java/util/Collections$EmptyIterator", "hasNext", "()Z"),
    ("java/util/Collections$EmptyIterator", "next", "()Ljava/lang/Object;"),
    ("java/util/Collections$EmptyIterator", "remove", "()V"),
    ("java/util/Collections$EmptyListIterator", "add", "(Ljava/lang/Object;)V"),
    ("java/util/Collections$EmptyListIterator", "hasNext", "()Z"),
    ("java/util/Collections$EmptyListIterator", "hasPrevious", "()Z"),
    ("java/util/Collections$EmptyListIterator", "next", "()Ljava/lang/Object;"),
    ("java/util/Collections$EmptyListIterator", "nextIndex", "()I"),
    ("java/util/Collections$EmptyListIterator", "previous", "()Ljava/lang/Object;"),
    ("java/util/Collections$EmptyListIterator", "previousIndex", "()I"),
    ("java/util/Collections$EmptyListIterator", "remove", "()V"),
    ("java/util/Collections$EmptyListIterator", "set", "(Ljava/lang/Object;)V"),
    ("java/util/Collections$SetFromMap", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/Collections$SetFromMap", "isEmpty", "()Z"),
    ("java/util/Collections$SetFromMap", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/Collections$SetFromMap", "size", "()I"),
    ("java/util/Collections$SetFromMap", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/LinkedList", "<init>", "()V"),
    ("java/util/LinkedList", "<init>", "(Ljava/util/Collection;)V"),
    ("java/util/LinkedList", "add", "(ILjava/lang/Object;)V"),
    ("java/util/LinkedList", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedList", "addAll", "(ILjava/util/Collection;)Z"),
    ("java/util/LinkedList", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/LinkedList", "addFirst", "(Ljava/lang/Object;)V"),
    ("java/util/LinkedList", "addLast", "(Ljava/lang/Object;)V"),
    ("java/util/LinkedList", "clear", "()V"),
    ("java/util/LinkedList", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedList", "descendingIterator", "()Ljava/util/Iterator;"),
    ("java/util/LinkedList", "get", "(I)Ljava/lang/Object;"),
    ("java/util/LinkedList", "getFirst", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "getLast", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "isEmpty", "()Z"),
    ("java/util/LinkedList", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/LinkedList", "listIterator", "()Ljava/util/ListIterator;"),
    ("java/util/LinkedList", "listIterator", "(I)Ljava/util/ListIterator;"),
    ("java/util/LinkedList", "offer", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedList", "peek", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "poll", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "pollFirst", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "pollLast", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "remove", "(I)Ljava/lang/Object;"),
    ("java/util/LinkedList", "remove", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedList", "removeFirst", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "removeFirstOccurrence", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedList", "removeIf", "(Ljava/util/function/Predicate;)Z"),
    ("java/util/LinkedList", "removeLast", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "removeLastOccurrence", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedList", "set", "(ILjava/lang/Object;)Ljava/lang/Object;"),
    ("java/util/LinkedList", "size", "()I"),
    ("java/util/LinkedList", "spliterator", "()Ljava/util/Spliterator;"),
    ("java/util/LinkedList", "stream", "()Ljava/util/stream/Stream;"),
    ("java/util/LinkedList", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/LinkedList", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/LinkedList", "toString", "()Ljava/lang/String;"),
    ("java/util/LinkedList$ListItr", "add", "(Ljava/lang/Object;)V"),
    ("java/util/LinkedList$ListItr", "hasNext", "()Z"),
    ("java/util/LinkedList$ListItr", "hasPrevious", "()Z"),
    ("java/util/LinkedList$ListItr", "next", "()Ljava/lang/Object;"),
    ("java/util/LinkedList$ListItr", "nextIndex", "()I"),
    ("java/util/LinkedList$ListItr", "previous", "()Ljava/lang/Object;"),
    ("java/util/LinkedList$ListItr", "previousIndex", "()I"),
    ("java/util/LinkedList$ListItr", "remove", "()V"),
    ("java/util/LinkedList$ListItr", "set", "(Ljava/lang/Object;)V"),
    ("java/util/Optional", "empty", "()Ljava/util/Optional;"),
    ("java/util/Optional", "equals", "(Ljava/lang/Object;)Z"),
    ("java/util/Optional", "filter", "(Ljava/util/function/Predicate;)Ljava/util/Optional;"),
    ("java/util/Optional", "flatMap", "(Ljava/util/function/Function;)Ljava/util/Optional;"),
    ("java/util/Optional", "get", "()Ljava/lang/Object;"),
    ("java/util/Optional", "hashCode", "()I"),
    ("java/util/Optional", "ifPresent", "(Ljava/util/function/Consumer;)V"),
    (
        "java/util/Optional",
        "ifPresentOrElse",
        "(Ljava/util/function/Consumer;Ljava/lang/Runnable;)V",
    ),
    ("java/util/Optional", "isEmpty", "()Z"),
    ("java/util/Optional", "isPresent", "()Z"),
    ("java/util/Optional", "map", "(Ljava/util/function/Function;)Ljava/util/Optional;"),
    ("java/util/Optional", "of", "(Ljava/lang/Object;)Ljava/util/Optional;"),
    ("java/util/Optional", "ofNullable", "(Ljava/lang/Object;)Ljava/util/Optional;"),
    ("java/util/Optional", "or", "(Ljava/util/function/Supplier;)Ljava/util/Optional;"),
    ("java/util/Optional", "orElse", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/util/Optional", "orElseGet", "(Ljava/util/function/Supplier;)Ljava/lang/Object;"),
    ("java/util/Optional", "orElseThrow", "()Ljava/lang/Object;"),
    ("java/util/Optional", "orElseThrow", "(Ljava/util/function/Supplier;)Ljava/lang/Object;"),
    ("java/util/Optional", "stream", "()Ljava/util/stream/Stream;"),
    ("java/util/Optional", "toString", "()Ljava/lang/String;"),
    ("java/util/OptionalDouble", "empty", "()Ljava/util/OptionalDouble;"),
    ("java/util/OptionalDouble", "getAsDouble", "()D"),
    ("java/util/OptionalDouble", "ifPresent", "(Ljava/util/function/DoubleConsumer;)V"),
    ("java/util/OptionalDouble", "isPresent", "()Z"),
    ("java/util/OptionalDouble", "of", "(D)Ljava/util/OptionalDouble;"),
    ("java/util/OptionalDouble", "orElse", "(D)D"),
    ("java/util/OptionalInt", "empty", "()Ljava/util/OptionalInt;"),
    ("java/util/OptionalInt", "getAsInt", "()I"),
    ("java/util/OptionalInt", "ifPresent", "(Ljava/util/function/IntConsumer;)V"),
    ("java/util/OptionalInt", "isPresent", "()Z"),
    ("java/util/OptionalInt", "of", "(I)Ljava/util/OptionalInt;"),
    ("java/util/OptionalInt", "orElse", "(I)I"),
    ("java/util/OptionalLong", "empty", "()Ljava/util/OptionalLong;"),
    ("java/util/OptionalLong", "getAsLong", "()J"),
    ("java/util/OptionalLong", "ifPresent", "(Ljava/util/function/LongConsumer;)V"),
    ("java/util/OptionalLong", "isPresent", "()Z"),
    ("java/util/OptionalLong", "of", "(J)Ljava/util/OptionalLong;"),
    ("java/util/OptionalLong", "orElse", "(J)J"),
    ("java/util/PriorityQueue", "<init>", "()V"),
    ("java/util/PriorityQueue", "<init>", "(I)V"),
    ("java/util/PriorityQueue", "<init>", "(Ljava/util/Collection;)V"),
    ("java/util/PriorityQueue", "<init>", "(Ljava/util/Comparator;)V"),
    ("java/util/PriorityQueue", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/PriorityQueue", "clear", "()V"),
    ("java/util/PriorityQueue", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/PriorityQueue", "isEmpty", "()Z"),
    ("java/util/PriorityQueue", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/PriorityQueue", "offer", "(Ljava/lang/Object;)Z"),
    ("java/util/PriorityQueue", "peek", "()Ljava/lang/Object;"),
    ("java/util/PriorityQueue", "poll", "()Ljava/lang/Object;"),
    ("java/util/PriorityQueue", "remove", "(Ljava/lang/Object;)Z"),
    ("java/util/PriorityQueue", "size", "()I"),
    ("java/util/PriorityQueue", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/PriorityQueue", "toString", "()Ljava/lang/String;"),
    ("java/util/PriorityQueue$Itr", "hasNext", "()Z"),
    ("java/util/PriorityQueue$Itr", "next", "()Ljava/lang/Object;"),
    ("java/util/PriorityQueue$Itr", "remove", "()V"),
    ("java/util/Stack", "<init>", "()V"),
    ("java/util/Stack", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/Stack", "clear", "()V"),
    ("java/util/Stack", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/Stack", "empty", "()Z"),
    ("java/util/Stack", "get", "(I)Ljava/lang/Object;"),
    ("java/util/Stack", "indexOf", "(Ljava/lang/Object;)I"),
    ("java/util/Stack", "isEmpty", "()Z"),
    ("java/util/Stack", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/Stack", "peek", "()Ljava/lang/Object;"),
    ("java/util/Stack", "pop", "()Ljava/lang/Object;"),
    ("java/util/Stack", "push", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/util/Stack", "remove", "(I)Ljava/lang/Object;"),
    ("java/util/Stack", "search", "(Ljava/lang/Object;)I"),
    ("java/util/Stack", "set", "(ILjava/lang/Object;)Ljava/lang/Object;"),
    ("java/util/Stack", "size", "()I"),
    ("java/util/Stack", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/Stack", "toString", "()Ljava/lang/String;"),
    ("java/util/Vector", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/zip/CRC32", "updateBytes", "(I[BII)I"),
    ("java/util/zip/CRC32C", "<init>", "()V"),
    ("java/util/zip/CRC32C", "getValue", "()J"),
    ("java/util/zip/CRC32C", "reset", "()V"),
    ("java/util/zip/CRC32C", "update", "(I)V"),
    ("java/util/zip/CRC32C", "update", "([B)V"),
    ("java/util/zip/CRC32C", "update", "([BII)V"),
    ("java/util/zip/ZipEntry", "setComment", "(Ljava/lang/String;)V"),
    ("java/util/zip/ZipFile", "<init>", "(Ljava/io/File;)V"),
    ("java/util/zip/ZipFile", "<init>", "(Ljava/lang/String;)V"),
    ("java/util/zip/ZipFile", "close", "()V"),
    ("java/util/zip/ZipFile", "entries", "()Ljava/util/Enumeration;"),
    ("java/util/zip/ZipFile", "getComment", "()Ljava/lang/String;"),
    ("java/util/zip/ZipFile", "getEntry", "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;"),
    ("java/util/zip/ZipFile", "getInputStream", "(Ljava/util/zip/ZipEntry;)Ljava/io/InputStream;"),
    ("java/util/zip/ZipFile", "getName", "()Ljava/lang/String;"),
    ("java/util/zip/ZipFile", "size", "()I"),
    ("java/util/zip/ZipFile", "stream", "()Ljava/util/stream/Stream;"),
];

/// Lane 1 wave 3 — `java/util/HashMap` and its six view/iterator classes, the
/// 98 §1.4 shadows the census calls bucket A or B.
///
/// ## Why this family was HELD until now, and what the hold turned out to be
///
/// The 2026-09-10 reading was "the state IS real but nine probes move", and
/// the 2026-09-11 reading narrowed it to ONE observable:
/// `hashMap.entrySet().toArray()` answered a zero-length array where HotSpot
/// answers three, while `size()`, the entry ITERATOR, `forEach`, `stream`,
/// `spliterator` and both other views were already exact. `keySet().toArray()`
/// and `values().toArray()` were exact too, which is what made it look like a
/// defect in the entry view specifically.
///
/// It is not. `apps/probes/L1EntrySetRouteProbe.java` asks the one question
/// that separates the two routes that can produce that array — what happens
/// to a TYPED destination:
///
/// ```text
///   HotSpot            entrySet().toArray(new String[0])  ArrayStoreException
///   dial-armed         entrySet().toArray(new String[0])  [0]java.lang.String
/// ```
///
/// Real `AbstractCollection.toArray(T[])` `aastore`s each element and so MUST
/// throw for three `Map.Entry`s and a `String[]`; it cannot throw for an empty
/// walk. A quiet `String[0]` therefore means a NATIVE answered and believed the
/// view was empty. `CRATONVM_DBG_TOARRAY` names it:
///
/// ```text
///   [DBG_TOARRAY] native_al_to_array (0-arg) HIT nargs=1
///   [DBG_TOARRAY] al_or_collection_elements recv=java/util/HashMap$EntrySet
///                 heuristic_len=0 nulls=0 suspect=false
///   WARN zgc real: field index OOB index=1 num_slots=1 op="get"   (x10)
/// ```
///
/// and the chain is:
///
/// 1. the dial's prefix `java/util/HashMap` also covers `$EntrySet`, so
///    `entrySet()` yields and hands back the image's own `HashMap$EntrySet` —
///    `this$0` set, identity stable, the map's own `entrySet` field populated,
///    all three verified against HotSpot;
/// 2. that object has ONE slot, and `hs_map_slot` puts a view carrier's
///    backing at `class_num_total_fields` — slot 1 — so every read is out of
///    bounds and `hs_backing_map` is empty (the ten `zgc` warnings are that
///    read);
/// 3. `toArray` on it resolves up to `java/util/AbstractCollection`, which is
///    NOT armed, so `native_al_to_array` fires;
/// 4. its documented fallback for an unmodelled layout is "ask the receiver's
///    own `size()`, and walk the real `iterator()` if it is non-zero" — and
///    that question is asked from INSIDE a native, where no dispatch door
///    exists. So it reaches `native_hs_size` on `HashMap$EntrySet`, which finds
///    no backing, tries `try_delegate_real_collection`, and that helper's
///    `invoke_special` re-finds the SAME native, trips its own re-entrancy
///    guard and returns the sentinel. `real_size == 0`, no walk, `[0]`.
///
/// **So the dial's nine red rows were a dial artefact, and the ops page's rule
/// cuts both ways: an armed sweep is not a verdict when it is GREEN, and it is
/// not a verdict when it is RED either.** Retirement does not decline at a
/// door — it removes the registration, so step 4's question reaches real
/// bytecode and answers 3. Measured, not argued: the trial binary takes
/// `apps/probes/L1MapFamilySweep.java` from 9 diffs to 0.
///
/// Arming `AbstractCollection`, `AbstractSet`, `AbstractMap`, `Set`,
/// `Collection` and `Map` alongside changes nothing, and now there is a reason
/// rather than a shrug: the door that would have to decline is not on the
/// `toArray` call at all, it is on the `size()` call the native makes, and that
/// call has no door.
///
/// ## What the family is
///
/// 98 triples over seven classes, every one bucket A or B:
///
/// ```text
///   java/util/HashMap                 A 30   B 3
///   java/util/HashMap$EntrySet        A  7   B 14
///   java/util/HashMap$KeySet          A  9   B 12
///   java/util/HashMap$Values          A  8   B  6
///   java/util/HashMap$EntryIterator   A  1   B  2
///   java/util/HashMap$KeyIterator     A  1   B  2
///   java/util/HashMap$ValueIterator   A  1   B  2
///   java/util/HashMap$Node            A  1
/// ```
///
/// They move as a SET and cannot move any other way. `hs_map_slot` puts a view
/// carrier's backing past the class's declared fields, `key_itr_carrier_for`
/// mints `HashMap$KeyIterator` for the view's cursor, and `map_state` reads the
/// map's own bucket array — retire the map and keep the views and the views
/// read a backing nothing fills; retire the views and keep the map and the
/// map's own `keySet()` native mints a carrier whose natives are gone.
///
/// ## Precondition 4 is measured, not waived
///
/// Three earlier probe runs reached 29 of the 98. `apps/probes/
/// L1MapFamilySweep.java` — 142 rows, written for this wave — reaches the rest
/// through ordinary Java: every constructor, every default-method override,
/// both iterator `remove()` contracts, `Node.setValue` through a detached
/// entry, serialization round-trip, comodification, a 100-entry resize and a
/// 12-way hash collision chain.
///
/// ## What retiring it REPAIRS
///
/// The nine rows above are §1.4 defects that yielding fixes, and three of them
/// are silent wrong answers rather than throws — `entrySet().toArray()`,
/// `new ArrayList<>(entrySet())` and `new HashSet<>(entrySet())` all read
/// EMPTY for a three-entry map on the armed binary. The unarmed binary is also
/// worse than HotSpot on five reflective rows this wave fixes (see
/// `apps/probes/L1MapFieldProbe.java`): `new HashMap<>()` + three puts leaves
/// `threshold = 0` where HotSpot has 12, `new HashMap<>(64)` leaves
/// `threshold = 64` where HotSpot has 48, and the copy constructor and
/// `new HashMap<>(Map.of(..))` leave `table = [16]java.lang.Object` — an
/// UNTYPED array where HotSpot has a typed `HashMap$Node[]` — with
/// `loadFactor = 0.0`.
///
/// `java/util/LinkedHashMap` is NOT in this table even though it is a
/// `HashMap` subclass: its entries live in `lhm_overlay()`, a Rust side table
/// the real bodies cannot read, and that is §10 item 2's own change.
/// `java/util/Hashtable` is not here either — its fields already match HotSpot
/// unarmed and its four moving probes are a separate question.
static RETIRED_SHADOW_L1_HM_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/HashMap", "<init>", "()V"),
    ("java/util/HashMap", "<init>", "(I)V"),
    ("java/util/HashMap", "<init>", "(IF)V"),
    ("java/util/HashMap", "<init>", "(Ljava/util/Map;)V"),
    ("java/util/HashMap", "clear", "()V"),
    (
        "java/util/HashMap",
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
    ),
    ("java/util/HashMap", "containsKey", "(Ljava/lang/Object;)Z"),
    (
        "java/util/HashMap",
        "containsValue",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
    ),
    (
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/HashMap", "isEmpty", "()Z"),
    (
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/HashMap", "putAll", "(Ljava/util/Map;)V"),
    (
        "java/util/HashMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    ("java/util/HashMap", "size", "()I"),
    ("java/util/HashMap", "toString", "()Ljava/lang/String;"),
];

/// Lane 1 wave 4 — the 29 `java/util/jar/` and `java/text/` rows that a dial
/// BISECTION cleared, once the whole-prefix red was traced to one class each.
///
/// ## §7's two vacuous greens were also two unbisected reds
///
/// The 2026-09-10 revision of the lane page held both families because
/// `apps/probes/L1TailSweep.java` turned each prefix red — jar `+28`, text
/// `+11` — after a 44-probe subset had called both `0 worse` with the dial
/// never engaging. That was the right call and it stopped there: "neither has
/// been bisected to a method".
///
/// Bisected on 2026-09-11 against `cratonvm-l1hm-base-20260911`, one class at
/// a time, scored on `L1TailSweep` (base 13 diffs), with the dial's own
/// engagement printed beside each row so a vacuous arm cannot pass as a green:
///
/// ```text
///   java/util/jar/                 +34 WORSE   reached=146
///     java/util/jar/JarFile        +34 WORSE   reached=4    <- the whole red
///     java/util/jar/JarEntry        +0 same    reached=16
///     java/util/jar/Manifest        +0 same    reached=23
///     java/util/jar/Attributes      +0 same    reached=156
///     java/util/jar/Attributes$Name +0 same    reached=127
///   java/text/                     +16 WORSE   reached=4
///     java/text/BreakIterator      +16 WORSE   reached=4    <- the whole red
///     java/text/ParseException      +0 same    reached=1
///     java/text/Normalizer          +0 same    reached=10
///     java/text/DateFormat          +0 same    reached=0    <- VACUOUS
/// ```
///
/// Each family's red is ONE class, and `java/util/jar/JarFile` is the same
/// class wave 2 already had to retire three defect rows out of. Those two
/// stay `Bridge` and keep their prefixes off this table; `DateFormat`'s single
/// registration is excluded for the reason §7 exists — its green said nothing.
///
/// ## What yielding REPAIRS: nine of thirteen rows, measured
///
/// `apps/probes/L1JarTextSweep.java` (87 rows, written for this wave) is
/// thirteen rows out from HotSpot 25.0.4+7 on the control. Armed on this
/// table's five classes together — 1,119 door engagements, so not a vacuous
/// arm — it is **four**: nine repaired, none made worse. Every one of the
/// thirteen is a §1.4 defect, the native answering where the image's own
/// body throws or throwing where it answers:
///
/// ```text
///   A.getValue.nullName    HotSpot NullPointerException   control null
///   A.put.rejectsString    HotSpot ClassCastException     control null
///   N.ctor.empty           HotSpot IllegalArgumentException  control ""
///   N.ctor.null            HotSpot NullPointerException   control null
///   N.ctor.illegalChar     HotSpot IllegalArgumentException  control "bad name"
///   N.ctor.tooLong         HotSpot IllegalArgumentException  control 71
///   M.ctorStream.garbage   HotSpot 0                      control IOException
///   M.ctorStream.null      HotSpot NPE "this.in is null"  control NPE, own text
///   M.ctorCopy.null        HotSpot NullPointerException   control 0
///   E.ctorName.null        HotSpot NullPointerException   control null
///   E.attributesFromJar    HotSpot "section-value"        control null
///   P.printStackTrace.writer / .stream   two more, same shape
/// ```
///
/// `Attributes$Name`'s constructor is the sharpest of them: the image
/// validates the header name (non-empty, ≤ 70 characters, `[0-9A-Za-z_-]`
/// only) and this VM validated nothing at all, so `new Attributes.Name("bad
/// name")` produced a Name that can never appear in a real manifest.
/// `E.attributesFromJar` is the only one that is a wrong VALUE rather than a
/// missing throw: a jar's per-entry manifest section was invisible.
///
/// The four that remain are not this table's to fix, and saying which is the
/// point of measuring them:
///
/// * `E.attributesFromJar` — `java/util/jar/JarFile`'s, which stays `Bridge`;
/// * `M.ctorStream.null` — the NPE is raised at the right place with the
///   right type and the message lacks its `because "this.in" is null`
///   clause, which is the helpful-NPE-message gap and not a jar defect;
/// * the two `P.printStackTrace` rows — see below.
///
/// ## `java/text/ParseException` was in this table and came out
///
/// It is the wave's own refusal, on its own measurement. Armed alone it is
/// `+0` — it repairs nothing — and two of its rows trade one wrong answer
/// for another:
///
/// ```text
///   e.setStackTrace(new StackTraceElement[0]); e.printStackTrace(w)
///     HotSpot   java.text.ParseException: bad
///     control   (wrong, one way)
///     armed     java.text.ParseException: bad
///                 at java.text.ParseException.<init>(ParseException.java:64)
///                 at ... six more frames
/// ```
///
/// Thirteen of its fourteen registrations are `java/lang/Throwable`'s
/// inherited surface, so what the yield exposes is that this VM's `Throwable`
/// model does not read back a `stackTrace` array that BYTECODE wrote — a
/// `Throwable` defect that a `java/text/` retirement merely made visible.
/// `printStackTrace` is called by too much real code to change its output for
/// no repair, so the class stays and the finding is written down instead.
///
/// ## Why these five and not the prefix
///
/// `java/util/jar/JarFile` is the family's producer and it stays; that is not
/// a half-retirement of the kind wave 3 hit, because `JarEntry`, `Manifest`
/// and `Attributes` are VALUES a `JarFile` hands out rather than carriers the
/// VM mints under a borrowed class name. The discriminator is whether a
/// SURVIVING native can be handed an object real bytecode built: for wave 3's
/// `HashMap$KeyIterator` it could, and did; here the natives that survive are
/// `JarFile`'s own and they are handed `JarFile`s.
static RETIRED_SHADOW_L1_JT_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/text/Normalizer",
        "isNormalized",
        "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Z",
    ),
    (
        "java/text/Normalizer",
        "normalize",
        "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Ljava/lang/String;",
    ),
    ("java/util/jar/Attributes", "<init>", "()V"),
    ("java/util/jar/Attributes", "<init>", "(I)V"),
    (
        "java/util/jar/Attributes",
        "containsKey",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/util/jar/Attributes", "entrySet", "()Ljava/util/Set;"),
    (
        "java/util/jar/Attributes",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/jar/Attributes",
        "getValue",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/util/jar/Attributes",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/jar/Attributes",
        "putValue",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    ),
    ("java/util/jar/Attributes", "size", "()I"),
    (
        "java/util/jar/Attributes$Name",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/jar/Attributes$Name",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/util/jar/Attributes$Name", "hashCode", "()I"),
    (
        "java/util/jar/Attributes$Name",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/jar/JarEntry", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/util/jar/JarEntry",
        "getComment",
        "()Ljava/lang/String;",
    ),
    ("java/util/jar/JarEntry", "getCompressedSize", "()J"),
    ("java/util/jar/JarEntry", "getMethod", "()I"),
    ("java/util/jar/JarEntry", "getName", "()Ljava/lang/String;"),
    ("java/util/jar/JarEntry", "getSize", "()J"),
    ("java/util/jar/JarEntry", "isDirectory", "()Z"),
    ("java/util/jar/Manifest", "<init>", "()V"),
    (
        "java/util/jar/Manifest",
        "<init>",
        "(Ljava/io/InputStream;)V",
    ),
    (
        "java/util/jar/Manifest",
        "<init>",
        "(Ljava/io/InputStream;Ljava/lang/String;)V",
    ),
    (
        "java/util/jar/Manifest",
        "<init>",
        "(Ljava/util/jar/JarVerifier;Ljava/io/InputStream;Ljava/lang/String;)V",
    ),
    (
        "java/util/jar/Manifest",
        "<init>",
        "(Ljava/util/jar/Manifest;)V",
    ),
    ("java/util/jar/Manifest", "getEntries", "()Ljava/util/Map;"),
    (
        "java/util/jar/Manifest",
        "getMainAttributes",
        "()Ljava/util/jar/Attributes;",
    ),
];

/// Lane 1, wave 5 — `sun/util/calendar/ZoneInfoFile`, the one row of the
/// Date/TimeZone/Locale family that a bisection left standing.
///
/// ## Why two rows and not the forty
///
/// §10 item 6 of the lane page holds `Date`/`TimeZone`/`sun/util/calendar/`
/// (40) and `Locale` + providers (35) together, and the reason they were held
/// together was that nobody had asked them separately. Armed one class at a
/// time on `LocaleDateTzShadowSweep` (base 2 diffs), 2026-09-11:
///
/// ```text
///   java/util/Locale                       +117 WORSE  rc 1   reached=58
///   sun/util/calendar/ZoneInfo               +4 WORSE         reached=5723
///   java/util/TimeZone                       +2 WORSE         reached=53
///   java/util/Currency                       +2 WORSE         reached=4
///   sun/util/calendar/ZoneInfoFile           +0 same          reached=3792
///   java/util/Date                           +0 same          reached=0   VACUOUS
///   sun/util/locale/provider/CalendarDataUtility      +0      reached=0   VACUOUS
///   sun/util/locale/provider/JRELocaleProviderAdapter +0      reached=0   VACUOUS
///   sun/util/locale/provider/LocaleResources          +0      reached=0   VACUOUS
///   sun/util/resources/Bundles                        +0      reached=0   VACUOUS
///   sun/util/resources/LocaleData                     +0      reached=0   VACUOUS
/// ```
///
/// Six of the seven `+0` rows are §7's vacuity trap wearing a green coat: the
/// probe never asks those classes anything, so `reached == 0` and the row says
/// NOTHING. `ZoneInfoFile` is the one `+0` that means something — 3,792 door
/// engagements and no diff — and it is the only row here.
///
/// **A green dial arm is a candidate, not a verdict, and so is a red one.**
/// This wave's own sibling finding is that `java/util/HashMap` sat held for two
/// page revisions on nine rows a trial binary reads as zero
/// (`RETIRED_SHADOW_L1_HM_TRIPLES`), and the converse — an armed arm that
/// agrees with base is exactly what a LEAKED dial row also looks like. So the
/// 3,792 buys this table a place in the queue, not a landing: the acceptance is
/// a trial binary against the whole probe tree, per the four preconditions in
/// the ops page §7.
///
/// ## What the two rows are
///
/// Both are registered in `native-builtins/src/lib.rs` and both answer with
/// `alloc_synth_timezone` — a `sun/util/calendar/ZoneInfo` this VM builds from
/// its own tzdb parse, with `ID`/`rawOffset`/`dstSavings` populated. The real
/// bytecode reads `tzdb.dat` out of the image instead. They are a pair on
/// purpose: `getZoneInfo` is the public entry point, `getZoneInfo0` the private
/// one it delegates to, and retiring one without the other would leave a
/// synthesised object being handed to real bytecode or the reverse — the
/// half-retirement shape wave 3 measured on `HashMap`'s iterators.
///
/// ## What is NOT here, and why the prefix is narrow
///
/// `sun/util/locale/provider/` and `sun/util/resources/` are the five vacuous
/// rows above, and they are all one blocker: `LocaleResources` answers `null`
/// for the class-based bundle families (`getBreakIteratorInfo`,
/// `getDateTimePattern`), which is what makes `java/text/BreakIterator`'s
/// yield throw `AbstractMethodError` and what BUG-15 in
/// `vm/src/vm/vm_exec.rs` pins a native over. That is one engineering front
/// under four of the lane's remaining items, and it is §10's, not this table's.
/// Admitting `sun/util/` as a prefix would put all of it one binary search from
/// a future table for no gain today, so the prefix names this class alone.
///
/// ## ADDENDUM, wave 6 (2026-09-11): the six vacuous rows are vacuous no more
///
/// A `reached == 0` row is a request for a workload, and wave 6 wrote one:
/// `apps/probes/L1LocaleProviderWorkload`, 147 rows driving every public API
/// whose real-JDK implementation goes through these classes. Armed one class
/// per process against the wave-6 control binary, the six rows above now
/// read:
///
/// ```text
///   java/util/Date                                     +0   reached=14
///   sun/util/locale/provider/CalendarDataUtility      +12   reached=108
///   sun/util/locale/provider/JRELocaleProviderAdapter   +0   reached=8
///   sun/util/locale/provider/LocaleResources          +12   reached=254
///   sun/util/resources/Bundles                          +0   reached=0
///   sun/util/resources/LocaleData                       +0   reached=21
/// ```
///
/// and four neighbours the same run priced for the first time:
///
/// ```text
///   sun/util/calendar/          +0   reached=411   (ZoneInfoFile already out)
///   java/util/TimeZone          +0   reached=196
///   java/util/Currency         +38   reached=75
///   java/util/Locale            +6   reached=16156
/// ```
///
/// So the paragraph above is HALF wrong and the half matters:
/// `LocaleResources` and `CalendarDataUtility` are genuinely blocked (+12
/// each, on 108 and 254 engagements), and `Currency` and `Locale` are worse
/// still. But `Date`, `JRELocaleProviderAdapter`, `LocaleData`, `TimeZone` and
/// the rest of `sun/util/calendar/` are CANDIDATES with engagement behind
/// them, and the sentence that called all five one blocker was reasoning from
/// an empty set. `sun/util/resources/Bundles` is the one row still at
/// `reached=0`, and it stays a non-answer.
///
/// A candidate is still not a verdict -- that is this table's own standing
/// warning, and none of these four has had a trial binary yet.
static RETIRED_SHADOW_L1_ZI_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/util/calendar/ZoneInfoFile",
        "getZoneInfo",
        "(Ljava/lang/String;)Lsun/util/calendar/ZoneInfo;",
    ),
    (
        "sun/util/calendar/ZoneInfoFile",
        "getZoneInfo0",
        "(Ljava/lang/String;)Lsun/util/calendar/ZoneInfo;",
    ),
];

/// Lane 1 wave 6, 2026-09-11: `java/text/BreakIterator`, all seventeen.
///
/// The family the lane page held for two waves as "one throw, and it is the
/// locale provider's". It was, and this is the other end of that thread.
///
/// # Why seventeen and not a subset
///
/// Because the blocker was never per-method. `java.text.BreakIterator` is
/// ABSTRACT: everything the JDK hands back is a `sun.text.RuleBasedBreakIterator`,
/// a `sun.text.DictionaryBasedBreakIterator` or a
/// `BreakIteratorProviderImpl$GraphemeBreakIterator`, and once the real chain
/// builds one, every one of the seventeen is answered by that object's own
/// bytecode. Retiring half would leave the fabricated carrier reachable from
/// the other half's factories.
///
/// # The three things that had to be true, each measured on its own binary
///
/// ```text
///   wave 5   LocaleResources.getBreakIteratorInfo / getBreakIteratorResources
///            answer from the image. `non_cldr_packages`: the family is not
///            one CLDR re-generated, and the blanket `cldr` mapping made
///            every candidate miss.
///   wave 6   setText(String) and preceding(int) -- the only two of the
///            seventeen that are CONCRETE on the abstract class -- step aside
///            for a receiver this VM did not fabricate. Until they did, the
///            chain built the RIGHT object and then wrote the text into slot 0
///            of an object whose slot 0 is `charCategoryTable`.
///   wave 6   the BREAKITER pin in `vm/src/vm/vm_exec.rs` is removed, so the
///            four static factories run their own bytecode.
/// ```
///
/// `apps/probes/L1BreakIterRealProbe` is the instrument, and it was built to
/// be measurable BEFORE any of this landed: its `P.*` rows reach
/// `BreakIteratorProviderImpl` directly, which the pin never covered. On the
/// control binary the chain already answered `sun.text.RuleBasedBreakIterator`
/// and every walk over it was `[0]`. On the wave-6 binary:
///
/// ```text
///   control  28 rows differ from HotSpot   (13 of them P.*, 15 F.*)
///   setText   16 rows differ               (0 P.*, all 16 the pinned F.*)
///   pin off    0 rows differ
/// ```
///
/// # What is NOT retired, and why the tempting row is absent
///
/// `java/text/BreakIterator` is the whole table. The natives stay registered
/// for synthetic-JDK mode, where there is no bytecode to prefer and the
/// fabricated carrier is the only BreakIterator there is -- retirement is
/// `--jdk-only`'s refusal and does not touch that mode.
///
/// The two `LocaleResources` readers wave 5 added are NOT retirable and must
/// not be swept in by a later widening of a prefix: they are the floor this
/// family now stands on, force-listed at both dispatch doors, and the family
/// returns to "Cannot load from null array" without them.
/// Lane 1 wave 6, 2026-09-11: the two locale-provider rows the vacuity
/// workload turned from non-answers into candidates, one method each.
///
/// Both were `reached == 0` in wave 5's bisection -- `+0` computed over an
/// empty set. `apps/probes/L1LocaleProviderWorkload` gave them a workload and
/// they came back with engagement and no divergence:
///
/// ```text
///   sun/util/resources/LocaleData.getBundle              +0   reached=21
///   .../JRELocaleProviderAdapter.getLocaleServiceProvider +0   reached=8
/// ```
///
/// Each class carries exactly ONE registration in the whole tree (grep for
/// the class name: one hit each, in `locale_resources.rs` and
/// `locale_bootstrap.rs`), so "retire the class" and "retire the method" are
/// the same act here and no half-retirement is possible.
///
/// THE TWO SIBLINGS THAT ARE NOT HERE ARE THE POINT. The same run measured
/// `LocaleResources` at +12 over 254 engagements and `CalendarDataUtility` at
/// +12 over 108, and the paragraph in `RETIRED_SHADOW_L1_ZI_TRIPLES` that
/// called all five "one blocker" was reasoning from `reached == 0`. Four of
/// them are candidates; two are measured blockers. Neither fact was visible
/// before there was a workload.
///
/// `sun/util/resources/Bundles` remains at `reached=0` even under the new
/// workload and is deliberately absent: a row nothing reaches is a row
/// nothing has measured.
static RETIRED_SHADOW_L1_LP_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/util/locale/provider/JRELocaleProviderAdapter",
        "getLocaleServiceProvider",
        "(Ljava/lang/Class;)Ljava/util/spi/LocaleServiceProvider;",
    ),
    (
        "sun/util/resources/LocaleData",
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/ResourceBundle;",
    ),
];

static RETIRED_SHADOW_L1_BI_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/text/BreakIterator", "current", "()I"),
    ("java/text/BreakIterator", "first", "()I"),
    ("java/text/BreakIterator", "following", "(I)I"),
    (
        "java/text/BreakIterator",
        "getCharacterInstance",
        "()Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getCharacterInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getLineInstance",
        "()Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getLineInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getSentenceInstance",
        "()Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getSentenceInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getText",
        "()Ljava/text/CharacterIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getWordInstance",
        "()Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getWordInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
    ),
    ("java/text/BreakIterator", "last", "()I"),
    ("java/text/BreakIterator", "next", "()I"),
    ("java/text/BreakIterator", "preceding", "(I)I"),
    ("java/text/BreakIterator", "previous", "()I"),
    (
        "java/text/BreakIterator",
        "setText",
        "(Ljava/lang/String;)V",
    ),
];

/// Lane 4 wave 1, 2026-09-11: 140 rows over 10 classes of `java/io/` and
/// `java/nio/`.
///
/// A SEVENTH table rather than rows merged into a sibling, for the reason the
/// second and third give for existing: these were adjudicated by a different
/// METHOD, and the method is the part worth seeing at a glance.
///
/// # The method: a per-family dial sweep, then the SAME families armed together
///
/// `apps/probes/l4famsweep.sh` arms `CRATONVM_ENFORCE_NATIVE_SHADOW` on one
/// receiver family at a time and diffs thirteen lane-4 probes against a real
/// HotSpot 25 on the same host. On 2026-09-10, with the `FileInputStream.skip`
/// residual fixed, sixteen of its eighteen families scored **0**:
///
/// ```text
///   java/io/PrintStream  File  FileInputStream  FileOutputStream
///   java/io/ByteArrayInputStream  ByteArrayOutputStream  DataInputStream
///   java/io/DataOutputStream  BufferedReader  BufferedWriter
///   java/io/FilterOutputStream   java/nio/ByteBuffer   java/nio/CharBuffer
///   java/nio/file/spi/FileSystemProvider   java/nio/file/attribute/
///   java/nio/channels/FileChannel                                    DIFF 0
///   java/nio/file/Files                                              DIFF 4
///   java/nio/file/Path                                             DIFF 100
/// ```
///
/// **Sixteen per-family greens are not one green for the wave.** Phase 2 armed
/// 270 classes one at a time, called 236 retire-safe, and arming those 236
/// together failed 54 of 118 corpus vectors. So the families were re-armed as
/// ONE scope, which is what a wave actually does, and measured against an
/// unarmed control on the same binary:
///
/// ```text
///   control (dial off)                 0 diffs over 4416 rows, 0 short
///   the 13 wave-1 families armed       0 diffs over 4416 rows, 0 short
///   ... plus java/io/PrintStream       0 diffs over 4416 rows, 0 short
/// ```
///
/// # The dial was ASKED, and this is the number that says so
///
/// A zero from an instrument that never ran reads exactly like a zero from one
/// that did. Under the wave-1 scope the dial's own counters report **180 268
/// dispatches reached and 176 112 YIELDED** across the thirteen probes -- that
/// many native calls declined and served by real JDK bytecode instead, for
/// 4 416 rows that stayed byte-identical to HotSpot. `L4TypedBufferSweep`
/// alone yields 27 849 times and moves no row.
///
/// # The funnel
///
/// Step 1 of the lane's increment loop, unioned over the whole instrument
/// rather than one probe: a registry dump per probe (13 dumps, schema 5,
/// `--explain-jdk-only` so the image columns are populated), and a row is a
/// candidate only if it **owns its slot**, is kind `Bridge`, its image
/// declaring method carries `Code` (bucket A or B), and it was **invoked at
/// least once** in one of those runs.
///
/// ```text
///   bucket C / D / E / F (no bytecode to yield to)      867  not candidates
///   not a Bridge                                         17
///   invocations 0 in all 13 runs                        820  unmeasured, not safe
///   outside a DIFF-0 family                             226  Files, Path, sun/nio/ch, …
///   CANDIDATES                                          191
///   backed out by the BUILD (see below)                  -51
///   RETIRED                                             140
/// ```
///
/// `invocations 0` is the largest exclusion and it is deliberately NOT read as
/// "unreachable": schema 5's `invocations_complete` bit exists because the
/// column is a lower bound. Those 820 rows are unmeasured by this instrument,
/// which is a reason not to retire them here, not a verdict about them.
///
/// # What the build found that the dial could not, and why the dial cannot
///
/// The screen above said 0 for all thirteen probes with the wave armed. Built,
/// the same thirteen probes moved **eight rows in strict mode** — and none in
/// compatible mode, which is the half that identifies the mechanism:
///
/// ```text
///   L4TailSweep2        3 rows   Files.createLink -> UnsupportedOperationException
///   L4TypedBufferSweep  3 rows   CharBuffer subSequence/toString, wrong WINDOW
///   TailFamilySweep     2 rows   the same two, through the asCharBuffer view
///
/// (`diff` counts both sides, so that is 16 differing LINES. The count that
/// matters is eight rows, in two families.)
/// ```
///
/// **The dial is not a faithful model of a retirement, and this is the shape of
/// the gap.** The dial declines a native at DISPATCH, and its decline is
/// conditional: when the receiver's own class has no concrete body to yield to
/// (`dispatch_has_code == false`) it answers no and the native runs anyway —
/// that is the `declined_no_bytecode` column, 4 156 of 180 268 here. A
/// retirement has no such fallback: the registration is simply not there, and
/// the call lands wherever real dispatch takes it. Every row the dial scored 0
/// on *because it declined to decline* is therefore unmeasured by it.
///
/// `java/nio/file/spi/FileSystemProvider` is exactly that row. This VM hands
/// out a fabricated provider stamped with the ABSTRACT class, so `createLink`
/// has no concrete body, so the dial ran the native and read 0. Retired, the
/// call reaches the abstract declaration and answers
/// `UnsupportedOperationException` for all three rows — the
/// fabricate-an-abstract-class trade, arrived at from the registration side.
/// All four of its rows are out.
///
/// `java/nio/CharBuffer` is the second, and it is §4's named failure mode
/// rather than a throw: `subSequence(1,3)` answered `cd` where HotSpot answers
/// `bc`, and `toString()` after `position(2)` answered the empty string. The
/// real `CharBuffer` bodies derive their window from `position()`/`limit()`,
/// and this VM's carrier does not keep those where the real accessors read
/// them, so retiring the accessors hands the real bodies the wrong window. It
/// produces correct-LOOKING output with the wrong characters, which is the one
/// outcome the lane page says to design the probe for. All seventeen of its
/// rows are out. `java/nio/ByteBuffer`'s thirty-four stay: the same accessors,
/// the same probe, zero rows moved — so this is a CharBuffer carrier defect and
/// not a buffer-wide one.
///
/// The four `ByteBufferAsCharBuffer{B,L,RB,RL}.order()` rows stay too. They were
/// suspected with CharBuffer and cleared by the re-measurement: `order()` is a
/// constant, and with CharBuffer's accessors restored those views are at 0.
///
/// # The corpus found three more, and the probe tree could not have
///
/// The thirteen probes scored 0 in BOTH modes on the built binary -- 4 416 rows,
/// binary against binary. The `--jdk-only` corpus went **132/132 -> 129/132**,
/// reproducibly, on the same binary:
///
/// ```text
///   RFileTimes       every timestamp reads 1970-01-01T00:00:00Z
///   RJdkSecurity     a property-named truststore IGNORED: 122 anchors for 1,
///                    and a certificate HotSpot REJECTS is ACCEPTED
///   RSslLiveSession  fails at client.responseCode = 200
/// ```
///
/// `RFileTimes` has one cause and it is the SETTER, not the getter:
/// `Files.setLastModifiedTime` reads `FileTime.toMillis()` off a fabricated
/// carrier, gets 0, and stamps the file at the epoch -- so all four read-back
/// rows follow from one write. Dial-armed attribution names
/// `java/nio/file/attribute/` for it and `java/io/ByteArrayInputStream` for
/// `RSslLiveSession`; `java/io/File` armed alone is clean, so `File.lastModified`
/// reading 1970 was a symptom of the same write and not a second defect.
///
/// `RJdkSecurity` reproduces under NO single family and not under all thirteen
/// armed together -- the dial's blind spot described above, and the dial was
/// never going to name it. What did was an eight-line probe,
/// `apps/probes/L4AbsPath.java`, printing path SHAPES rather than paths:
///
/// ```text
///   temp.getPath          abs=true len=32 slashes=2      (correct)
///   temp.isAbsolute       false                          (HotSpot: true)
///   temp.getAbsolutePath  abs=true len=45 slashes=5      (the cwd, prepended)
/// ```
///
/// Every `java.io.File` this VM builds kept its path in slot 0 and wrote
/// nothing else, so real `File` bytecode read `prefixLength = 0` and called
/// every path relative. Fixed in the commit before this table, in
/// [`crate::file_layout`], at all six producing call sites across two crates.
/// **That fix is why the six `prefixLength`-dependent `File` rows below --
/// `isAbsolute`, `getAbsolutePath`, `getAbsoluteFile`, `getCanonicalPath`,
/// `getCanonicalFile`, `toURI` -- are retired here rather than carved out.**
///
/// Five fabricated carriers in this lane have now been found with their real
/// fields empty: `Path`, `CharBuffer`, `FileTime`, `FileSystemProvider` and
/// `File`. The carrier is the blocker, not the retirement.
///
/// The whole `--jdk-only` corpus, armed on the trimmed scope, is back at its
/// unarmed baseline. **That screen belongs before the build, not after it**, and
/// its absence is what cost this wave five of its seven builds.
///
/// # What is held back, and why
///
///  * **`java/io/PrintStream`.** It scores 0 armed, alone
///    and in the wave, and it is still not here: `System.out` and `System.err`
///    are how every lane reads its probes, so a regression there reads as all
///    nine lanes failing at once. The lane page requires it in its own wave
///    with its own commit naming the blast radius. The measurement above is
///    that wave's evidence, taken early.
///  * **The five families the builds backed out (51 rows).**
///    `java/nio/CharBuffer` (17) and `java/nio/file/spi/FileSystemProvider` (4)
///    from the probe tree; `java/nio/file/attribute/` (12) and
///    `java/io/ByteArrayInputStream` (9) from the corpus, attributed by dial
///    sweep; and the file-handle group (9) — `java/io/FileOutputStream`,
///    `FileCleanable`, `FileDescriptor` and the abstract-receiver
///    `java/nio/channels/FileChannel` — as an un-attributed GROUP, because
///    `RJdkSecurity` reproduces under no dial scope at all. Every one of them is
///    blocked on a CARRIER defect rather than on the retirement: a fabricated
///    receiver whose real fields this VM never writes.
///  * **`java/nio/file/Files` (DIFF 4) and `java/nio/file/Path` (DIFF 100).**
///    Above the floor, so not candidates. `Path`'s 100 are one defect: the
///    carrier is stamped with the INTERFACE, so `toString()` lands on
///    `Object.toString()`. See `crate::path_layout`.
///  * **`sun/nio/ch/`.** The 2026-08-19 package verdict stands; this wave does
///    not try to beat it.
///  * **`native-io/src/concrete_receiver.rs:185`**, a cross-lane registrar of
///    191 rows over 21 classes that lane T owns whole. The funnel filters it by
///    call site and it removed NOTHING here -- its classes are `sun/nio/ch`,
///    already outside the wave -- so the filter is a guard for later waves
///    rather than a thing that fired.
///  * **`jdk/internal/foreign`.** Nothing under it reached the candidate set,
///    so its prefix is not admitted either.
///
/// The wave's only FFM-adjacent rows went out with CharBuffer:
/// `session()Ljdk/internal/foreign/MemorySessionImpl;` and `checkSession()V`
/// were on that class. Nothing retired here touches
/// `jdk/internal/foreign`, whose carrier is this VM's own allocation shape
/// rather than the JDK's
/// (`docs/known-issues/jdk-only/the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`).
///
/// Measured on **linux/x86_64 against JDK 25**. `java/io/File`'s separators,
/// absolute-path rules and permission methods differ by OS, and the two shell
/// gates in this campaign are keyed `25/linux` and refuse on Windows, so that
/// is the platform this table is measured on and the only one.
static RETIRED_SHADOW_L4_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/io/ByteArrayOutputStream", "<init>", "()V"),
    ("java/io/ByteArrayOutputStream", "<init>", "(I)V"),
    ("java/io/ByteArrayOutputStream", "close", "()V"),
    ("java/io/ByteArrayOutputStream", "flush", "()V"),
    ("java/io/ByteArrayOutputStream", "reset", "()V"),
    ("java/io/ByteArrayOutputStream", "size", "()I"),
    ("java/io/ByteArrayOutputStream", "toByteArray", "()[B"),
    (
        "java/io/ByteArrayOutputStream",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/ByteArrayOutputStream",
        "toString",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/io/ByteArrayOutputStream",
        "toString",
        "(Ljava/nio/charset/Charset;)Ljava/lang/String;",
    ),
    ("java/io/ByteArrayOutputStream", "write", "(I)V"),
    ("java/io/ByteArrayOutputStream", "write", "([B)V"),
    ("java/io/ByteArrayOutputStream", "write", "([BII)V"),
    ("java/io/DataInputStream", "available", "()I"),
    ("java/io/DataInputStream", "close", "()V"),
    ("java/io/DataInputStream", "read", "()I"),
    ("java/io/DataInputStream", "read", "([BII)I"),
    ("java/io/DataInputStream", "readBoolean", "()Z"),
    ("java/io/DataInputStream", "readByte", "()B"),
    ("java/io/DataInputStream", "readChar", "()C"),
    ("java/io/DataInputStream", "readDouble", "()D"),
    ("java/io/DataInputStream", "readFloat", "()F"),
    ("java/io/DataInputStream", "readFully", "([B)V"),
    ("java/io/DataInputStream", "readFully", "([BII)V"),
    ("java/io/DataInputStream", "readInt", "()I"),
    ("java/io/DataInputStream", "readLong", "()J"),
    ("java/io/DataInputStream", "readShort", "()S"),
    ("java/io/DataInputStream", "readUTF", "()Ljava/lang/String;"),
    ("java/io/DataInputStream", "readUnsignedByte", "()I"),
    ("java/io/DataInputStream", "readUnsignedShort", "()I"),
    ("java/io/DataInputStream", "skipBytes", "(I)I"),
    (
        "java/io/DataOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
    ),
    ("java/io/DataOutputStream", "close", "()V"),
    ("java/io/DataOutputStream", "flush", "()V"),
    ("java/io/DataOutputStream", "size", "()I"),
    ("java/io/DataOutputStream", "write", "(I)V"),
    ("java/io/DataOutputStream", "write", "([BII)V"),
    ("java/io/DataOutputStream", "writeBoolean", "(Z)V"),
    ("java/io/DataOutputStream", "writeByte", "(I)V"),
    ("java/io/DataOutputStream", "writeChar", "(I)V"),
    ("java/io/DataOutputStream", "writeDouble", "(D)V"),
    ("java/io/DataOutputStream", "writeFloat", "(F)V"),
    ("java/io/DataOutputStream", "writeInt", "(I)V"),
    ("java/io/DataOutputStream", "writeLong", "(J)V"),
    ("java/io/DataOutputStream", "writeShort", "(I)V"),
    (
        "java/io/DataOutputStream",
        "writeUTF",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/io/File",
        "<init>",
        "(Ljava/io/File;Ljava/lang/String;)V",
    ),
    ("java/io/File", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/io/File",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    ),
    ("java/io/File", "<init>", "(Ljava/net/URI;)V"),
    ("java/io/File", "canExecute", "()Z"),
    ("java/io/File", "canRead", "()Z"),
    ("java/io/File", "canWrite", "()Z"),
    ("java/io/File", "compareTo", "(Ljava/io/File;)I"),
    ("java/io/File", "createNewFile", "()Z"),
    (
        "java/io/File",
        "createTempFile",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/io/File;)Ljava/io/File;",
    ),
    ("java/io/File", "delete", "()Z"),
    ("java/io/File", "equals", "(Ljava/lang/Object;)Z"),
    ("java/io/File", "exists", "()Z"),
    ("java/io/File", "getAbsoluteFile", "()Ljava/io/File;"),
    ("java/io/File", "getAbsolutePath", "()Ljava/lang/String;"),
    ("java/io/File", "getCanonicalFile", "()Ljava/io/File;"),
    ("java/io/File", "getCanonicalPath", "()Ljava/lang/String;"),
    ("java/io/File", "getFreeSpace", "()J"),
    ("java/io/File", "getName", "()Ljava/lang/String;"),
    ("java/io/File", "getParent", "()Ljava/lang/String;"),
    ("java/io/File", "getParentFile", "()Ljava/io/File;"),
    ("java/io/File", "getPath", "()Ljava/lang/String;"),
    ("java/io/File", "getTotalSpace", "()J"),
    ("java/io/File", "getUsableSpace", "()J"),
    ("java/io/File", "hashCode", "()I"),
    ("java/io/File", "isAbsolute", "()Z"),
    ("java/io/File", "isDirectory", "()Z"),
    ("java/io/File", "isFile", "()Z"),
    ("java/io/File", "isHidden", "()Z"),
    ("java/io/File", "lastModified", "()J"),
    ("java/io/File", "length", "()J"),
    ("java/io/File", "list", "()[Ljava/lang/String;"),
    (
        "java/io/File",
        "list",
        "(Ljava/io/FilenameFilter;)[Ljava/lang/String;",
    ),
    ("java/io/File", "listFiles", "()[Ljava/io/File;"),
    (
        "java/io/File",
        "listFiles",
        "(Ljava/io/FileFilter;)[Ljava/io/File;",
    ),
    (
        "java/io/File",
        "listFiles",
        "(Ljava/io/FilenameFilter;)[Ljava/io/File;",
    ),
    ("java/io/File", "listRoots", "()[Ljava/io/File;"),
    ("java/io/File", "mkdir", "()Z"),
    ("java/io/File", "mkdirs", "()Z"),
    ("java/io/File", "renameTo", "(Ljava/io/File;)Z"),
    ("java/io/File", "setExecutable", "(Z)Z"),
    ("java/io/File", "setExecutable", "(ZZ)Z"),
    ("java/io/File", "setLastModified", "(J)Z"),
    ("java/io/File", "setReadOnly", "()Z"),
    ("java/io/File", "setReadable", "(Z)Z"),
    ("java/io/File", "setReadable", "(ZZ)Z"),
    ("java/io/File", "setWritable", "(Z)Z"),
    ("java/io/File", "setWritable", "(ZZ)Z"),
    ("java/io/File", "toPath", "()Ljava/nio/file/Path;"),
    ("java/io/File", "toString", "()Ljava/lang/String;"),
    ("java/io/File", "toURI", "()Ljava/net/URI;"),
    ("java/io/FilterOutputStream", "close", "()V"),
    ("java/io/FilterOutputStream", "flush", "()V"),
    ("java/io/FilterOutputStream", "write", "(I)V"),
    ("java/io/FilterOutputStream", "write", "([B)V"),
    ("java/io/FilterOutputStream", "write", "([BII)V"),
    (
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "array", "()[B"),
    ("java/nio/ByteBuffer", "arrayOffset", "()I"),
    ("java/nio/ByteBuffer", "capacity", "()I"),
    ("java/nio/ByteBuffer", "clear", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "clear", "()Ljava/nio/ByteBuffer;"),
    (
        "java/nio/ByteBuffer",
        "compareTo",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    ("java/nio/ByteBuffer", "equals", "(Ljava/lang/Object;)Z"),
    ("java/nio/ByteBuffer", "flip", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "flip", "()Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "get", "([B)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "hasArray", "()Z"),
    ("java/nio/ByteBuffer", "hasRemaining", "()Z"),
    ("java/nio/ByteBuffer", "hashCode", "()I"),
    ("java/nio/ByteBuffer", "limit", "()I"),
    ("java/nio/ByteBuffer", "limit", "(I)Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "limit", "(I)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "mark", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "mark", "()Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "order", "()Ljava/nio/ByteOrder;"),
    (
        "java/nio/ByteBuffer",
        "order",
        "(Ljava/nio/ByteOrder;)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "position", "()I"),
    ("java/nio/ByteBuffer", "position", "(I)Ljava/nio/Buffer;"),
    (
        "java/nio/ByteBuffer",
        "position",
        "(I)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "put",
        "(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "put", "([B)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "put", "([BII)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "remaining", "()I"),
    ("java/nio/ByteBuffer", "reset", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "rewind", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "rewind", "()Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "wrap", "([B)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "wrap", "([BII)Ljava/nio/ByteBuffer;"),
    (
        "java/nio/ByteBufferAsCharBufferB",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "java/nio/ByteBufferAsCharBufferL",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "java/nio/ByteBufferAsCharBufferRB",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "java/nio/ByteBufferAsCharBufferRL",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
];

/// Lane 4 wave 2, 2026-09-11: the nine `ValueLayouts$Of*Impl` carriers.
///
/// **An EIGHTH table**, and the first under `jdk/internal/foreign/`. Wave 1's
/// note said "nothing under it reached the candidate set, so its prefix is not
/// admitted either" — that was a statement about the instrument, not about the
/// package. The lane's ledger put 479 rows in "no probe in this tree invokes
/// it" and named this shape as over half of them; one probe,
/// `apps/probes/L4FfmLayoutSweep.java`, moved every one of these into the
/// funnel in a single pass.
///
/// # The funnel
///
/// 146 registrations over nine classes, and the census taken from that probe's
/// own run reports `invocations > 0` on **all 146**, with the schema-5
/// `invocations_complete` bit set on every row — so this is not the lower bound
/// that bucket usually is. 83 are bucket A and 63 bucket B (`byteSize`,
/// `byteAlignment`, `name`, `order`, `carrier`, `byteOffset`, inherited from
/// `AbstractLayout` and `ValueLayouts$AbstractValueLayout`, which is where the
/// real bodies live).
///
/// # What the retirement does, row by row
///
/// Measured on `vm-l4ffm-fix`, one binary against itself, with the dial scoped
/// to exactly this wave (`CRATONVM_ENFORCE_NATIVE_SHADOW=jdk/internal/foreign/
/// layout/ValueLayouts$`), against HotSpot 25 over the probe's 359 rows:
///
/// ```text
///   unarmed --jdk-only          25 rows differ
///   armed on this wave          25 rows differ    <- the SAME number
/// ```
///
/// The same number and **not the same rows**, which is the whole finding. 19
/// rows the native answered wrongly become right, because the real bodies throw
/// the JDK's own text and render the JDK's own string:
///
/// ```text
///   withByteAlignment(3)   "Invalid alignment constraint: 3" -> "Invalid alignment: 3"     x9
///   byteOffset(groupElement)  a home-grown message -> "Bad layout path: ..."               x9
///   ADDRESS.withTargetLayout(JAVA_INT).toString()   "a8" -> "a8:i4"                        x1
/// ```
///
/// and 19 become wrong, every one of them `varHandle`. **So `varHandle` is
/// carved out and this table is 137 rows, not 146.** The real
/// `AbstractValueLayout.varHandle()` runs `Utils.makeSegmentViewVarHandle`,
/// which on this VM ends in `NoClassDefFoundError: java/lang/invoke/
/// BoundMethodHandle` or hands back a CratonVM VarHandle with no variable-type
/// metadata — `varType()` and `coordinateTypes()` then refuse, and a
/// `vh.set`/`vh.get` round trip through a heap segment reads `size=0`. That is
/// a MethodHandle-infrastructure gap, not a layout one, and it is the only
/// method of the thirteen that the real body cannot service.
///
/// With `varHandle` out, the arm is the floor minus those 19: **six rows**, and
/// none of them is on a class in this table.
///
/// # The precondition, which is the commit before this one
///
/// This wave does not stand on its own. The carrier fix in
/// `native-builtins/src/phases_late/foreign_ffm.rs` had to land first, for the
/// same reason wave 1's `java.io.File` fix had to: **a layout minted by this VM
/// did not hold what the real bodies read.** `AbstractLayout.name` is an
/// `Optional<String>` and the mint wrote a bare reference; `carrier` was never
/// written at all, and the native `carrier()` derived its answer from the class
/// NAME instead — so the method answered correctly while the field behind it
/// was null. Armed before that fix, this same scope took the probe from 100
/// differing lines to **358**; after it, to the floor. The dial did not find
/// the defect and could not have: it names a family, and the failure was
/// underneath all nine of them.
///
/// # What is held back, and why
///
/// The prefix admits `jdk/internal/foreign/layout/`; the table decides. Of the
/// 251 bucket-A/B rows under `jdk/internal/foreign` in this census:
///
///  * **`varHandle` (9)** — above.
///  * **The group layouts (35): `StructLayoutImpl`, `UnionLayoutImpl`,
///    `SequenceLayoutImpl`, `PaddingLayoutImpl`.** One carrier defect, not
///    thirteen: `AbstractGroupLayout.elements` is declared
///    `java.util.List<MemoryLayout>` and this VM stores a java ARRAY in that
///    field. Armed, `memberLayouts()` answers 0 where the oracle answers 2,
///    `byteOffset(groupElement("c"))` cannot resolve a member that is plainly
///    there, and every group `toString` dies in
///    `NoSuchMethodError: 'int java.lang.foreign.MemoryLayout.size()'`. It is
///    the same SHAPE as the defect this wave fixed and it wants the same
///    treatment — a real `List`, written at the mint — which is a change to the
///    group factories and belongs in its own commit with its own measurement.
///  * **The segment, arena and session carriers (70).** Blocked by a decision
///    on record, not by a missing measurement:
///    `docs/known-issues/jdk-only/the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`
///    settles that `cratonvm/internal/foreign/MemorySegmentImpl` is the VM's own
///    allocation shape and is laid out DELIBERATELY unlike
///    `AbstractMemorySegmentImpl`, whose `length`/`readOnly`/`scope` would alias
///    the carrier's `ptr`/`size`/`arena`. Retiring one of these would run a real
///    body over those three slots. That is not a wave that needs screening; it
///    is a wave that must not be run while that decision stands.
///  * **`java/lang/foreign/*`** — the interfaces. Bucket C: abstract in the
///    image, dispatched through no door, and still named by the FFM arm of
///    `force_native_over_real_jdk_bytecode`, which this commit narrows to
///    exactly them.
///
/// # The residual this wave found and did not fix
///
/// COMPATIBLE mode keeps a second, independent copy of the mint:
/// `make_prepared_value_layout` in `vm/src/vm/vm_util.rs`, the preseed that
/// gives `ValueLayout.JAVA_INT` and its fifteen siblings their statics before
/// `<clinit>`. It resolves `byteSize`, `byteAlignment` and `name` by name and
/// writes **three of the five** real fields; `carrier` and `order` stay null,
/// and `ValueLayout.JAVA_INT.withName("k").equals(...)` still throws
/// `NullPointerException` there. `--jdk-only` drops that preseed entirely and
/// runs the real `<clinit>`, which is why the strict arm is clean and the
/// compatible one is not (50 differing lines against 68). **It does not affect
/// this table**: `NativeKind::allowed_in(Compatible)` is `true` for every kind,
/// so a retired triple still dispatches its native in compatible mode and the
/// re-tag is a `--jdk-only` change only.
///
/// Measured on **linux/x86_64 against JDK 25**, the platform both shell gates
/// in this campaign are keyed to.
static RETIRED_SHADOW_L4_FFM_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "targetLayout",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/AddressLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/AddressLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/AddressLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withTargetLayout",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/AddressLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfBoolean;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfBoolean;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfBoolean;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfByte;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfByte;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfByte;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfChar;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfChar;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfChar;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfDouble;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfDouble;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfDouble;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfFloat;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfFloat;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfFloat;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfInt;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfInt;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfInt;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfLong;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfLong;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfLong;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfShort;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfShort;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfShort;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
];

/// Lane 4 wave 3, 2026-09-12: the four GROUP carriers.
///
/// `StructLayoutImpl`, `UnionLayoutImpl`, `SequenceLayoutImpl` and
/// `PaddingLayoutImpl` — the half of `jdk/internal/foreign/layout/` wave 2 held
/// back, under the prefix wave 2 admitted. 28 rows of 31: `varHandle` is carved
/// out on all three classes that register it, for the reason wave 2 carved it
/// out on the nine value layouts.
///
/// # The carrier was the blocker, and it was ONE defect for all four classes
///
/// `jdk.internal.foreign.layout.AbstractGroupLayout` declares
/// `List<MemoryLayout> elements` and this VM stored a bare ARRAY in it; `kind`
/// and `minByteAlignment` were never written at all. `memberLayouts()` is
/// `return elements;` — one `getfield` — so the first real body to touch a
/// group got an array where the JDK's own code calls `List` methods:
///
/// ```text
///   struct.memberLayouts().size()      0          (oracle 2)
///   struct.byteOffset(groupElement)    cannot resolve a member plainly there
///   struct.toString()                  NoSuchMethodError: MemoryLayout.size()
/// ```
///
/// Fixed in the commit before this table, through one reader and one writer:
/// `p67_group_members` decodes whichever shape the field holds and returns the
/// COUNT beside the array (an `ArrayList` has capacity past its size, and
/// reading `array_length` would invent trailing members); `p67_group_set_members`
/// writes the list; `p67_group_set_kind` writes `kind` from the enum's own
/// statics, so there is one `STRUCT` object in the VM and `==` on it answers
/// what the JDK expects.
///
/// **The list is UNMODIFIABLE and that is measured, not tidy.** HotSpot answers
/// `UnsupportedOperationException` to `memberLayouts().add(...)`; since the
/// retired accessor returns the field itself, an `ArrayList` here would hand
/// out a mutable view of a layout's members.
///
/// # The funnel
///
/// 31 distinct triples over the four classes, every one of them reached by
/// `apps/probes/L4FfmLayoutSweep.java` — six were cold on the first pass
/// (`PaddingLayoutImpl.byteAlignment`, the three `withName` covariant bridges
/// at the `MemoryLayout` static type, and `UnionLayoutImpl`'s `byteOffset` and
/// `varHandle`) and the probe was extended until none was. A row no probe
/// invokes is a row the funnel must not take.
///
/// # What the retirement does
///
/// One binary against itself, the group table un-retired with
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` naming the four classes -- which prints
/// `7 + 7 + 8 + 6 = 28 table row(s)` and is the arm's own receipt that it armed
/// this table and not its neighbour:
///
/// ```text
///   control (origin/dev)                    13 rows differ
///   carrier fix, group table un-retired      3
///   carrier fix + this table                 2
/// ```
///
/// **The retirement fixes one row and breaks none.** `byteOffset` on a padding
/// layout throws the JDK's own `Bad layout path: attempting to select a group
/// element from a non-group layout: x4` where the native answered its own
/// wording. Eleven of the control's thirteen were the carrier, which is the
/// commit before this one; the two left over are neither this wave's nor a
/// group's, and both are in `varHandle`:
///
///  * `ADDRESS.varHandle().varType()` answers `long` where the oracle says
///    `MemorySegment`;
///  * a union's `varHandle` ACCEPTS a misaligned access HotSpot refuses -- a
///    missing refusal, which is the silent-wrong-answer shape this lane owns,
///    in the one method carved out of both FFM waves.
///
/// **28 refusals, 0 survivors**, and on the control all 28 were `native-won`:
/// a native winning over real bytecode that was there the whole time.
///
/// Measured on **linux/x86_64 against JDK 25**.
static RETIRED_SHADOW_L4_FFM_GROUP_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "elementCount",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "elementLayout",
        "()Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "memberLayouts",
        "()Ljava/util/List;",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "memberLayouts",
        "()Ljava/util/List;",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
];

/// The 2026-09-11 lane-L6 wave: `java/net/HttpURLConnection` and
/// `ProxySelector.getDefault` -- 15 rows of a lane of 966, after the corpus
/// refused 109 that the probe tree had cleared.
///
/// A fifth table rather than rows merged into a sibling, for the reason the
/// second gives for existing: these were adjudicated by a different METHOD --
/// eight differential probes written for this lane, and then six builds of
/// paired corpus runs -- and the method is the part worth being able to see at
/// a glance.
///
/// # The probe tree cleared 124 rows. The corpus refused 109 of them.
///
/// This is the finding of the wave and it goes first, because it re-states one
/// of the four preconditions and every future lane will meet it.
///
/// Armed on the candidate prefixes, **all 125 probes in the tree got no worse**
/// and five got dramatically better. Built, and run as a two-binary A/B
/// against a control from the same merged tree, the same five improved --
/// `L6UriSweep` 80 diff lines to 0, `L6X500Sweep` 276 to 0,
/// `L6HttpLogicSweep` 64 to 18, `L6InetSweep` 380 to 368, `L6SocketSweep` 42
/// to 38 -- and **zero probes regressed**. Both instruments said 124 rows.
///
/// The `--jdk-only` corpus, paired against that control and run alone, said
/// 15. The ladder, one build per row:
///
/// ```text
///   #  table                          corpus (paired, alone)   measured out
///   1  124 rows, 10 classes           trial 126/132            URL, DatagramSocket,
///                                     ctrl  132/132            MulticastSocket, Inet*  (70)
///   2   54 rows,  4 classes           trial 130/132            URI (29) -- RJdkBridge1
///   3   25 rows,  3 classes           RSslLiveSession 3/3 red  nothing: a guess
///   4   11 rows,  2 classes           RSslLiveSession 3/3 red  nothing: the same guess
///   5   10 rows, X500Principal alone  RSslLiveSession 3/3 red  X500Principal (10)
///   6   15 rows                       trial 132/132, ctrl 132/132   --
/// ```
///
/// Builds 3 and 4 are listed because they are the cost of guessing:
/// `HttpURLConnection` was the plausible culprit for an HTTPS vector, was
/// dropped twice, was innocent, and is one of the two classes this table
/// carries.
///
/// # Precondition 3 is checked one frame too high
///
/// Every refusal is the same species:
///
/// ```text
///   RJdkServices              NPE  URLStreamHandler.openConnection, "this.handler" is null
///   RServiceLoaderDoubleSource   (same)
///   RJdkDefineClass           NPE  URLStreamHandler.getDefaultPort,  "this.handler" is null
///   RJdkNet                   ULE  sun/nio/ch/DatagramChannelImpl.receive0
///   RJdkNet  (InetAddress)    ULE  java/net/Inet6AddressImpl.lookupAllHostAddr
///   RNetIfaceScope            every scoped IPv6 address must round-trip, 2 did not
///   RJdkBridge1               URL.toURI().getPath() lost a lone surrogate to U+FFFD
///   RSslLiveSession           CK client.responseCode = 200 unclassified
/// ```
///
/// Precondition 3 asks whether the IMAGE METHOD carries `Code` to yield to.
/// All 124 rows passed it. It does not ask what that code then CALLS:
///
///  * `java.net.URL`'s methods are one line each through `this.handler`, a
///    field only the real constructor writes -- and this VM MINTS `URL`
///    objects in `classloader.rs` without running it;
///  * `X500Principal` has one declared instance field, `transient X500Name
///    thisX500Name`, and `native-builtins/src/jca/x500.rs` documents in its own
///    header that this VM repurposes that slot to hold a **String**. Any JDK
///    bytecode on the class dereferences a String as an `X500Name`;
///  * `java.net.URI` is the same shape one level out: `URL.toURI()` allocates a
///    real `java.net.URI` and publishes fields into it rather than running its
///    constructor;
///  * `InetAddress.getByName` and `DatagramSocket` yield into
///    `Inet6AddressImpl.lookupAllHostAddr` and
///    `sun/nio/ch/DatagramChannelImpl.receive0`, both `ACC_NATIVE` and neither
///    implemented here -- so the retirement trades a shadow for an
///    `UnsatisfiedLinkError`, one frame deeper than precondition 3 looks.
///
/// **So precondition 3 is really: the image method carries `Code`, AND that
/// code's own callees are satisfiable in this VM.** Four of the six are a
/// field only a real constructor writes, which is `Class.getModule`'s
/// situation from lane-0 §7. A VM that allocates a JDK carrier without
/// constructing it has, for every such class, a shadow that cannot be retired
/// until the carrier is built properly.
///
/// # The dial is a lead in both directions, and a hand-run vector is not a run
///
/// Two instrument traps, both paid for here:
///
///  * the dial (`CRATONVM_ENFORCE_NATIVE_SHADOW`) was **optimistic** on
///    `URL`/`DatagramSocket`/`Inet*`, **pessimistic on the wrong vector** for
///    `X500Principal` (it failed `RJdkX509Intercept`, which passes on every
///    binary built here), and **silent** about `RSslLiveSession`, the vector
///    that actually refuses it. It DECLINES at dispatch and arms a PREFIX;
///    this table re-tags at REGISTRATION and is per-triple;
///  * `cratonvm --jdk-only -cp build:... RJdkBridge1` passes on a binary whose
///    HARNESS run of the same vector fails. `run.sh` builds the classpath,
///    sets the flags and applies the cross-VM diff. The isolation that settles
///    a vector is `ONLY=<Vector> bash regression-suite/run.sh`, three runs on
///    each binary.
///
/// # Why the lane's other prefixes carry no table
///
/// Each was armed ALONE on the whole 125-probe tree:
///
/// ```text
///   javax/net/                                    2 probes worse   L6TlsParamSweep 66 -> 94
///   java/security/,sun/security/,
///     javax/crypto/,javax/security/               8 probes worse   SecuritySurfaceSweep 0 -> 2594
///   sun/net/                                      0               123 of 125 VACUOUS
///   jdk/net/,jdk/internal/net/                    0               123 of 125 VACUOUS
/// ```
///
/// A VACUOUS arm is not a pass: `sun/net/` and the two `jdk` prefixes reached
/// the dial in 2 probes of 125, which is precondition 1 and the trap 146 of
/// Phase 2's 236 candidates fell into.
///
/// Three structural reasons sit behind the rest, each recorded in full in
/// `docs/internal/retired/lane-6-net-security-RETIRED-20260910.md`:
///
///  1. **`javax/net/ssl/` and `sun/security/ssl/` are an IMPLEMENTATION.**
///     This VM's TLS is rustls (`native-builtins/src/t27_tls.rs`, 20,630
///     lines). Yielding does not restore a JDK behaviour this VM approximates
///     -- it removes TLS. That is `StrictMath`'s situation: a 0-diff probe is
///     evidence the family WORKS, never on its own a reason to retire it.
///  2. **The 67 `HttpsURLConnectionImpl` rows are a null-`delegate`
///     workaround.** `register_https_delegate_forwarders` exists because this
///     VM ALLOCATES that carrier rather than constructing it, and each
///     forwarder runs the SUPERCLASS body the Impl overrides -- the
///     retirement's own remedy, one level up.
///  3. **A base-class row loses the dispatch to its own subclass.** 15 of
///     `InetAddress`'s 18 bucket-A rows are never dispatched, because every
///     instance is an `Inet4Address` or an `Inet6Address` and the door asks
///     the registry about the DECLARING class.
///
/// # The refusals are not inert
///
/// A refusal is a retirement only when nothing already owns the triple:
/// `JdkOnlyViolation::SyntheticNativeRegistered` carries a `survivor`, and a
/// non-null one means an earlier registration is still serving, so strict mode
/// runs that older native and every probe reads exactly as before. Measured on
/// the trial binary over a `--jdk-only-report` of the probe tree: every row of
/// this table appears in the refusal set and ZERO refusals carry a survivor.
static RETIRED_SHADOW_L6_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/net/HttpURLConnection", "<init>", "(Ljava/net/URL;)V"),
    ("java/net/HttpURLConnection", "addRequestProperty", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/net/HttpURLConnection", "getHeaderFieldDate", "(Ljava/lang/String;J)J"),
    ("java/net/HttpURLConnection", "getInstanceFollowRedirects", "()Z"),
    ("java/net/HttpURLConnection", "getRequestMethod", "()Ljava/lang/String;"),
    ("java/net/HttpURLConnection", "getRequestProperties", "()Ljava/util/Map;"),
    ("java/net/HttpURLConnection", "getRequestProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/net/HttpURLConnection", "setChunkedStreamingMode", "(I)V"),
    ("java/net/HttpURLConnection", "setConnectTimeout", "(I)V"),
    ("java/net/HttpURLConnection", "setDoOutput", "(Z)V"),
    ("java/net/HttpURLConnection", "setFixedLengthStreamingMode", "(I)V"),
    ("java/net/HttpURLConnection", "setReadTimeout", "(I)V"),
    ("java/net/HttpURLConnection", "setRequestMethod", "(Ljava/lang/String;)V"),
    ("java/net/HttpURLConnection", "setRequestProperty", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/net/ProxySelector", "getDefault", "()Ljava/net/ProxySelector;"),
];

/// Is this exact triple a retired §1.4 shadow?
///
/// The class-name prefix test is a cheap discriminator: every entry is under
/// `java/util/`, and almost no registration is, so the common case costs one
/// prefix compare and nothing else.
///
/// The prefix was `java/util/logging/` until the 2026-08-12 collections wave;
/// widening it to `java/util/` is not a widening of what is retired — the
/// binary search still decides that — only of what gets asked. An entry under
/// a prefix this test rejects answers `false`, which reads as "not retired" and
/// is invisible; `every_entry_is_reachable_through_the_predicate` is the guard.
/// `java/util/` now admits `java/util/concurrent/`, `java/util/stream/` and the
/// rest of the package tree to one extra binary search each, which is the whole
/// cost, and `the_held_collection_families_are_not_retired` is the test that
/// says admitting them changes no answer.
/// The 2026-09-11 lane-5 RESIDUAL wave: 67 `sun/misc/Unsafe` rows, and the
/// reason they were held until now was a missing INSTRUMENT, not a blocker.
///
/// The retired lane page held all 82 `sun/misc/Unsafe` registrations with
/// "precondition 1 fails by measurement" — 121 probes reported the dial VACUOUS
/// on that scope and the 132 `--jdk-only` corpus reports reached 18 of the 82.
/// Its own §9 said what that was worth: **until a workload exists, the count is
/// not evidence of anything.** It is not evidence the rows are right, not
/// evidence they are retirable, and not evidence they are dead.
///
/// `apps/probes/L5SunMiscUnsafe.java` is that workload, and with it the four
/// preconditions read:
///
/// ```text
///   1. the dial was asked              y/r = 76/76 on the workload
///   2. whole probe tree no worse       134 measured: 0 toward, 1 away, and
///                                      that row is y/r=0/0 VACUOUS with a
///                                      line count that moved — not the dial
///   3. the image target carries Code   outcome=bytecode-won on all 67, and
///                                      `javap -p sun.misc.Unsafe` reports
///                                      ZERO native methods on the class —
///                                      all 99 carry Code and delegate to
///                                      `theInternalUnsafe`
///   4. a per-triple dispatch observed  67 distinct triples, one row each
/// ```
///
/// **The wave was 48 rows for an afternoon.** The first workload reached 48 of
/// them; widening it to the volatile twins, the long atomics and the bulk
/// memory trio reached 19 more, and this page's own account had said that is
/// what takes them — *a probe edit, not a build*. It cost one probe edit and
/// two runs.
///
/// Precondition 2 was measured on the 36-row workload and is NOT re-run for
/// the 19: the battery's question is whether arming this prefix disturbs the
/// OTHER 133 probes, and widening one probe cannot change their answer. The
/// row that did change is this probe's own, and it was re-measured directly —
/// `d(base,armed) = 0` on 50 rows, so arming the prefix changes nothing about
/// its output at all.
///
/// Precondition 3 is not read off a `javap` here: `outcome=bytecode-won` in the
/// `--jdk-only-report` IS the image's bytecode having run and produced the
/// answer. And the answer is the same one: the probe is **byte-identical armed
/// and unarmed**, 36 rows, including the two lines that differ from HotSpot for
/// an unrelated reason (see below). A retirement that changes no answer while
/// refusing 48 natives is the definition of a shadow.
///
/// # What `sun.misc.Unsafe` actually is on JDK 25, measured
///
/// Fully functional. Every field accessor and its volatile twin, all three
/// `compareAndSwap*`, `getAndAdd`/`getAndSet`, the static-field pair,
/// `allocateMemory`/`setMemory`/`freeMemory`, `allocateInstance`,
/// `getLoadAverage`, `park`/`unpark` and `throwException` answer on HotSpot 25
/// and on CratonVM alike — only a terminal-deprecation WARNING is printed, on
/// stderr, which the A/B harness drops. The class being deprecated for removal
/// says nothing about whether it works today.
///
/// # The 34 rows NOT here, and why each is out
///
///   * **`ensureClassInitialized(Ljava/lang/Class;)V` and
///     `shouldBeInitialized(Ljava/lang/Class;)Z` are ABSENT from the JDK 25
///     image** — `javap -p sun.misc.Unsafe` declares neither, and the workload
///     gets `NoSuchMethodException` for both on HotSpot. Nothing can dispatch
///     them on a supported image, so they are bucket-F DELETIONS rather than
///     retirements, the same verdict `AbstractExecutorService`'s four rows got.
///     Left for a deletion commit with its own census: a retirement table entry
///     would claim a dispatch nobody has observed, which is the rule this table
///     is under.
///   * **`getUnsafe()Lsun/misc/Unsafe;` is NOT absent, and the first version of
///     this note said it was.** `javap -p sun.misc.Unsafe` prints
///     `public static sun.misc.Unsafe getUnsafe();` on the 17, 21 AND 25
///     images. The `NoSuchMethodException` that put it in the list above came
///     from `getMethod`, and it is the JDK's core-reflection METHOD FILTER
///     working: `jdk.internal.reflect.Reflection.methodFilterMap` hides this
///     one method from the reflective surface, which is the door that stops a
///     library from acquiring `Unsafe` reflectively.
///
///     So it is not a deletion candidate, and CratonVM's `getUnsafe` native is
///     correct — it throws `SecurityException` for a caller off the boot path,
///     measured against HotSpot. What diverges is that this VM implements no
///     member filter AT ALL, so the method is reflectively visible here and
///     invisible there. That is cross-cutting rather than lane 5's, and it has
///     its own page:
///     `docs/known-issues/jdk-only/core-reflection-has-no-member-filter-20260911.md`.
///     It stays out of this table because the divergence is in the reflective
///     surface, not in the native, and retiring the native would not move it.
///
///     **The lesson is worth more than the row: the image is not the authority
///     on what reflection answers.** A census built from class files cannot see
///     this defect, and a `javap` check would have prevented the wrong claim —
///     which is what eventually caught it.
///   * **the rest were not dispatched by this workload.** Precondition 4 is
///     per-triple and this table honours that: a row with no observed dispatch
///     stays out however obvious its sibling looks. The registered surface on
///     this class is larger than the workload reaches, and closing the gap is
///     more probe rows rather than a weaker rule.
///
/// # Why this is a separate table from `RETIRED_SHADOW_L5_TRIPLES`
///
/// Same reason the Phase 2 table gives for existing: these were adjudicated by
/// a different instrument, on a different day, against a measurement the
/// earlier wave explicitly did not have. Merging them would make the earlier
/// table's account cover rows it never saw.
static RETIRED_SHADOW_L5R_TRIPLES: &[(&str, &str, &str)] = &[
    ("sun/misc/Unsafe", "addressSize", "()I"),
    ("sun/misc/Unsafe", "allocateInstance", "(Ljava/lang/Class;)Ljava/lang/Object;"),
    ("sun/misc/Unsafe", "allocateMemory", "(J)J"),
    ("sun/misc/Unsafe", "arrayBaseOffset", "(Ljava/lang/Class;)I"),
    ("sun/misc/Unsafe", "arrayIndexScale", "(Ljava/lang/Class;)I"),
    ("sun/misc/Unsafe", "compareAndSwapInt", "(Ljava/lang/Object;JII)Z"),
    ("sun/misc/Unsafe", "compareAndSwapLong", "(Ljava/lang/Object;JJJ)Z"),
    ("sun/misc/Unsafe", "compareAndSwapObject", "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z"),
    ("sun/misc/Unsafe", "copyMemory", "(Ljava/lang/Object;JLjava/lang/Object;JJ)V"),
    ("sun/misc/Unsafe", "freeMemory", "(J)V"),
    ("sun/misc/Unsafe", "fullFence", "()V"),
    ("sun/misc/Unsafe", "getAndAddInt", "(Ljava/lang/Object;JI)I"),
    ("sun/misc/Unsafe", "getAndAddLong", "(Ljava/lang/Object;JJ)J"),
    ("sun/misc/Unsafe", "getAndSetInt", "(Ljava/lang/Object;JI)I"),
    ("sun/misc/Unsafe", "getAndSetLong", "(Ljava/lang/Object;JJ)J"),
    ("sun/misc/Unsafe", "getAndSetObject", "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;"),
    ("sun/misc/Unsafe", "getBoolean", "(Ljava/lang/Object;J)Z"),
    ("sun/misc/Unsafe", "getBooleanVolatile", "(Ljava/lang/Object;J)Z"),
    ("sun/misc/Unsafe", "getByte", "(J)B"),
    ("sun/misc/Unsafe", "getByte", "(Ljava/lang/Object;J)B"),
    ("sun/misc/Unsafe", "getByteVolatile", "(Ljava/lang/Object;J)B"),
    ("sun/misc/Unsafe", "getChar", "(Ljava/lang/Object;J)C"),
    ("sun/misc/Unsafe", "getCharVolatile", "(Ljava/lang/Object;J)C"),
    ("sun/misc/Unsafe", "getDouble", "(Ljava/lang/Object;J)D"),
    ("sun/misc/Unsafe", "getDoubleVolatile", "(Ljava/lang/Object;J)D"),
    ("sun/misc/Unsafe", "getFloat", "(Ljava/lang/Object;J)F"),
    ("sun/misc/Unsafe", "getFloatVolatile", "(Ljava/lang/Object;J)F"),
    ("sun/misc/Unsafe", "getInt", "(Ljava/lang/Object;J)I"),
    ("sun/misc/Unsafe", "getIntVolatile", "(Ljava/lang/Object;J)I"),
    ("sun/misc/Unsafe", "getLoadAverage", "([DI)I"),
    ("sun/misc/Unsafe", "getLong", "(J)J"),
    ("sun/misc/Unsafe", "getLong", "(Ljava/lang/Object;J)J"),
    ("sun/misc/Unsafe", "getLongVolatile", "(Ljava/lang/Object;J)J"),
    ("sun/misc/Unsafe", "getObject", "(Ljava/lang/Object;J)Ljava/lang/Object;"),
    ("sun/misc/Unsafe", "getObjectVolatile", "(Ljava/lang/Object;J)Ljava/lang/Object;"),
    ("sun/misc/Unsafe", "getShort", "(Ljava/lang/Object;J)S"),
    ("sun/misc/Unsafe", "getShortVolatile", "(Ljava/lang/Object;J)S"),
    ("sun/misc/Unsafe", "loadFence", "()V"),
    ("sun/misc/Unsafe", "objectFieldOffset", "(Ljava/lang/reflect/Field;)J"),
    ("sun/misc/Unsafe", "pageSize", "()I"),
    ("sun/misc/Unsafe", "park", "(ZJ)V"),
    ("sun/misc/Unsafe", "putBoolean", "(Ljava/lang/Object;JZ)V"),
    ("sun/misc/Unsafe", "putBooleanVolatile", "(Ljava/lang/Object;JZ)V"),
    ("sun/misc/Unsafe", "putByte", "(Ljava/lang/Object;JB)V"),
    ("sun/misc/Unsafe", "putByteVolatile", "(Ljava/lang/Object;JB)V"),
    ("sun/misc/Unsafe", "putChar", "(Ljava/lang/Object;JC)V"),
    ("sun/misc/Unsafe", "putCharVolatile", "(Ljava/lang/Object;JC)V"),
    ("sun/misc/Unsafe", "putDouble", "(Ljava/lang/Object;JD)V"),
    ("sun/misc/Unsafe", "putDoubleVolatile", "(Ljava/lang/Object;JD)V"),
    ("sun/misc/Unsafe", "putFloat", "(Ljava/lang/Object;JF)V"),
    ("sun/misc/Unsafe", "putFloatVolatile", "(Ljava/lang/Object;JF)V"),
    ("sun/misc/Unsafe", "putInt", "(Ljava/lang/Object;JI)V"),
    ("sun/misc/Unsafe", "putIntVolatile", "(Ljava/lang/Object;JI)V"),
    ("sun/misc/Unsafe", "putLong", "(JJ)V"),
    ("sun/misc/Unsafe", "putLong", "(Ljava/lang/Object;JJ)V"),
    ("sun/misc/Unsafe", "putLongVolatile", "(Ljava/lang/Object;JJ)V"),
    ("sun/misc/Unsafe", "putObject", "(Ljava/lang/Object;JLjava/lang/Object;)V"),
    ("sun/misc/Unsafe", "putObjectVolatile", "(Ljava/lang/Object;JLjava/lang/Object;)V"),
    ("sun/misc/Unsafe", "putShort", "(Ljava/lang/Object;JS)V"),
    ("sun/misc/Unsafe", "putShortVolatile", "(Ljava/lang/Object;JS)V"),
    ("sun/misc/Unsafe", "reallocateMemory", "(JJ)J"),
    ("sun/misc/Unsafe", "setMemory", "(Ljava/lang/Object;JJB)V"),
    ("sun/misc/Unsafe", "staticFieldBase", "(Ljava/lang/reflect/Field;)Ljava/lang/Object;"),
    ("sun/misc/Unsafe", "staticFieldOffset", "(Ljava/lang/reflect/Field;)J"),
    ("sun/misc/Unsafe", "storeFence", "()V"),
    ("sun/misc/Unsafe", "throwException", "(Ljava/lang/Throwable;)V"),
    ("sun/misc/Unsafe", "unpark", "(Ljava/lang/Object;)V"),
];

/// Lane 4 wave 4, 2026-09-12: `java/nio/CharBuffer`, and the blocker was gone.
///
/// The seventeen §1.4 shadows on `java/nio/CharBuffer` whose image method
/// declares or inherits `Code`. Ten declare it, seven inherit it from
/// `java/nio/Buffer`. `java/nio/` was admitted as a prefix by wave 1, which
/// retires this class's 34-row `java/nio/ByteBuffer` twin under it -- same
/// accessors, same registrar, same shape -- so there is no prefix decision
/// here, only a table.
///
/// # This family was BACKED OUT once, and the interesting part is why it came back
///
/// §9.2 of the lane page lost `java/nio/CharBuffer` on the first build of wave
/// 1: armed, `subSequence(1, 3)` answered `cd` where the oracle says `bc`,
/// diagnosed as a carrier whose `position`/`limit` the real accessors could not
/// read. Re-measured on 2026-09-12 the defect does not reproduce, and the
/// registrar says why in its own comment: `p62_alloc_char_buffer` and every
/// `wrap`/`subSequence`/`slice` producer in
/// `native-builtins/src/phases_late/charset_buffers.rs` mint
/// `java/nio/HeapCharBuffer` -- the real CONCRETE class -- so the real bodies
/// find real state in real slots. `4ba4f312b` (2026-08-06, "CharBuffer.wrap
/// stamped the abstract class, so subSequence checked nothing") is that fix,
/// and it predates the §9.2 measurement: what §9.2 armed was a five-family
/// scope, and the wrong answer it attributed to this family was not this
/// family's.
///
/// **So the row that reopened this wave is a re-measurement, not a repair.**
/// That is worth being explicit about, because three of this lane's four waves
/// so far have been "fix the carrier, then take the rows" and it would be easy
/// to file this one the same way. Nothing in this wave fixes anything; the
/// fixing was done five weeks earlier by somebody else.
///
/// # `charAt` is carved out, and by the re-tag's own condition
///
/// Eighteen registrations on this class clear the §1.4 bucket test, not
/// seventeen. `charAt(I)C` is the eighteenth and it is NOT here, because
/// [`crate::registry::NativeMethodRegistry::register`] re-tags a retired triple
/// only when `effective_category() == NativeKind::Bridge`, and `charAt` is
/// registered as an `intrinsic` from `native-builtins/src/lib.rs`. A row for it
/// would be INERT BY CONSTRUCTION -- the predicate would answer `true` and
/// nothing would ever ask. The shape
/// [`the_charbuffer_wave_is_seventeen_bridges_without_the_intrinsic`] guards.
///
/// # The funnel
///
/// Seventeen triples, every one of them reached by
/// `apps/probes/L4CharBufferSweep.java` under `--nojit
/// CRATONVM_DISABLE_INTRINSICS=1`, which is what makes the census's
/// `invocations_complete: true` mean what it says. `session()` and
/// `checkSession()` are package-private `java.nio.Buffer` internals no probe
/// can call by name; they are reached 5 and 149 times respectively as callees
/// of the buffer operations above them, which is the only way anything reaches
/// them and is how the JDK reaches them too. Both are no-ops here for a reason
/// the retirement makes real rather than emulated: `Buffer.session()` returns
/// null when `segment` is null, and this VM never writes `segment`.
///
/// # What the retirement does, and the defect it found
///
/// One binary against itself, plus a third carrying the table without the fix
/// the commit before this one makes. `apps/probes/L4CharBufferSweep.java`, 261
/// rows, oracle stable over three captures:
///
/// ```text
///   L4CharBufferSweep, --jdk-only, 261 rows        rows differing
///     A  control (origin/dev bdb02d94e)                  0
///     B  this table, WITHOUT the carrier-side fix        12
///     C  the fix + this table                             0
///     D  the fix, this table un-retired (same binary)     0
/// ```
///
/// Arm B is the finding. `CharBuffer.toString()` is `toString(position(),
/// limit())` in the JDK and this VM held `toString(int, int)` to TWO index
/// conventions -- absolute for a `StringCharBuffer`, relative to the position
/// for everything else -- with its own `toString()` passing `0, limit -
/// position` to cancel the second one out. Retiring `toString()` put a real
/// body on the calling end and the window moved by exactly `position` on
/// twelve rows. Bisected to that single row in one pass with
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW`: seventeen runs, sixteen still at 12 and
/// `toString()Ljava/lang/String;` alone at 0.
///
/// **A native that owns both ends of a convention agrees with itself whatever
/// the convention is.** Three earlier waves in this lane learned that about a
/// carrier's FIELDS; this is the same sentence about a method's ARGUMENTS, and
/// the retirement is what asked for the second opinion.
///
/// Compatible mode is 0 on the control and 0 on the wave. The re-tag is
/// mode-blind and `SyntheticStub` is allowed in compatible mode, so the native
/// still wins there and nothing moves — which is the whole of why a retirement
/// is a `--jdk-only`-only behaviour change.
///
/// **17 refusals, 0 survivors**, every one `synthetic-native-registered` over
/// the seventeen distinct triples. The control reports ONE of them as
/// `native-won` rather than seventeen, which is a fact about what the report
/// records rather than about the other sixteen: the census beside it has all
/// seventeen at `invocations > 0` with `invocations_complete: true`.
///
/// Corpus: `134 passed, 0 failed` on the control and on the wave under
/// `CRATONVM_ARGS=--jdk-only`, compared VECTOR BY VECTOR and not by totals;
/// `SUITE=all` 134/134 and `SUITE=core` 93/93 on the wave. Kind map: seventeen
/// rows amended `bridge -> synthetic-stub` in the 25/linux baseline, after
/// which the wave fires 870 flips -- exactly the control's count, so the gate
/// is as red as dev and no redder.
///
/// Measured on **linux/x86_64 against JDK 25**.
static RETIRED_SHADOW_L4_CHARBUFFER_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/nio/CharBuffer",
        "allocate",
        "(I)Ljava/nio/CharBuffer;",
    ),
    ("java/nio/CharBuffer", "array", "()[C"),
    ("java/nio/CharBuffer", "arrayOffset", "()I"),
    ("java/nio/CharBuffer", "capacity", "()I"),
    ("java/nio/CharBuffer", "checkSession", "()V"),
    ("java/nio/CharBuffer", "clear", "()Ljava/nio/CharBuffer;"),
    ("java/nio/CharBuffer", "flip", "()Ljava/nio/CharBuffer;"),
    ("java/nio/CharBuffer", "hasArray", "()Z"),
    ("java/nio/CharBuffer", "hasRemaining", "()Z"),
    ("java/nio/CharBuffer", "limit", "()I"),
    ("java/nio/CharBuffer", "limit", "(I)Ljava/nio/CharBuffer;"),
    ("java/nio/CharBuffer", "position", "()I"),
    (
        "java/nio/CharBuffer",
        "position",
        "(I)Ljava/nio/CharBuffer;",
    ),
    ("java/nio/CharBuffer", "remaining", "()I"),
    ("java/nio/CharBuffer", "rewind", "()Ljava/nio/CharBuffer;"),
    (
        "java/nio/CharBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/CharBuffer", "toString", "()Ljava/lang/String;"),
];

/// Lane 5's SECOND residual wave, 2026-09-11 — 33 triples over the two
/// `Unsafe` spellings, and the two halves earned their rows by different
/// instruments.
///
/// # The 13 `sun/misc/Unsafe` rows: the ADDRESS-form accessors
///
/// `RETIRED_SHADOW_L5R_TRIPLES` retired 67 rows and left 32 out with a reason
/// that was honest and narrow: *precondition 4 is per triple however obvious a
/// sibling looks*. `getByte(J)B`, `getLong(J)J` and `putLong(JJ)V` were in that
/// wave; `getInt(J)I` and its eleven siblings were not, because nothing in the
/// tree had ever called them. The page said the remedy was **a probe edit, not
/// a build**, and this is the edit: `apps/probes/L5SunMiscUnsafe.java` grew
/// seven `addrRow` cases that allocate, round-trip one type through the
/// `(long)` accessor pair, and free.
///
/// ```text
///   dial armed on sun/misc/Unsafe   reached 13   yielded 13   declined_no_bytecode 0
///   per triple                      outcome = bytecode-won on all 13, one row each
///   the workload                    d(base, armed) = 0 over 57 rows
///   vs HotSpot                      d(hs, base) = d(hs, armed) = 2  (the ONE
///                                   `getUnsafe` row, which is the member-filter
///                                   defect and not this wave's)
/// ```
///
/// 13 dispatches for 14 calls is not a miscount: `getByte(J)B` is already
/// retired, so it is no longer registered and the dial cannot reach it.
///
/// # The 20 `jdk/internal/misc/Unsafe` rows: the delegating accessors
///
/// These are §10.4's list, and they are admitted by the rule §4 used for the
/// sixteen already retired: **a Java method that delegates to an `ACC_NATIVE`
/// primitive at the SAME offset.** `javap -p jdk.internal.misc.Unsafe` on the
/// 25 image separates the two populations for you — 68 of the class's methods
/// are `ACC_NATIVE` and can never be retired (contract §1.5), and these 20 are
/// all in the other group, `declared` with `has_code`.
///
/// # What the class-wide dial says, and why it is not a reason to stop
///
/// Arming `jdk/internal/misc/Unsafe` WHOLE takes `UnsafeShadowSweep` from 472
/// rows to 292 and `rc=0` to `rc=1`. That is the measurement §10.4 predicted
/// ("the class-wide arm is where `L4BridgeSweep` goes from 499 rows to zero"),
/// and it is a statement about the CLASS, not about these 20 triples: the same
/// armed run yields 46 distinct triples to bytecode, and the 26 this table does
/// not take are exactly the ones that cannot survive it.
///
/// **Three families are excluded, each for a reason that is not "we did not get
/// to it":**
///
///   * every `*Unaligned` row — `getIntUnaligned`, `putLongUnaligned` and their
///     six siblings do byte-offset arithmetic on the offset they are handed.
///     This VM answers `objectFieldOffset` with a SLOT INDEX, so the JDK's
///     arithmetic lands nowhere. Permanently blocked, not pending.
///   * the sub-word atomics — `compareAndExchange{Byte,Short}`,
///     `compareAndSet{Byte,Short}`, `getAndAdd{Byte,Short}`. Same cause: the
///     JDK emulates these by masking within an enclosing word at a computed
///     byte offset.
///   * the four that report the VM's own numbering — `objectFieldOffset`,
///     `staticFieldOffset`, `staticFieldBase`, `arrayIndexScale` — plus
///     `getUnsafe` and `ensureClassInitialized`. Yielding these hands the rest
///     of the class a number from the other model, which is how one arm turns
///     into 180 changed lines.
///
/// `weakCompareAndSetIntPlain` is absent from this table and its seven siblings
/// are present. That is not an oversight: the sweep never dispatched it, and a
/// row whose only evidence is that its siblings passed is exactly what
/// precondition 4 refuses.
static RETIRED_SHADOW_L5S_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeIntAcquire",
        "(Ljava/lang/Object;JII)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeIntRelease",
        "(Ljava/lang/Object;JII)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeLongAcquire",
        "(Ljava/lang/Object;JJJ)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeLongRelease",
        "(Ljava/lang/Object;JJJ)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeReferenceAcquire",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getIntAcquire",
        "(Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getIntOpaque",
        "(Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getLongAcquire",
        "(Ljava/lang/Object;J)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getReferenceOpaque",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putIntRelease",
        "(Ljava/lang/Object;JI)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putLongRelease",
        "(Ljava/lang/Object;JJ)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetIntAcquire",
        "(Ljava/lang/Object;JII)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetIntRelease",
        "(Ljava/lang/Object;JII)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetLongAcquire",
        "(Ljava/lang/Object;JJJ)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetLongPlain",
        "(Ljava/lang/Object;JJJ)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetLongRelease",
        "(Ljava/lang/Object;JJJ)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetReferenceAcquire",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetReferencePlain",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    ("sun/misc/Unsafe", "getAddress", "(J)J"),
    ("sun/misc/Unsafe", "getChar", "(J)C"),
    ("sun/misc/Unsafe", "getDouble", "(J)D"),
    ("sun/misc/Unsafe", "getFloat", "(J)F"),
    ("sun/misc/Unsafe", "getInt", "(J)I"),
    ("sun/misc/Unsafe", "getShort", "(J)S"),
    ("sun/misc/Unsafe", "putAddress", "(JJ)V"),
    ("sun/misc/Unsafe", "putByte", "(JB)V"),
    ("sun/misc/Unsafe", "putChar", "(JC)V"),
    ("sun/misc/Unsafe", "putDouble", "(JD)V"),
    ("sun/misc/Unsafe", "putFloat", "(JF)V"),
    ("sun/misc/Unsafe", "putInt", "(JI)V"),
    ("sun/misc/Unsafe", "putShort", "(JS)V"),
];

pub fn triple_is_retired_shadow(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    if !RETIRED_SHADOW_PREFIXES
        .iter()
        .any(|p| class_name.starts_with(p))
    {
        return false;
    }
    let key = (class_name, method_name, descriptor);
    // Driven off `RETIRED_SHADOW_TABLES` rather than one `||` arm per table.
    // `any` short-circuits exactly as the chain did and each table is still
    // binary-searched, so this is the same work in the same order of magnitude
    // — what changes is that a table which is not in the const is not
    // consulted, instead of being consulted while the const says otherwise.
    // Both lane 1 and lane 7 shipped a table missing from that const; the
    // arrangement below cannot reproduce it.
    //
    // The const's order differs from the chain's it replaces. That is
    // immaterial and `no_triple_is_claimed_by_two_tables` is why: no triple is
    // in two tables, so no input can reach a second table that would answer
    // differently, and which table answers first is unobservable.
    RETIRED_SHADOW_TABLES
        .iter()
        .any(|table| table.binary_search(&key).is_ok())
        // `CRATONVM_UNRETIRE_NATIVE_SHADOW` turns named rows back off, so a
        // wave can be bisected in RUNS rather than one build per hypothesis.
        // Unset -- every shipping configuration -- this is `false` without
        // consulting anything; see [`crate::unretire`], whose
        // `the_default_is_inert_across_every_retired_row` asserts it against
        // every row of every table above.
        && !crate::unretire::is_excluded(class_name, method_name, descriptor)
}

/// Every retired-shadow table, in one slice, so a gate can walk the whole
/// population instead of naming one wave.
///
/// Added 2026-09-10 for
/// `registry::tests::real_layout_bridge_keeps_are_not_retired_shadows`. That
/// test needs the ROWS, not the predicate: it asks, of each retired triple,
/// whether real-JDK mode would have KEPT it had the `Bridge` -> `SyntheticStub`
/// re-tag in [`crate::registry::NativeMethodRegistry::register`] not run first.
/// A `yes` means the table is silently disarming a `keep_real_*_bridge` arm in
/// a mode the retiring lane never measured — see the "a retirement is
/// mode-blind and a keep arm is not" section on [`RETIRED_SHADOW_L5_TRIPLES`],
/// which is the case that prompted this.
///
/// **Add every new table here.** Forgetting is not caught by the sorted/unique
/// tests, which are per-table; the cost of the omission is that the new wave is
/// simply not asked the question.
pub(crate) const RETIRED_SHADOW_TABLES: &[&[(&str, &str, &str)]] = &[
    RETIRED_SHADOW_TRIPLES,
    RETIRED_SHADOW_STATELESS_TRIPLES,
    RETIRED_SHADOW_PHASE2_TRIPLES,
    RETIRED_SHADOW_L2_TRIPLES,
    RETIRED_SHADOW_PHASE3_TRIPLES,
    RETIRED_SHADOW_L5_TRIPLES,
    // Added here by the 2026-09-11 merge of lane 1's wave 2, NOT by lane 1:
    // this const and lane 1's table were written on branches that never saw
    // each other, so `git merge` resolved both files without a conflict and
    // left the new 325-row table off the list. That is the exact drift
    // `the_tables_const_lists_every_table_the_predicate_consults` exists to
    // catch, and it is what caught it.
    RETIRED_SHADOW_L1_TRIPLES,
    // And again on 2026-09-11, one merge later, for lanes L0 and L3 -- same
    // mechanism, same silence. This const arrived on `dev` while L0's and L3's
    // tables were being written on this branch, so neither file conflicted and
    // the predicate chain above ended up consulting NINE tables against this
    // const's SEVEN. The count assertion is the only thing that noticed.
    //
    // The cost was not cosmetic: this const is what feeds
    // `real_layout_bridge_keeps_are_not_retired_shadows`, so until this line
    // L0's 19 and L3's 24 were never asked whether their `Bridge` ->
    // `SyntheticStub` re-tag disarms a `keep_real_*_bridge` arm in REAL-JDK
    // mode -- the question that took lane 5 from 100 rows to 98.
    RETIRED_SHADOW_L0_TRIPLES,
    RETIRED_SHADOW_L3_TRIPLES,
    // And here by the 2026-09-11 merge of lane 7, for exactly the reason the
    // note above gives -- same shape, second occurrence in two days. Lane 7's
    // table and this const were also written on branches that never saw each
    // other, `git merge` resolved both files, and the omission was again
    // invisible to every per-table test.
    RETIRED_SHADOW_L7_TRIPLES,
    RETIRED_SHADOW_L4_TRIPLES,
    // Lane 1's HashMap (21 triples) and jar/text (29 triples over
    // java/text/Normalizer and java/util/jar/*) waves, added by the 2026-09-11
    // L7 merge and NOT by lane 1 -- the third and fourth occurrence of this
    // exact drift in two days, after lane 1's first table and lane 7's.
    //
    // They arrived as two more `||` arms in the predicate with no entry here,
    // which under the old chain meant "consulted, but invisible to every gate
    // driven off this const". Under the loop the predicate reads this list, so
    // the same omission would have UN-RETIRED both waves instead of merely
    // under-covering them -- a louder failure, and the reason the loop is worth
    // having: the two lists cannot disagree, because there is only one.
    // Lane 6, added under the loop rather than beside a `||` arm -- which is
    // the whole difference the loop makes. This lane hit the old failure mode
    // twice while the chain still existed: once on its own table, once on
    // lane 1's waves 3 and 4, which were consulted by the chain and missing
    // from this const on `dev`. Under the loop neither omission is possible,
    // because forgetting the const row does not under-cover a table -- it
    // un-retires it, loudly, in the lane's own probe run.
    RETIRED_SHADOW_L6_TRIPLES,
    RETIRED_SHADOW_L1_HM_TRIPLES,
    RETIRED_SHADOW_L1_JT_TRIPLES,
    // Lane 5's residual wave, added here in the same 2026-09-11 merge. The
    // note above is why this line exists at all: the loop below reads THIS
    // list, so a table missing from it is a table the predicate does not
    // consult.
    RETIRED_SHADOW_L5R_TRIPLES,
    // 2026-09-11, L1 wave 5. Added HERE and nowhere else, which under the loop
    // above is the whole registration -- there is no chain arm to forget any
    // more. See `RETIRED_SHADOW_L1_ZI_TRIPLES` for why two rows and not forty,
    // and why the table is written but not yet accepted.
    RETIRED_SHADOW_L1_ZI_TRIPLES,
    // Lane 5's SECOND residual wave, 2026-09-11. Same one-line registration as
    // the line above it, and the same consequence for forgetting it: the loop
    // below reads THIS list, so a table that is not named here is a table
    // nothing consults and 33 rows that read as un-retired.
    RETIRED_SHADOW_L5S_TRIPLES,
    // 2026-09-11, L1 wave 6: `java/text/BreakIterator`, all seventeen, with
    // the BREAKITER pin in `vm_exec.rs` removed in the same commit. Neither
    // half is correct alone -- retire without lifting the pin and the pin
    // still serves the factories; lift without retiring and the census still
    // counts seventeen shadows over a family that no longer uses them.
    RETIRED_SHADOW_L1_BI_TRIPLES,
    RETIRED_SHADOW_L1_LP_TRIPLES,
    // Lane 4 wave 2, added WITH the table rather than after a gate caught it --
    // which is the whole point of the loop this const now feeds.
    RETIRED_SHADOW_L4_FFM_TRIPLES,
    // Lane 4 wave 3, the group half of the same prefix. No new prefix:
    // `jdk/internal/foreign/layout/` was admitted by wave 2 and its note
    // there says what this line is the other half of.
    RETIRED_SHADOW_L4_FFM_GROUP_TRIPLES,
    // Lane 4 wave 4. No new prefix either: `java/nio/` was admitted by wave 1,
    // which retires this class's `java/nio/ByteBuffer` twin under it.
    RETIRED_SHADOW_L4_CHARBUFFER_TRIPLES,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_l4_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_L4_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "lane 4's table is binary-searched, so it must be sorted and                  unique: {:?} does not precede {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn the_ffm_wave_is_the_nine_value_layouts_without_varhandle() {
        // The carve-out is the load-bearing claim of this wave, so it is
        // asserted rather than described. Retiring `varHandle` runs the real
        // `ValueLayouts$AbstractValueLayout.varHandle()`, which reaches
        // `Utils.makeSegmentViewVarHandle` and ends in
        // `NoClassDefFoundError: java/lang/invoke/BoundMethodHandle` -- the one
        // method of the thirteen whose real body this VM cannot service. A
        // later edit that "completes" the table by adding the nine missing rows
        // would reintroduce exactly that, silently, in `--jdk-only` only.
        let mut classes = std::collections::BTreeSet::new();
        for (c, m, d) in RETIRED_SHADOW_L4_FFM_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in the lane 4 FFM table and the predicate cannot see it"
            );
            assert!(
                c.starts_with("jdk/internal/foreign/layout/ValueLayouts$Of"),
                "{c} is not one of the nine value-layout carriers this wave measured"
            );
            assert_ne!(
                *m, "varHandle",
                "{c}.{m}{d} is carved out of this wave on purpose -- see the table's doc comment"
            );
        }
        assert_eq!(RETIRED_SHADOW_L4_FFM_TRIPLES.len(), 137);
        classes.extend(RETIRED_SHADOW_L4_FFM_TRIPLES.iter().map(|(c, _, _)| *c));
        assert_eq!(classes.len(), 9, "{classes:?}");
        for c in &classes {
            assert!(
                !triple_is_retired_shadow(c, "varHandle", "()Ljava/lang/invoke/VarHandle;"),
                "{c}.varHandle()Ljava/lang/invoke/VarHandle; must stay a live Bridge"
            );
        }
    }

    /// Wave 3's group half, and its carve-out.
    ///
    /// This test read the other way round until 2026-09-12 -- it asserted the
    /// four group carriers were NOT retired, because wave 2 held them back on
    /// a measured carrier defect. Wave 3 fixed the carrier
    /// (`AbstractGroupLayout.elements` is a `java.util.List` and this VM stored
    /// an array in it) and took them, so the assertion is inverted rather than
    /// deleted: the thing worth guarding is still the same, which is that the
    /// PREFIX is not the decision.
    #[test]
    fn the_ffm_group_wave_is_four_carriers_without_varhandle() {
        let mut classes = std::collections::BTreeSet::new();
        for (c, m, d) in RETIRED_SHADOW_L4_FFM_GROUP_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in the lane 4 group table and the predicate cannot see it"
            );
            assert!(
                c.starts_with("jdk/internal/foreign/layout/") && !c.contains("ValueLayouts"),
                "{c} is not one of the four group carriers this wave measured"
            );
            assert_ne!(
                *m, "varHandle",
                "{c}.{m}{d} is carved out of this wave on purpose -- the real                  `AbstractLayout.varHandle` reaches `Utils.makeSegmentViewVarHandle`                  and ends in `NoClassDefFoundError: java/lang/invoke/BoundMethodHandle`,                  exactly as it does for the nine value layouts"
            );
            classes.insert(*c);
        }
        assert_eq!(RETIRED_SHADOW_L4_FFM_GROUP_TRIPLES.len(), 28);
        assert_eq!(classes.len(), 4, "{classes:?}");
        for c in &classes {
            assert!(
                !triple_is_retired_shadow(
                    c,
                    "varHandle",
                    "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;"
                ),
                "{c}.varHandle(PathElement...) must stay a live Bridge"
            );
        }
    }

    /// Wave 4's seventeen, and the carve-out the RE-TAG's own condition makes.
    ///
    /// Eighteen registrations on `java/nio/CharBuffer` clear the §1.4 bucket
    /// test. The eighteenth is `charAt(I)C`, registered as an `intrinsic`, and
    /// `NativeMethodRegistry::register` re-tags a retired triple only when the
    /// effective category is `Bridge` -- so a row for it would be inert by
    /// construction. That is a different reason from the FFM waves'
    /// `varHandle` carve-out (which is a real body this VM cannot service) and
    /// worth a separate assertion, because it is invisible in the table itself:
    /// a table row costs nothing and does nothing, and only this test says so.
    #[test]
    fn the_charbuffer_wave_is_seventeen_bridges_without_the_intrinsic() {
        for (c, m, d) in RETIRED_SHADOW_L4_CHARBUFFER_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in lane 4's CharBuffer table and the predicate cannot see it"
            );
            assert_eq!(
                *c, "java/nio/CharBuffer",
                "this wave is ONE class; {c} is not it"
            );
            assert_ne!(
                *m, "charAt",
                "charAt(I)C is registered as an intrinsic, and `register` re-tags a \
                 retired triple only when the effective category is Bridge -- a row \
                 here would be inert by construction"
            );
        }
        assert_eq!(RETIRED_SHADOW_L4_CHARBUFFER_TRIPLES.len(), 17);
        assert!(
            !triple_is_retired_shadow("java/nio/CharBuffer", "charAt", "(I)C"),
            "charAt is carved out of this wave on purpose"
        );
    }

    /// The five typed-buffer families BESIDE CharBuffer are not retired.
    ///
    /// They share a registrar and a field layout with it, which is exactly why
    /// this is worth asserting: `java/nio/` is a prefix wave 1 admitted, so a
    /// later wave that widens this table by class name rather than by
    /// measurement would take them silently. None of them has been through a
    /// probe, and `java/nio/CharBuffer` only came back after one.
    ///
    /// `java/nio/Buffer` itself is in the list for a stronger reason: its
    /// `<init>` is registered as a Bridge with 143 invocations in this wave's
    /// own probe run, and it is the constructor EVERY buffer in the VM runs.
    /// Retiring anything on `java/nio/Buffer` is a whole-NIO change, not a
    /// family's, and this wave measured one family.
    #[test]
    fn the_typed_buffer_families_beside_charbuffer_are_not_retired() {
        for c in [
            "java/nio/Buffer",
            "java/nio/IntBuffer",
            "java/nio/LongBuffer",
            "java/nio/FloatBuffer",
            "java/nio/DoubleBuffer",
            "java/nio/ShortBuffer",
            "java/nio/HeapCharBuffer",
            "java/nio/HeapCharBufferR",
            "java/nio/StringCharBuffer",
        ] {
            for (m, d) in [
                ("position", "()I"),
                ("limit", "()I"),
                ("capacity", "()I"),
                ("remaining", "()I"),
                ("hasArray", "()Z"),
                ("toString", "()Ljava/lang/String;"),
                ("toString", "(II)Ljava/lang/String;"),
                ("<init>", "(IIIILjava/lang/foreign/MemorySegment;)V"),
            ] {
                assert!(
                    !triple_is_retired_shadow(c, m, d),
                    "{c}.{m}{d} is retired, and no wave has measured {c}"
                );
            }
        }
    }

    /// The segment, arena and session carriers are NOT retired, and not because
    /// nobody has screened them.
    ///
    /// `the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md` decides
    /// that `cratonvm/internal/foreign/MemorySegmentImpl` is the VM's own
    /// allocation shape, laid out DELIBERATELY unlike
    /// `AbstractMemorySegmentImpl`, whose `length`/`readOnly`/`scope` would
    /// alias the carrier's `ptr`/`size`/`arena`. Retiring one of these runs a
    /// real body over those three slots. That is not a wave awaiting a
    /// measurement; it is a wave that must not be run while the decision
    /// stands, which is why it is asserted here rather than left to a reader.
    #[test]
    fn the_ffm_allocation_shape_carriers_are_never_retired() {
        for (c, m, d) in [
            ("jdk/internal/foreign/ArenaImpl", "close", "()V"),
            ("jdk/internal/foreign/MemorySessionImpl", "close", "()V"),
            (
                "jdk/internal/foreign/AbstractMemorySegmentImpl",
                "byteSize",
                "()J",
            ),
            (
                "jdk/internal/foreign/NativeMemorySegmentImpl",
                "address",
                "()J",
            ),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is retired, and the FFM carrier decision says it must not be"
            );
        }
    }

    #[test]
    fn every_lane_4_entry_is_reachable_through_the_predicate() {
        // A table entry under a prefix `triple_is_retired_shadow` rejects
        // answers `false`, which reads as "not retired" and is INVISIBLE --
        // the failure mode `every_entry_is_reachable_through_the_predicate`
        // exists for. Lane 4 needed two new prefixes to be reachable at all.
        for (c, m, d) in RETIRED_SHADOW_L4_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in the lane 4 table and the predicate cannot see it"
            );
        }
    }

    /// [`RETIRED_SHADOW_TABLES`] must list every table the predicate consults.
    ///
    /// It is a hand-maintained second list of the same tables, which is the
    /// shape that drifts. Nothing catches the omission by behaviour: a wave
    /// left out of the const is simply never asked whether it disarms a
    /// real-JDK keep arm, and the gate that asks
    /// (`registry::tests::real_layout_bridge_keeps_are_not_retired_shadows`)
    /// passes on a smaller population without saying so.
    ///
    /// So count the arms in the predicate's own source instead of trusting the
    /// two lists to stay in step. A source-scanning check is a parser and is
    /// wrong in both directions — here it can only be wrong if someone renames
    /// the tables or consults one without a `binary_search`, and either is a
    /// change to this file that should be reading this comment.
    #[test]
    fn the_predicate_names_no_table_directly() {
        // The inverse of the test this replaces, and the reason it can be an
        // inverse: `triple_is_retired_shadow` now iterates
        // `RETIRED_SHADOW_TABLES`, so "the const lists every table the
        // predicate consults" is true by construction and no longer needs
        // asserting. What DOES need asserting is that nobody reintroduces a
        // hand-written arm beside the loop — one `|| RETIRED_SHADOW_LX_TRIPLES
        // .binary_search(..)` would consult a table the const does not list,
        // and every gate driven off the const would quietly stop covering it.
        let src = include_str!("retired_shadow.rs");
        let body = src
            .split("pub fn triple_is_retired_shadow(")
            .nth(1)
            .expect("the predicate is in this file");
        let body = body.split("\n}\n").next().expect("the predicate has a body");
        let total = body.matches("RETIRED_SHADOW_").count();
        let allowed = body.matches("RETIRED_SHADOW_PREFIXES").count()
            + body.matches("RETIRED_SHADOW_TABLES").count();
        assert_eq!(
            total, allowed,
            "`triple_is_retired_shadow` names a retired-shadow table directly. \
             It must reach every table through `RETIRED_SHADOW_TABLES`, which \
             is what makes the const the single place a new table is \
             registered — and what makes every gate driven off the const cover \
             it. Delete the hand-written arm and add the table to the const."
        );
        assert!(
            body.contains("RETIRED_SHADOW_TABLES"),
            "`triple_is_retired_shadow` no longer consults \
             `RETIRED_SHADOW_TABLES` at all — it would answer `false` for every \
             retired triple, silently un-retiring the whole population."
        );
    }

    #[test]
    fn the_l1_jar_text_table_is_disjoint_from_every_other_table() {
        for key in RETIRED_SHADOW_L1_JT_TRIPLES {
            for (other, name) in [
                (RETIRED_SHADOW_TRIPLES, "RETIRED_SHADOW_TRIPLES"),
                (
                    RETIRED_SHADOW_STATELESS_TRIPLES,
                    "RETIRED_SHADOW_STATELESS_TRIPLES",
                ),
                (
                    RETIRED_SHADOW_PHASE2_TRIPLES,
                    "RETIRED_SHADOW_PHASE2_TRIPLES",
                ),
                (RETIRED_SHADOW_L2_TRIPLES, "RETIRED_SHADOW_L2_TRIPLES"),
                (
                    RETIRED_SHADOW_PHASE3_TRIPLES,
                    "RETIRED_SHADOW_PHASE3_TRIPLES",
                ),
                (RETIRED_SHADOW_L5_TRIPLES, "RETIRED_SHADOW_L5_TRIPLES"),
                (RETIRED_SHADOW_L1_TRIPLES, "RETIRED_SHADOW_L1_TRIPLES"),
                (RETIRED_SHADOW_L1_HM_TRIPLES, "RETIRED_SHADOW_L1_HM_TRIPLES"),
            ] {
                assert!(
                    other.binary_search(key).is_err(),
                    "{key:?} is in both lane 1 wave 4's table and {name}."
                );
            }
        }
    }

    #[test]
    fn the_l1_jar_text_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_L1_JT_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "lane 1 wave 4's table is binary-searched, so it must be \
                 sorted and unique: {:?} does not precede {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn every_zone_info_file_entry_is_reachable_through_the_predicate() {
        for (c, m, d) in RETIRED_SHADOW_L1_ZI_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in lane 1 wave 5's table but answers false —                  the prefix list does not admit it, so the entry is inert and                  silent."
            );
        }
    }

    // NO per-table sortedness or disjointness test for wave 5, deliberately.
    //
    // `the_every_table_is_sorted_and_unique` and
    // `no_triple_is_claimed_by_two_tables` walk `RETIRED_SHADOW_TABLES`, so a
    // new table is covered by being in the const — the same thing that makes
    // it consulted. A hand-written sibling list is the shape those two exist
    // to replace: this wave was written with one, and it named nine tables
    // and omitted lane 4's, which landed between the writing and the merge.
    // Adding L4 to it would have fixed today and rotted again on the next
    // lane.

    /// The narrow prefix admits ONE class, and the table under it retires the
    /// two rows the bisection measured — not the family, and not the package.
    ///
    /// The six classes named here all read `+0` on the same armed sweep as
    /// `ZoneInfoFile` did, and every one of them read `reached == 0` with it:
    /// the probe never asked them anything, so their green is §7's vacuity
    /// trap and says nothing at all. They are spelled out rather than left
    /// implicit because a future wave reading "+0" off that table without the
    /// engagement column beside it would admit all six.
    #[test]
    fn the_zone_info_file_prefix_retires_only_the_two_measured_rows() {
        assert_eq!(
            RETIRED_SHADOW_L1_ZI_TRIPLES.len(),
            2,
            "wave 5 is `getZoneInfo` and `getZoneInfo0`. A third row needs its              own trial binary, not this table."
        );
        // WAVE 6 NARROWED THIS LIST, and the narrowing is the whole point of
        // the guard rather than an erosion of it. Four of the six were
        // released by a MEASUREMENT, not by an argument:
        // `apps/probes/L1LocaleProviderWorkload` gave them the workload their
        // `reached == 0` was asking for, and `JRELocaleProviderAdapter` (+0,
        // reached=8) and `LocaleData` (+0, reached=21) are retired in
        // `RETIRED_SHADOW_L1_LP_TRIPLES` on that reading plus a trial binary.
        //
        // `java/util/Date` (+0, reached=14) and the rest of
        // `sun/util/calendar/` (+0, reached=411) are candidates with
        // engagement and no table yet -- they stay here, because a candidate
        // is not a verdict.
        //
        // The two that stay for a STRONGER reason than vacuity are
        // `LocaleResources` and `CalendarDataUtility`: they measured +12 each,
        // on 254 and 108 engagements. Those are not unmeasured rows any more;
        // they are measured NO.
        for c in [
            "java/util/Date",
            "sun/util/locale/provider/CalendarDataUtility",
            "sun/util/locale/provider/LocaleResources",
            "sun/util/resources/Bundles",
        ] {
            for t in RETIRED_SHADOW_TABLES.iter() {
                assert!(
                    !t.iter().any(|(tc, _, _)| *tc == c),
                    "{c} is retired, and wave 6 measured it either VACUOUS                      (`reached == 0`, so its `+0` is arithmetic on an empty                      set) or WORSE ARMED (+12). Neither is a licence to                      retire; a candidate needs its own trial binary."
                );
            }
        }
    }

    /// Lane 1's four tables are all in `RETIRED_SHADOW_TABLES`, which since
    /// the 2026-09-11 loop rewrite is the ONLY thing that retires them.
    ///
    /// `triple_is_retired_shadow` used to be a chain of `||` arms and this
    /// const a second, hand-maintained list of the same tables; a table in the
    /// chain and not the const was consulted but invisible to the gates driven
    /// off the const. Three lanes shipped that drift in two days — lane 1's
    /// first table, lane 7's, and then lane 1's waves 3 and 4 — and the guard
    /// on it compared two COUNTS, so it reported `10 == 8` without ever saying
    /// which two were missing.
    ///
    /// The loop removed the second list, which removes the drift. It also
    /// raised the stakes of the remaining omission: a table absent from the
    /// const is now not consulted AT ALL, so forgetting it silently
    /// UN-RETIRES a whole wave rather than merely under-covering it. That is a
    /// better failure — it is a behaviour change a probe can see — but it is
    /// still worth a test that names the table rather than counting.
    #[test]
    fn lane_ones_six_tables_are_all_in_the_tables_const() {
        for (t, name) in [
            (RETIRED_SHADOW_L1_TRIPLES, "RETIRED_SHADOW_L1_TRIPLES"),
            (RETIRED_SHADOW_L1_HM_TRIPLES, "RETIRED_SHADOW_L1_HM_TRIPLES"),
            (RETIRED_SHADOW_L1_JT_TRIPLES, "RETIRED_SHADOW_L1_JT_TRIPLES"),
            (RETIRED_SHADOW_L1_ZI_TRIPLES, "RETIRED_SHADOW_L1_ZI_TRIPLES"),
            (RETIRED_SHADOW_L1_BI_TRIPLES, "RETIRED_SHADOW_L1_BI_TRIPLES"),
            (RETIRED_SHADOW_L1_LP_TRIPLES, "RETIRED_SHADOW_L1_LP_TRIPLES"),
        ] {
            // Compared BY VALUE, not by pointer. `RETIRED_SHADOW_TABLES` is a
            // `const`, so each use site materialises its own array and
            // `ptr::eq` on the slices inside it is not guaranteed to hold
            // (measured 2026-09-11: it does not, for an entry that IS in the
            // list). Two tables can only compare equal if they carry identical
            // rows, and `no_triple_is_claimed_by_two_tables` already forbids
            // that for every non-empty pair.
            assert!(
                RETIRED_SHADOW_TABLES.iter().any(|listed| *listed == t),
                "{name} is consulted by `triple_is_retired_shadow` but is not in                  `RETIRED_SHADOW_TABLES`, so it is never asked whether it                  disarms a real-JDK keep arm."
            );
        }
    }

    #[test]
    fn every_locale_provider_entry_is_reachable_through_the_predicate() {
        for (c, m, d) in RETIRED_SHADOW_L1_LP_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in lane 1 wave 6's locale-provider table and                  the predicate cannot see it -- the narrow prefix for that                  class is missing."
            );
        }
    }

    /// The two MEASURED blockers must not drift into a table by prefix.
    ///
    /// `LocaleResources` and `CalendarDataUtility` read +12 each on the
    /// wave-6 workload, with 254 and 108 door engagements behind the number.
    /// They are the reason this lane spells its `sun/util/` prefixes one
    /// class at a time.
    #[test]
    fn the_two_measured_locale_blockers_are_not_retired() {
        for c in [
            "sun/util/locale/provider/LocaleResources",
            "sun/util/locale/provider/CalendarDataUtility",
        ] {
            assert!(
                !RETIRED_SHADOW_PREFIXES
                    .iter()
                    .any(|p| c.starts_with(p) && *p != c),
                "{c} is admitted by a retired-shadow prefix. It measured +12                  in wave 6 with engagement behind it; admitting the package                  puts a measured blocker one binary search from a future                  table."
            );
        }
    }

    #[test]
    fn every_break_iterator_entry_is_reachable_through_the_predicate() {
        for (c, m, d) in RETIRED_SHADOW_L1_BI_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in lane 1 wave 6's table and the predicate                  cannot see it, so the entry is inert and silent."
            );
        }
    }

    /// The family is retired WHOLE, and the count is the guard on that.
    ///
    /// `java.text.BreakIterator` is abstract: every instance the JDK hands
    /// back is a real subclass whose own bytecode answers all seventeen. A
    /// subset would leave the fabricated carrier reachable from whichever
    /// factory was left behind, which is the half-retirement the lane page
    /// warns about for the HashMap views.
    #[test]
    fn the_break_iterator_family_is_retired_whole() {
        assert_eq!(
            RETIRED_SHADOW_L1_BI_TRIPLES.len(),
            17,
            "`register_p66_break_iterator` registers seventeen triples on              `java/text/BreakIterator`. A table with a different length is              either a half-retirement or a registrar that changed without              this table."
        );
        for (c, _, _) in RETIRED_SHADOW_L1_BI_TRIPLES {
            assert_eq!(
                *c, "java/text/BreakIterator",
                "this table is one class; a `sun/text/*` row belongs to                  whoever measures that class, not to this one."
            );
        }
    }

    /// The two `LocaleResources` readers are the family's FLOOR, not part of
    /// it, and nothing may retire them by widening a prefix.
    ///
    /// With the BREAKITER pin gone there is no fallback behind them: retire
    /// `getBreakIteratorInfo` or `getBreakIteratorResources` and every
    /// BreakIterator factory is back to "Cannot load from null array" at
    /// `BreakIteratorProviderImpl.getBreakInstance pc=21`.
    #[test]
    fn the_two_locale_resources_readers_are_not_retired_by_anything() {
        for m in ["getBreakIteratorInfo", "getBreakIteratorResources"] {
            for d in [
                "(Ljava/lang/String;)Ljava/lang/Object;",
                "(Ljava/lang/String;)[B",
            ] {
                assert!(
                    !triple_is_retired_shadow(
                        "sun/util/locale/provider/LocaleResources",
                        m,
                        d
                    ),
                    "sun/util/locale/provider/LocaleResources.{m}{d} is                      retired. It is the floor `java/text/BreakIterator`'s                      retirement stands on -- see                      `RETIRED_SHADOW_L1_BI_TRIPLES`."
                );
            }
        }
    }

    #[test]
    fn every_l1_jar_text_entry_is_reachable_through_the_predicate() {
        for (c, m, d) in RETIRED_SHADOW_L1_JT_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in lane 1 wave 4's table but answers false — \
                 the prefix list does not admit it, so the entry is inert and \
                 silent."
            );
        }
    }

    /// Wave 4 is six classes, and the two that carry each family's red are
    /// NOT among them.
    ///
    /// `java/util/jar/JarFile` is the whole of `java/util/jar/`'s `+34` and
    /// `java/text/BreakIterator` is the whole of `java/text/`'s `+16`;
    /// `java/text/DateFormat`'s one registration is excluded because its
    /// green was VACUOUS (`reached == 0`), which is §7's trap and not a
    /// result. Adding any of the three back means re-running the bisection,
    /// not editing this list.
    #[test]
    fn wave_four_is_six_classes_and_refuses_jarfile_and_dateformat() {
        for (c, _, _) in RETIRED_SHADOW_L1_JT_TRIPLES {
            assert!(
                matches!(
                    *c,
                    "java/util/jar/JarEntry"
                        | "java/util/jar/Manifest"
                        | "java/util/jar/Attributes"
                        | "java/util/jar/Attributes$Name"
                        | "java/text/Normalizer"
                ),
                "{c} is in wave 4's table and is not one of the six classes \
                 the 2026-09-11 bisection cleared."
            );
        }
        for (c, m, d) in [
            (
                "java/util/jar/JarFile",
                "getManifest",
                "()Ljava/util/jar/Manifest;",
            ),
            (
                "java/util/jar/JarFile",
                "getJarEntry",
                "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
            ),
            // `java/text/BreakIterator` was HERE until wave 6 and is not any
            // more. Wave 4 was right that the family carried its whole
            // regression in one class; wave 6 fixed the cause (the provider
            // chain, then the two CONCRETE registrations that hijacked real
            // receivers) and retired all seventeen with a probe that matches
            // HotSpot on every row. What remains of the guard is the pair
            // below: the two `LocaleResources` readers the family now stands
            // on, which must never be retired by anything.
            (
                "java/text/DateFormat",
                "getInstance",
                "()Ljava/text/DateFormat;",
            ),
            (
                "sun/util/locale/provider/LocaleResources",
                "getBreakIteratorInfo",
                "(Ljava/lang/String;)Ljava/lang/Object;",
            ),
            (
                "sun/util/locale/provider/LocaleResources",
                "getBreakIteratorResources",
                "(Ljava/lang/String;)[B",
            ),
            // `java/text/ParseException` was IN wave 4's table and came out
            // on its own measurement. Armed alone on
            // `apps/probes/L1JarTextSweep.java` it is `+0` — it repairs
            // nothing — and two of its rows trade one wrong answer for
            // another: `e.setStackTrace(new StackTraceElement[0]);
            // e.printStackTrace(w)` prints one header line on HotSpot and
            // the FULL seven-frame trace when the class yields, because this
            // VM's `Throwable` model does not read back a `stackTrace` array
            // that bytecode wrote. Thirteen of its fourteen triples are
            // `Throwable`'s inherited surface, so the defect is `Throwable`'s
            // and not `java/text/`'s; retiring a Throwable SUBCLASS is how it
            // becomes visible. `printStackTrace` is too widely called to
            // change its output for no repair.
            (
                "java/text/ParseException",
                "printStackTrace",
                "(Ljava/io/PrintWriter;)V",
            ),
            (
                "java/text/ParseException",
                "setStackTrace",
                "([Ljava/lang/StackTraceElement;)V",
            ),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is retired. `JarFile` carries its family's whole \n                 regression, `DateFormat`'s arm was vacuous, and the two \n                 `LocaleResources` readers are the floor BreakIterator's own \n                 retirement stands on."
            );
        }
    }

    /// The nine rows wave 4 repairs, by the triple that repairs
    /// each. A later edit that drops one of these fails here rather than in a
    /// probe nobody runs.
    #[test]
    fn the_nine_jar_text_defect_rows_this_wave_repairs_are_retired() {
        for (c, m, d) in [
            (
                "java/util/jar/Attributes",
                "getValue",
                "(Ljava/lang/String;)Ljava/lang/String;",
            ),
            (
                "java/util/jar/Attributes",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            (
                "java/util/jar/Attributes$Name",
                "<init>",
                "(Ljava/lang/String;)V",
            ),
            (
                "java/util/jar/Manifest",
                "<init>",
                "(Ljava/io/InputStream;)V",
            ),
            (
                "java/util/jar/Manifest",
                "<init>",
                "(Ljava/util/jar/Manifest;)V",
            ),
            ("java/util/jar/JarEntry", "<init>", "(Ljava/lang/String;)V"),
            // `JarEntry.getAttributes` is deliberately NOT here.
            // `L1JarTextSweep`'s `E.attributesFromJar` row is a real control
            // defect — a jar's per-entry manifest section reads `null` where
            // HotSpot reads `section-value` — but the class declares no
            // native for it, so nothing in THIS table can repair it. It
            // belongs to whatever fills a `JarEntry` in on the way out of
            // `java/util/jar/JarFile`, which stays `Bridge`. Twelve of the
            // thirteen are this wave's; the thirteenth is §10 item 5's.
        ] {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is not retired, and `apps/probes/\
                 L1JarTextSweep.java` measured the control answering where \
                 the image's own body throws."
            );
        }
    }

    #[test]
    fn the_l1_hashmap_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_L1_HM_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "lane 1 wave 3's table is binary-searched, so it must be \
                 sorted and unique: {:?} does not precede {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn every_l1_hashmap_entry_is_reachable_through_the_predicate() {
        for (c, m, d) in RETIRED_SHADOW_L1_HM_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in lane 1 wave 3's table but answers false — \
                 the prefix list does not admit it, so the entry is inert and \
                 silent."
            );
        }
    }

    #[test]
    fn the_l1_hashmap_table_is_disjoint_from_every_other_table() {
        for key in RETIRED_SHADOW_L1_HM_TRIPLES {
            for (other, name) in [
                (RETIRED_SHADOW_TRIPLES, "RETIRED_SHADOW_TRIPLES"),
                (
                    RETIRED_SHADOW_STATELESS_TRIPLES,
                    "RETIRED_SHADOW_STATELESS_TRIPLES",
                ),
                (
                    RETIRED_SHADOW_PHASE2_TRIPLES,
                    "RETIRED_SHADOW_PHASE2_TRIPLES",
                ),
                (RETIRED_SHADOW_L2_TRIPLES, "RETIRED_SHADOW_L2_TRIPLES"),
                (
                    RETIRED_SHADOW_PHASE3_TRIPLES,
                    "RETIRED_SHADOW_PHASE3_TRIPLES",
                ),
                (RETIRED_SHADOW_L1_TRIPLES, "RETIRED_SHADOW_L1_TRIPLES"),
                // Lane 5 landed on `origin/dev` between this wave's
                // acceptance and its merge. Named here rather than
                // assumed: a patch authored on a stale base has once
                // `git apply`'d clean over this same file and silently
                // deleted a sibling lane's 311-line table, so every
                // table in the chain is checked against every other
                // BY NAME.
                (RETIRED_SHADOW_L5_TRIPLES, "RETIRED_SHADOW_L5_TRIPLES"),
            ] {
                assert!(
                    other.binary_search(key).is_err(),
                    "{key:?} is in both lane 1 wave 3's table and {name}. Two \
                     tables claiming one triple means two measurements claim \
                     it, and only one of them can be the record."
                );
            }
        }
    }

    /// Wave 3 is `java/util/HashMap` and its OWN view and iterator classes,
    /// and nothing that merely looks like them.
    ///
    /// `java/util/LinkedHashMap` is a `HashMap` SUBCLASS and its views are
    /// named `LinkedKeySet`/`LinkedEntrySet`, so a prefix-shaped edit to this
    /// table would swallow it — and it must not, because its entries live in
    /// `lhm_overlay()`, a side table the real bodies cannot read. Retiring it
    /// on this table's coat-tails would hand real bytecode an empty map. Same
    /// argument for `Hashtable`, whose views the JDK wraps in a
    /// `Collections$Synchronized*`.
    #[test]
    fn the_l1_hashmap_table_is_the_hashmap_family_and_only_it() {
        for (c, _, _) in RETIRED_SHADOW_L1_HM_TRIPLES {
            assert!(
                *c == "java/util/HashMap" || c.starts_with("java/util/HashMap$"),
                "{c} is in wave 3's table but is not `java/util/HashMap` or \
                 one of its nested classes. The wave is one family and its \
                 acceptance measured that family."
            );
        }
        for (c, m, d) in [
            (
                "java/util/LinkedHashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            (
                "java/util/LinkedHashMap$LinkedEntrySet",
                "iterator",
                "()Ljava/util/Iterator;",
            ),
            ("java/util/LinkedHashMap$LinkedKeySet", "size", "()I"),
            (
                "java/util/Hashtable",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            (
                "java/util/Hashtable$EntrySet",
                "iterator",
                "()Ljava/util/Iterator;",
            ),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is retired, and wave 3 did not measure it. \
                 `LinkedHashMap` keeps its entries in `lhm_overlay()` and \
                 `Hashtable` was measured separately; both are §10's own \
                 changes, not this table's."
            );
        }
    }

    /// What wave 3 measured and then REFUSED, with the failure each refusal
    /// is holding back.
    ///
    /// The first trial binary retired all 98 and the probe tree read **12
    /// worse, 1 better**. Two causes, both of them a family boundary this
    /// table cannot cross alone:
    ///
    /// * **the three iterator carriers.** `key_itr_carrier_for` mints
    ///   `java/util/HashMap$KeyIterator` for EVERY receiver that is not
    ///   LinkedHashMap-shaped — including `java/util/HashSet`'s and
    ///   `java/util/Hashtable`'s views, whose producers are lane T's and the
    ///   Hashtable family's. Retire the carrier's natives and the live
    ///   producer keeps minting it, so real `HashMap$HashIterator.nextNode`
    ///   runs on an object no bytecode built:
    ///   `NullPointerException: Cannot read field "modCount" because
    ///   "this.this$0" is null`, which killed `MethodRefDoorProbe` on its
    ///   HashSet row and truncated four more probes behind it. The cluster
    ///   note on `register_set_view_carrier_natives` predicted exactly this;
    ///   wave 3 is its first measurement.
    /// * **the three view classes and the three accessors that produce
    ///   them.** They are one unit with the iterators: retire
    ///   `HashMap.entrySet()` and real bytecode mints a real
    ///   `HashMap$EntrySet`, whose surviving natives then find no backing —
    ///   the very defect §3's wave-3 trace is about, moved one class along.
    /// * **eight methods `java/util/LinkedHashMap` inherits.** LinkedHashMap
    ///   is a `HashMap` SUBCLASS and registers its own native for 33 of these
    ///   methods but NOT for these eight, so retiring them routes a
    ///   LinkedHashMap receiver into real `HashMap` bytecode over a table its
    ///   entries are not in — they are in `lhm_overlay()`.
    ///   `LinkedSequencedShadowSweep` read `{b=22, c=33, a=2}` -> `{a=2}`.
    ///
    /// This test is the record. Deleting a row from it means claiming the
    /// blocker is gone, which is a measurement, not an edit.
    #[test]
    fn wave_three_refused_the_iterators_the_views_and_lhm_s_inherited_eight() {
        for (c, m, d) in [
            // the iterator carriers — shared with HashSet and Hashtable
            ("java/util/HashMap$KeyIterator", "next", "()Ljava/lang/Object;"),
            ("java/util/HashMap$ValueIterator", "next", "()Ljava/lang/Object;"),
            (
                "java/util/HashMap$EntryIterator",
                "next",
                "()Ljava/lang/Object;",
            ),
            // the view classes and the accessors that produce them
            ("java/util/HashMap", "entrySet", "()Ljava/util/Set;"),
            ("java/util/HashMap", "keySet", "()Ljava/util/Set;"),
            ("java/util/HashMap", "values", "()Ljava/util/Collection;"),
            (
                "java/util/HashMap$EntrySet",
                "toArray",
                "()[Ljava/lang/Object;",
            ),
            (
                "java/util/HashMap$EntrySet",
                "iterator",
                "()Ljava/util/Iterator;",
            ),
            ("java/util/HashMap$KeySet", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/HashMap$Values", "iterator", "()Ljava/util/Iterator;"),
            (
                "java/util/HashMap$Node",
                "setValue",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            // the eight LinkedHashMap inherits
            (
                "java/util/HashMap",
                "merge",
                "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            ),
            (
                "java/util/HashMap",
                "compute",
                "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            ),
            (
                "java/util/HashMap",
                "computeIfPresent",
                "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            ),
            (
                "java/util/HashMap",
                "replaceAll",
                "(Ljava/util/function/BiFunction;)V",
            ),
            ("java/util/HashMap", "equals", "(Ljava/lang/Object;)Z"),
            ("java/util/HashMap", "hashCode", "()I"),
            (
                "java/util/HashMap",
                "readObject",
                "(Ljava/io/ObjectInputStream;)V",
            ),
            (
                "java/util/HashMap",
                "writeObject",
                "(Ljava/io/ObjectOutputStream;)V",
            ),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is retired, and wave 3 measured that retiring it                  makes the probe tree WORSE. See this test's doc comment for                  which of the three blockers it belongs to."
            );
        }
    }

    #[test]
    fn the_l2_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_L2_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "lane 2's table is binary-searched, so it must be sorted and \
                 unique: {:?} does not precede {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn every_l2_entry_is_reachable_through_the_predicate() {
        for (c, m, d) in RETIRED_SHADOW_L2_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is in lane 2's table but answers false — the \
                 prefix list does not admit it, so the entry is inert and \
                 silent."
            );
        }
    }
    /// Lane 2 retires two families and nothing either side of them.
    ///
    /// The prefix list now admits the whole of `java/lang/` and `java/math/`,
    /// which is a much wider door than the table's three classes. This is the
    /// guard that says widening the door changed no answer — the same job
    /// `the_held_collection_families_are_not_retired` does for `java/util/`.
    #[test]
    fn the_l2_table_holds_only_what_lane_2_measured() {
        const RETIRED_CLASSES: &[&str] = &[
            // wave 1, 2026-09-10
            "java/lang/Character",
            "java/math/BigInteger",
            // wave 2, 2026-09-10 — the families a corpus screen called clean
            // and nothing else had measured. Each is here because the IMAGE
            // was asked two questions the corpus cannot: is the shadowed
            // method reachable at all (an interface receiver, a private
            // constructor or method, or a signature the image does not
            // declare is not a shadow), and does the real body reach an
            // ACC_NATIVE method this VM does not register. Ten rows failed the
            // first question and are recorded in the lane page rather than
            // retired; none failed the second.
            "java/lang/ExceptionInInitializerError",
            "java/lang/IllegalThreadStateException",
            "java/lang/NullPointerException",
            "java/lang/Object",
            "java/lang/Package",
            "java/lang/StringUTF16",
            "java/lang/Throwable",
            "java/lang/UnsatisfiedLinkError",
            "java/lang/VirtualMachineError",
            "java/lang/management/ManagementFactory",
            "java/lang/management/MemoryUsage",
        ];
        for (c, m, d) in RETIRED_SHADOW_L2_TRIPLES {
            assert!(
                RETIRED_CLASSES.contains(c),
                "{c}.{m}{d} is outside the two classes lane 2 measured. Take \
                 the per-class corpus screen and the probe-tree A/B before \
                 adding a third."
            );
        }
        // The nine BigInteger triples held back, and WHY each is held. A
        // re-add has to move the blocker first, and the blocker is not in this
        // file: `add`/`subtract`/`multiply` need the dead `math_bignum.rs`
        // registrations gone, the other six need the JIT to stop dropping the
        // helpful-NPE message. Both are measured; see this table's doc comment.
        for m in ["add", "subtract", "multiply"] {
            assert!(
                !triple_is_retired_shadow(
                    "java/math/BigInteger",
                    m,
                    "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"
                ),
                "BigInteger.{m} was retired, but an older math_bignum.rs \
                 Intrinsic still owns the slot after the refusal — the \
                 retirement is INERT and the survivor returns null for a null \
                 argument where the real body throws."
            );
        }
        for m in [
            "remainder",
            "mod",
            "gcd",
            "and",
            "or",
            "xor",
            "divide",
            "modInverse",
        ] {
            assert!(
                !triple_is_retired_shadow(
                    "java/math/BigInteger",
                    m,
                    "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"
                ),
                "BigInteger.{m} was retired. It reaches bytecode correctly \
                 INTERPRETED, and loses its NullPointerException message once \
                 the body is JIT-compiled."
            );
        }
        for (m, d) in [
            (
                "modPow",
                "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
            ),
            ("<init>", "([B)V"),
            ("<init>", "(I[B)V"),
        ] {
            assert!(
                !triple_is_retired_shadow("java/math/BigInteger", m, d),
                "BigInteger.{m}{d} was retired. Wave 1's rule is structural: a \
                 row whose real body can dereference a null reference ARGUMENT \
                 is exposed to the JIT's dropped NullPointerException message, \
                 and which rows show it varies run to run."
            );
        }

        // Wave 1's rule, asserted over the TABLE rather than over a list of
        // names, so a row added later has to satisfy it too. Only the parameter
        // list is examined: `toByteArray()[B` returns an array and takes
        // nothing, and reading the whole descriptor would reject it.
        for (c, m, d) in RETIRED_SHADOW_L2_TRIPLES {
            if *c != "java/math/BigInteger" {
                continue;
            }
            let params = d
                .split_once('(')
                .and_then(|(_, rest)| rest.split_once(')'))
                .map(|(p, _)| p)
                .unwrap_or("");
            assert!(
                !params.contains('L') && !params.contains('['),
                "{c}.{m}{d} takes a reference parameter. Until the JIT carries \
                 the helpful-NPE message into compiled code, such a row \
                 regresses the message it used to get from the native."
            );
        }

        // The ten rows the image says are not shadows. Six are `<init>` on an
        // INTERFACE, which declares no constructor at all; the rest are a
        // private constructor, a private method, and two signatures the image
        // does not declare. Retiring any of them trades a shadow for a
        // `NoSuchMethodError` — the `Logger.log` eighth-overload shape — so
        // they are held here as well as filtered by the funnel.
        for (c, m, d) in [
            ("java/lang/management/ClassLoadingMXBean", "<init>", "()V"),
            ("java/lang/management/CompilationMXBean", "<init>", "()V"),
            (
                "java/lang/management/GarbageCollectorMXBean",
                "<init>",
                "()V",
            ),
            ("java/lang/management/MemoryMXBean", "<init>", "()V"),
            (
                "java/lang/management/PlatformLoggingMXBean",
                "<init>",
                "()V",
            ),
            ("java/lang/management/RuntimeMXBean", "<init>", "()V"),
            ("java/lang/management/ManagementFactory", "<init>", "()V"),
            (
                "java/lang/management/ManagementFactory",
                "loadNativeLib",
                "()V",
            ),
            ("java/lang/management/MemoryUsage", "<init>", "()V"),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} was retired, and the image declares no such \
                 dispatchable method. That is a NoSuchMethodError, not a \
                 retirement."
            );
        }

        // ...and the two that look identical to the funnel and are NOT the
        // same thing. `Package.equals` resolves to `Object.equals` and
        // `ExceptionInInitializerError.initCause` to `Throwable.initCause`,
        // both concrete. A constructor is never inherited; an ordinary method
        // is, so only the `<init>` rows above are phantoms.
        for (c, m, d) in [
            ("java/lang/Package", "equals", "(Ljava/lang/Object;)Z"),
            (
                "java/lang/ExceptionInInitializerError",
                "initCause",
                "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
            ),
        ] {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is an inherited CONCRETE method, so it is a real                  bucket-B shadow and wave 2 retired it. If it is being held                  again, say which measurement changed."
            );
        }

        // The lane's three named blockers, each with a measurement behind it.
        // `StringBuilder` is the JIT intrinsic door (2026-08-28, N2);
        // `System.getProperty` is the property-store inversion; `System$1` is
        // the hidden-class `defineClass0` failure found on 2026-09-10.
        for (c, m, d) in [
            ("java/lang/StringBuilder", "append", "(I)Ljava/lang/StringBuilder;"),
            ("java/lang/AbstractStringBuilder", "charAt", "(I)C"),
            ("java/lang/System", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("java/lang/System$1", "defineClass", "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;"),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} was retired, and it is one of lane 2's recorded \
                 blockers. Read the lane page before moving it."
            );
        }
    }

    /// The stateless table is binary-searched too, so ordering is correctness
    /// there for the identical reason.
    #[test]
    fn the_stateless_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_STATELESS_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "out of order or duplicated: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
    }

    /// Same guard as its sibling: a stateless entry outside every prefix in
    /// [`RETIRED_SHADOW_PREFIXES`] answers `false`, which reads as "not
    /// retired" and is invisible in a workload.
    #[test]
    fn every_stateless_entry_is_reachable() {
        for (c, m, d) in RETIRED_SHADOW_STATELESS_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable entry: {c}.{m}{d}"
            );
        }
    }

    /// The two tables must not both claim a triple. A duplicate is harmless to
    /// the predicate and NOT harmless to the record: two waves would each
    /// report having retired it, and the count in either doc would be wrong.
    #[test]
    fn the_two_tables_are_disjoint() {
        for t in RETIRED_SHADOW_STATELESS_TRIPLES {
            assert!(
                RETIRED_SHADOW_TRIPLES.binary_search(t).is_err(),
                "{t:?} is in both tables"
            );
        }
        // Three tables now. A triple in two of them is not a doubled
        // retirement -- the predicate ORs -- but it IS two provenances for one
        // decision, and the next reader cannot tell which measurement backs it.
        for t in RETIRED_SHADOW_PHASE2_TRIPLES {
            assert!(
                RETIRED_SHADOW_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_STATELESS_TRIPLES.binary_search(t).is_err(),
                "{t:?} is in the phase-2 table and an earlier one"
            );
        }
        for t in RETIRED_SHADOW_PHASE3_TRIPLES {
            assert!(
                RETIRED_SHADOW_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_STATELESS_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_PHASE2_TRIPLES.binary_search(t).is_err(),
                "{t:?} is in the phase-3 table and an earlier one"
            );
        }
        for t in RETIRED_SHADOW_L6_TRIPLES {
            assert!(
                RETIRED_SHADOW_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_STATELESS_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_PHASE2_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_PHASE3_TRIPLES.binary_search(t).is_err(),
                "{t:?} is in the L6 table and an earlier one"
            );
        }
    }

    /// Sorted and binary-searched like every sibling. An out-of-order entry
    /// makes the predicate answer `false` for a row that IS present, which
    /// reads as "not retired" and is invisible in a workload — the one failure
    /// mode of this file that no gate downstream can see.
    #[test]
    fn the_l6_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_L6_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "out of order or duplicated: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
    }

    /// Every L6 entry is asked through the REAL predicate, not through the
    /// table it lives in.
    ///
    /// This wave added a prefix (`java/net/`), which is the case where a
    /// missing one would be caught — but it is also the case where a
    /// MIS-SPELLED one would not be, because a table whose every row starts
    /// with `java/net/` and a prefix list containing `java/nett/` produce a
    /// predicate that answers `false` for all 114 rows and a build that
    /// compiles and passes every other test in this file.
    #[test]
    fn every_l6_entry_is_reachable() {
        for (c, m, d) in RETIRED_SHADOW_L6_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable entry: {c}.{m}{d}"
            );
        }
    }

    /// The lane's OTHER eight prefixes retire nothing, and that is deliberate.
    ///
    /// `javax/net/`, `sun/net/`, `java/security/`, `sun/security/`,
    /// `javax/crypto/`, `javax/security/`, `jdk/net/` and `jdk/internal/net/`
    /// were each armed on the whole probe tree and each moved at least one
    /// probe AWAY from HotSpot — the TLS stack because it is rustls rather
    /// than a shim, the `HttpsURLConnectionImpl` rows because they forward to
    /// a superclass body in place of a null `delegate`. Their absence from
    /// `RETIRED_SHADOW_PREFIXES` is a measured verdict, so this test states it
    /// as one: if a future wave adds a table under one of them it must delete
    /// this test in the same commit, which is the point.
    #[test]
    fn the_lane_l6_security_and_tls_prefixes_are_not_admitted() {
        for p in [
            "javax/net/",
            "sun/net/",
            "java/security/",
            "sun/security/",
            "javax/crypto/",
            "javax/security/",
            "javax/security/auth/x500/",
            "jdk/net/",
            "jdk/internal/net/",
        ] {
            assert!(
                !RETIRED_SHADOW_PREFIXES.contains(&p),
                "{p} is admitted, but no L6 table retires anything under it"
            );
        }
        // `javax/security/auth/x500/` is in that list for a reason worth
        // stating: it was ADMITTED through the fifth of this wave's six builds
        // and came back out, because `X500Principal` alone breaks
        // `RSslLiveSession`. A future lane that re-admits it has to delete the
        // line above, which is the point.
        //
        // ...and the one prefix that IS admitted carries every row of the
        // table. `java/net/` is wider than the table under it: two classes of
        // the package are retired and six more were measured out by a corpus
        // vector each. A prefix admits a package to the binary search; the
        // table decides what is retired.
        for (c, _, _) in RETIRED_SHADOW_L6_TRIPLES {
            assert!(
                c.starts_with("java/net/"),
                "{c} is in the L6 table but outside the prefix that admits it"
            );
        }
    }

    /// The four rows lane T holds must NOT be here.
    ///
    /// `MalformedURLException` and `UnknownHostException` sit inside this
    /// lane's prefix set and are produced by `lang_misc.rs`'s throwable-family
    /// registrar, which spans seven lanes. Lane-0 §3: while lane T holds a
    /// registrar, no prefix lane may retire any triple that registrar
    /// produces — even one inside its own prefixes. They were candidates and
    /// they were dropped; a later wave that adds them without lane T's
    /// registrar moving is the mistake this test names.
    #[test]
    fn the_throwable_family_rows_are_left_to_lane_t() {
        for (c, _, _) in RETIRED_SHADOW_L6_TRIPLES {
            assert!(
                !c.ends_with("Exception"),
                "{c} is registered by the cross-lane throwable table"
            );
        }
    }

    /// Sorted and binary-searched like both siblings, and for the same reason:
    /// an out-of-order entry makes the predicate answer `false` for a row that
    /// is present, which reads as "not retired" and is invisible in a workload.
    #[test]
    fn the_phase2_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_PHASE2_TRIPLES.windows(2) {
            assert!(w[0] < w[1], "out of order or duplicated: {:?} then {:?}", w[0], w[1]);
        }
    }

    /// An entry outside every prefix in [`RETIRED_SHADOW_PREFIXES`] answers
    /// `false`, which reads as "not retired" and retires nothing.
    #[test]
    fn every_phase2_entry_is_reachable() {
        for (c, m, d) in RETIRED_SHADOW_PHASE2_TRIPLES {
            assert!(triple_is_retired_shadow(c, m, d), "unreachable entry: {c}.{m}{d}");
        }
    }

    /// The same guard for the phase-3 table, and it is 185 rows of it.
    ///
    /// `RETIRED_SHADOW_PREFIXES` already carried `java/util/`, so this wave
    /// needed no prefix edit — which is exactly the situation where a missing
    /// one would go unnoticed. Every entry is asked through the real predicate
    /// rather than the table it lives in.
    #[test]
    fn every_phase3_entry_is_reachable() {
        for (c, m, d) in RETIRED_SHADOW_PHASE3_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable entry: {c}.{m}{d}"
            );
        }
    }

    /// **The N-way disjointness test**, over [`RETIRED_SHADOW_TABLES`] rather
    /// than over a list of sibling names typed out by hand.
    ///
    /// `docs/known-issues/jdk-only-lanes/lane-0-integration-and-gates.md` §4
    /// specifies one of these and assigns it to L0, in the skeleton commit that
    /// was to land before any lane started. That commit never landed — lane 1
    /// says so in `RETIRED_SHADOW_TABLES`' own comment — so each lane wrote its
    /// own instead, and on 2026-09-11 they had drifted into covering different
    /// and partial sets:
    ///
    /// ```text
    ///   L1  no disjointness test at all
    ///   L2  STATELESS, PHASE2
    ///   L5  STATELESS, PHASE2, PHASE3
    ///   L7  STATELESS, PHASE2, PHASE3, L1, L2, L5
    /// ```
    ///
    /// So L1's rows were checked against nothing, and L2xL5, L2xL7, L2xPHASE3
    /// and L5xL1 were checked by neither side. A per-lane test cannot close
    /// this: it names its siblings at the moment it is written, and the next
    /// lane to land is not on the list.
    ///
    /// A collision is harmless to the predicate, which ORs, and NOT harmless to
    /// the record — two waves each report having retired the row and the next
    /// reader cannot tell which measurement backs the decision. This walks
    /// every table against every other, so a new table is covered by being in
    /// the const, which is the same thing that makes it consulted.
    #[test]
    fn no_triple_is_claimed_by_two_tables() {
        use std::collections::BTreeMap;
        let mut first: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
        let mut collisions = Vec::new();
        for (i, table) in RETIRED_SHADOW_TABLES.iter().enumerate() {
            for t in table.iter() {
                if let Some(prev) = first.insert(*t, i) {
                    collisions.push((*t, prev, i));
                }
            }
        }
        assert!(
            collisions.is_empty(),
            "{} triple(s) are claimed by two tables of `RETIRED_SHADOW_TABLES`. \
             Each is retired twice, so two waves each report having retired it \
             and neither measurement is identifiable as the one behind the \
             decision. Delete the later claim, keeping the row in the table \
             whose page measured it: {:?}",
            collisions.len(),
            collisions
        );
    }

    /// Sorted and unique for EVERY table, for the same reason each lane asserts
    /// it of its own: the predicate binary-searches, so an out-of-order entry
    /// answers `false` for a row that is present and retires nothing, silently.
    /// Driven off the const so a new table cannot arrive unchecked.
    #[test]
    fn every_table_is_sorted_and_unique() {
        for (i, table) in RETIRED_SHADOW_TABLES.iter().enumerate() {
            for w in table.windows(2) {
                assert!(
                    w[0] < w[1],
                    "table {i} of `RETIRED_SHADOW_TABLES` is out of order or \
                     has a duplicate: {:?} then {:?}",
                    w[0],
                    w[1]
                );
            }
        }
    }

    /// Every row of every table answers `true` through the REAL predicate.
    ///
    /// Per-lane versions of this exist and each covers its own table; this one
    /// covers the tables nobody wrote one for. A row outside every
    /// [`RETIRED_SHADOW_PREFIXES`] entry answers `false`, which reads as "not
    /// retired" and is invisible in a workload — a missing prefix is the one
    /// edit that silently un-retires a whole wave.
    #[test]
    fn every_row_of_every_table_is_reachable() {
        for (i, table) in RETIRED_SHADOW_TABLES.iter().enumerate() {
            for (c, m, d) in table.iter() {
                assert!(
                    triple_is_retired_shadow(c, m, d),
                    "table {i}: {c}.{m}{d} is in a retired-shadow table and the \
                     predicate says it is not retired — almost always a missing \
                     `RETIRED_SHADOW_PREFIXES` entry"
                );
            }
        }
    }

    /// Sorted, unique and binary-searched like every sibling, and for the same
    /// reason: an out-of-order entry makes the predicate answer `false` for a
    /// row that is present, which reads as "not retired" and is invisible.
    #[test]
    fn the_l7_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_L7_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "out of order or duplicated: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
    }

    /// Lane 5's residual wave: sorted, unique, reachable, and one class.
    ///
    /// Disjointness is NOT asserted here — the N-way test above covers every
    /// table in [`RETIRED_SHADOW_TABLES`], which is strictly more than the
    /// hand-listed loop this test carried before the 2026-09-11 merge. What is
    /// left is what that test cannot know: that this wave is one class, and
    /// that every row reaches the predicate (i.e. `sun/misc/` is still a
    /// prefix — a row under no prefix answers `false` and is invisible).
    #[test]
    fn the_l5r_table_is_sorted_unique_reachable_and_one_class() {
        for w in RETIRED_SHADOW_L5R_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "out of order or duplicated: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
        for &(c, m, d) in RETIRED_SHADOW_L5R_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable through the predicate: {c}.{m}{d} — is `sun/misc/` still in RETIRED_SHADOW_PREFIXES?"
            );
            assert_eq!(
                c, "sun/misc/Unsafe",
                "this wave is one class; {c} does not belong in it"
            );
        }
    }

    /// Sorted, duplicate-free, reachable through the real predicate, and over
    /// exactly the two classes this wave measured.
    ///
    /// The class assertion is not decoration. This is the first lane-5 table to
    /// span TWO classes, and `jdk/internal/misc/` and `sun/misc/` are separate
    /// prefixes — a row added under a third class would sail past the sort
    /// check and answer `false` at the prefix, which reads as "not retired" and
    /// is invisible in a workload.
    #[test]
    fn the_l5s_table_is_sorted_unique_reachable_and_two_classes() {
        for w in RETIRED_SHADOW_L5S_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "out of order or duplicated: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
        for &(c, m, d) in RETIRED_SHADOW_L5S_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable through the predicate: {c}.{m}{d} — are `jdk/internal/misc/` and `sun/misc/` both still in RETIRED_SHADOW_PREFIXES?"
            );
            assert!(
                c == "jdk/internal/misc/Unsafe" || c == "sun/misc/Unsafe",
                "this wave is two classes; {c} does not belong in it"
            );
        }
    }

    /// The three families the second residual wave REFUSED, asserted by name.
    ///
    /// Each is excluded for a reason that does not expire with more probe rows,
    /// which is what separates them from `weakCompareAndSetIntPlain` — that one
    /// is merely undispatched and a future workload may take it. These cannot
    /// be taken by any workload:
    ///
    ///   * `*Unaligned` and the sub-word atomics compute a BYTE offset from the
    ///     offset they are handed, and this VM hands them a slot index;
    ///   * `objectFieldOffset` / `staticFieldOffset` / `staticFieldBase` /
    ///     `arrayIndexScale` ARE that numbering, so yielding one publishes the
    ///     other model's number to everything downstream.
    ///
    /// A future wave that adds one of these will fail here rather than in a
    /// corpus arm three hours later, which is the whole point of naming them.
    #[test]
    fn the_l5s_wave_refuses_the_three_families_that_cannot_be_retired() {
        for m in [
            "getIntUnaligned",
            "getLongUnaligned",
            "getShortUnaligned",
            "getCharUnaligned",
            "putIntUnaligned",
            "putLongUnaligned",
            "putShortUnaligned",
            "putCharUnaligned",
            "compareAndExchangeByte",
            "compareAndExchangeShort",
            "compareAndSetByte",
            "compareAndSetShort",
            "getAndAddByte",
            "getAndAddShort",
            "objectFieldOffset",
            "staticFieldOffset",
            "staticFieldBase",
            "arrayIndexScale",
            "getUnsafe",
        ] {
            for &(c, tm, _) in RETIRED_SHADOW_L5S_TRIPLES {
                assert_ne!(
                    (c, tm),
                    ("jdk/internal/misc/Unsafe", m),
                    "jdk/internal/misc/Unsafe.{m} is excluded by measurement, not by omission — see this test's doc comment"
                );
            }
        }
    }

    /// Three `sun/misc/Unsafe` triples stay OUT of that table, for THREE
    /// different reasons, and the reasons are the point of this test.
    ///
    /// `ensureClassInitialized` and `shouldBeInitialized` were first recorded
    /// here as "declared by NO supported image ... deletions". **That was
    /// wrong, and it was wrong the same way the `getUnsafe` row below was.**
    /// `javap -p sun.misc.Unsafe` declares both, `public`, on **17 and 21**:
    ///
    /// ```text
    ///   17   public boolean shouldBeInitialized(java.lang.Class<?>);
    ///        public void ensureClassInitialized(java.lang.Class<?>);
    ///   21   both, identically
    ///   25   neither — removed
    /// ```
    ///
    /// So they are not deletions: deleting them would take the registration
    /// away from two of the three supported images. They are a VERSION
    /// BOUNDARY — live on 17 and 21, gone on 25 — and this lane's workload runs
    /// on 25, where nothing can dispatch them. Precondition 4 is a dispatch
    /// observed by the citing instrument, and on this image there can be none,
    /// so they stay out until someone measures them on a 17 or 21 run.
    ///
    /// The lesson is the same one `getUnsafe` taught and is worth more than the
    /// two rows: **"declared by no supported image" is a claim about THREE
    /// images.** Checking one and generalising is how both of these got
    /// misfiled, once from a `NoSuchMethodException` and once from a `javap`
    /// run on 25 alone.
    ///
    /// `getUnsafe` is here for a DIFFERENT reason and was very nearly recorded
    /// under the first one. It IS declared, public, on all three images; the
    /// `NoSuchMethodException` that first suggested absence was the JDK's
    /// core-reflection METHOD FILTER hiding it. The native is correct
    /// (`SecurityException` off the boot path, measured against HotSpot); what
    /// diverges is that this VM implements no member filter at all, which is
    /// cross-cutting and has its own page,
    /// `docs/known-issues/jdk-only/core-reflection-has-no-member-filter-20260911.md`.
    /// Retiring the native would not move that, so the row stays out.
    #[test]
    fn the_l5r_wave_excludes_three_triples_for_three_different_reasons() {
        for (m, d) in [
            ("getUnsafe", "()Lsun/misc/Unsafe;"),
            ("ensureClassInitialized", "(Ljava/lang/Class;)V"),
            ("shouldBeInitialized", "(Ljava/lang/Class;)Z"),
        ] {
            assert!(
                !triple_is_retired_shadow("sun/misc/Unsafe", m, d),
                "sun/misc/Unsafe.{m}{d} must stay out of the residual table — see this test's doc comment for which of the three reasons applies"
            );
        }
    }

    /// Sorted and duplicate-free, for the reason every sibling table is: the
    /// predicate binary-searches it, so an out-of-order entry answers `false`
    /// for a row that is present -- which reads as "not retired" and is
    /// invisible in a workload.
    #[test]
    fn the_l5_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_L5_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "out of order or duplicated: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
    }

    /// An entry outside every prefix in [`RETIRED_SHADOW_PREFIXES`] answers
    /// `false`, which reads as "not retired" and retires nothing. This wave
    /// ADDED two prefixes, which is exactly the edit that is easy to forget.
    #[test]
    fn every_l7_entry_is_reachable() {
        for (c, m, d) in RETIRED_SHADOW_L7_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable entry: {c}.{m}{d}"
            );
        }
    }

    /// Every L5 entry is asked through the REAL predicate, not through the
    /// table it lives in. Lane 5 added four prefixes
    /// (`jdk/internal/misc/`, `jdk/internal/vm/`, `java/lang/Thread`,
    /// `sun/misc/`) and inherited `java/util/`, so this is exactly the wave
    /// where a missing one would go unnoticed for the rows under the prefix
    /// that was already there.
    #[test]
    fn every_l5_entry_is_reachable() {
        for (c, m, d) in RETIRED_SHADOW_L5_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable entry: {c}.{m}{d}"
            );
        }
    }

    /// No lane-7 triple may be claimed by another wave. This asks about EVERY
    /// other table, including the two (L1, L5) that landed on `dev` after this
    /// test was written -- a disjointness test that names a fixed list of
    /// siblings stops being a disjointness test the day a sibling is added.
    #[test]
    fn the_l7_table_is_disjoint_from_the_earlier_ones() {
        for t in RETIRED_SHADOW_L7_TRIPLES {
            assert!(
                RETIRED_SHADOW_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_STATELESS_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_PHASE2_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_PHASE3_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_L1_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_L2_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_L5_TRIPLES.binary_search(t).is_err(),
                "{t:?} is in the lane-7 table and an earlier one"
            );
        }
    }

    /// No triple may be claimed by two waves. Harmless to the predicate, which
    /// ORs; NOT harmless to the record, because two waves would each report
    /// having retired it and the next reader cannot tell which measurement
    /// backs the decision.
    #[test]
    fn the_l5_table_is_disjoint_from_the_earlier_four() {
        for t in RETIRED_SHADOW_L5_TRIPLES {
            assert!(
                RETIRED_SHADOW_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_STATELESS_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_PHASE2_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_PHASE3_TRIPLES.binary_search(t).is_err(),
                "{t:?} is in the L5 table and an earlier one"
            );
        }
    }

    /// The two prefixes lane 7 added are a licence for exactly two rows.
    ///
    /// `java/lang/ClassLoader` is a PREFIX, so it also admits
    /// `java/lang/ClassLoader$ParallelLoaders` and `java/lang/ClassLoaderHelper`
    /// to one extra binary search each, and `java/security/SecureClassLoader`
    /// admits that class whole. None of their other triples is retired, and this
    /// test is what says so — the guard
    /// `the_phase2_wave_retires_only_the_triple_it_measured` provides for
    /// `sun/nio/ch/`.
    #[test]
    fn the_l7_prefixes_retire_only_the_two_measured_rows() {
        for (c, m, d) in [
            (
                "java/lang/ClassLoader",
                "getUnnamedModule",
                "()Ljava/lang/Module;",
            ),
            (
                "java/lang/ClassLoader",
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
            ),
            (
                "java/lang/ClassLoader",
                "getParent",
                "()Ljava/lang/ClassLoader;",
            ),
            ("java/lang/ClassLoader", "<init>", "(Ljava/lang/ClassLoader;)V"),
            (
                "java/lang/ClassLoader",
                "getSystemClassLoader",
                "()Ljava/lang/ClassLoader;",
            ),
            (
                "java/lang/ClassLoaderHelper",
                "mapAlternativeName",
                "(Ljava/io/File;)Ljava/io/File;",
            ),
            (
                "java/security/SecureClassLoader",
                "getPermissions",
                "(Ljava/security/CodeSource;)Ljava/security/PermissionCollection;",
            ),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} was retired by a prefix, not by a measurement"
            );
        }
    }

    /// Both lane-7 rows are retired AS A PAIR, and the pair is the fix.
    ///
    /// Retiring only `SecureClassLoader.<clinit>` would run its real bytecode
    /// into the constant-`true` native and register nothing; retiring only
    /// `registerAsParallelCapable` would leave the `<clinit>` a no-op, so
    /// nothing would call it. Either half alone leaves
    /// `BuiltinClassLoader.<clinit>` throwing `InternalError` — the same "all
    /// four or none" shape as the `LogRecord` source pair above.
    #[test]
    fn the_l7_pair_is_retired_together() {
        assert!(triple_is_retired_shadow(
            "java/lang/ClassLoader",
            "registerAsParallelCapable",
            "()Z"
        ));
        assert!(triple_is_retired_shadow(
            "java/security/SecureClassLoader",
            "<clinit>",
            "()V"
        ));
    }

    /// The four prefixes lane 5 added are an EARLY-OUT, not a licence.
    ///
    /// This is the gate on scope creep for this lane, and it is written as a
    /// list of rows the lane measured and DECLINED rather than as a count.
    /// Each one is a live `Bridge` on a receiver whose prefix is now admitted
    /// to the binary search, so "not in the table" has to be checkable rather
    /// than inferred from absence.
    ///
    /// The sub-word atomics are here because their refusal is structural: this
    /// VM's `objectFieldOffset` answers a SLOT INDEX, and the JDK's Java-level
    /// implementation of these methods computes `offset & ~3` and
    /// `(offset & 3) << 3` over what it believes is a byte offset. Retiring one
    /// does not produce a wrong answer at the margin; it names a different
    /// field.
    #[test]
    fn the_l5_prefixes_retire_nothing_on_their_own() {
        for (c, m, d) in [
            // The sub-word CAS layer -- see this table's doc comment.
            ("jdk/internal/misc/Unsafe", "compareAndSetByte", "(Ljava/lang/Object;JBB)Z"),
            ("jdk/internal/misc/Unsafe", "compareAndSetShort", "(Ljava/lang/Object;JSS)Z"),
            ("jdk/internal/misc/Unsafe", "compareAndExchangeByte", "(Ljava/lang/Object;JBB)B"),
            ("jdk/internal/misc/Unsafe", "compareAndExchangeShort", "(Ljava/lang/Object;JSS)S"),
            ("jdk/internal/misc/Unsafe", "getAndAddByte", "(Ljava/lang/Object;JB)B"),
            ("jdk/internal/misc/Unsafe", "getAndAddShort", "(Ljava/lang/Object;JS)S"),
            // The unaligned family: a slot index has no bytes to address.
            ("jdk/internal/misc/Unsafe", "getIntUnaligned", "(Ljava/lang/Object;J)I"),
            ("jdk/internal/misc/Unsafe", "getLongUnaligned", "(Ljava/lang/Object;J)J"),
            ("jdk/internal/misc/Unsafe", "getShortUnaligned", "(Ljava/lang/Object;J)S"),
            ("jdk/internal/misc/Unsafe", "getCharUnaligned", "(Ljava/lang/Object;J)C"),
            ("jdk/internal/misc/Unsafe", "putIntUnaligned", "(Ljava/lang/Object;JI)V"),
            ("jdk/internal/misc/Unsafe", "putLongUnaligned", "(Ljava/lang/Object;JJ)V"),
            ("jdk/internal/misc/Unsafe", "putShortUnaligned", "(Ljava/lang/Object;JS)V"),
            ("jdk/internal/misc/Unsafe", "putCharUnaligned", "(Ljava/lang/Object;JC)V"),
        ] {
            assert!(!triple_is_retired_shadow(c, m, d), "wrongly retired: {c}.{m}{d}");
        }
    }

    /// The `sun/nio/ch/` prefix admits a package the 2026-08-19 sweep scored
    /// 34/36 and called unretirable. That verdict stands for the PACKAGE; this
    /// wave retires one triple inside it, measured alone. The guard is that the
    /// prefix must not become a licence for the rest: every sibling below was a
    /// `native-won` shadow in the same census and none of them was measured.
    #[test]
    fn the_phase2_wave_retires_only_the_triple_it_measured() {
        assert!(triple_is_retired_shadow(
            "sun/nio/ch/FileChannelImpl",
            "truncate",
            "(J)Ljava/nio/channels/FileChannel;"
        ));
        // `open` is NOT retired, and the doc comment above records why the
        // first draft of this wave retired it and moved nothing.
        assert!(!triple_is_retired_shadow(
            "sun/nio/ch/FileChannelImpl",
            "open",
            "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)\
Ljava/nio/channels/FileChannel;"
        ));
        for (c, m, d) in [
            ("sun/nio/ch/SelectionKeyImpl", "cancel", "()V"),
            ("sun/nio/ch/SelectionKeyImpl", "isValid", "()Z"),
            ("sun/nio/ch/SocketChannelImpl", "close", "()V"),
            ("sun/nio/ch/EPollSelectorImpl", "select", "()I"),
            ("sun/nio/ch/NativeThread", "current", "()J"),
            ("sun/nio/ch/FileLockImpl", "release", "()V"),
            // ArenaImpl: measured, and REJECTED by precondition 2. Listed here
            // rather than merely omitted, so that "not in the table" does not
            // read as "not yet looked at".
            ("jdk/internal/foreign/ArenaImpl", "allocate", "(J)Ljava/lang/foreign/MemorySegment;"),
            ("jdk/internal/foreign/ArenaImpl", "close", "()V"),
        ] {
            assert!(!triple_is_retired_shadow(c, m, d), "wrongly retired: {c}.{m}{d}");
        }
    }

    /// The prefixes the table is allowed to use, and nothing else.
    ///
    /// This is the gate on scope creep. Five of the six were keyed to the
    /// 102-vector ARM on 2026-08-19 rather than to the 36-vector screen,
    /// because the screen passed `java/lang/ref/` and `sun/nio/fs/` and the arm
    /// failed them on `RClassUnloadSweep{,Gen}` and `RFileTimes`.
    ///
    /// **`sun/nio/fs/` is the sixth, added 2026-08-20 (H2-1), and it is the one
    /// entry here NOT backed by an arm run.** The `RFileTimes` diff named its
    /// own cause — a Unix-epoch millis value in a field the real class reads as
    /// a Windows FILETIME — and that cause is fixed in
    /// `native-builtins/src/phases_late/nio_file.rs`. Nothing has been built or
    /// run since. Treat the eight entries as PREDICTED-retirable until an arm
    /// says otherwise; `docs/known-issues/jdk-only/H2-1-*` names the exact
    /// revert.
    ///
    /// `java/lang/ref/` is still absent, and that is a measured decision rather
    /// than an omission — see the header.
    ///
    /// `java/nio/file/` and `java/math/` each cost a vector on the screen alone
    /// and `jdk/internal/access/` cost 14, and nothing in the code stops a later
    /// entry under any of them being appended to a table whose prefix list
    /// already admits it. `java/util/` admits the whole package tree, so the
    /// assertion is per-ENTRY, not per-prefix.
    #[test]
    fn the_stateless_table_stays_inside_the_measured_prefixes() {
        const ADJUDICATED: &[&str] = &[
            "java/lang/module/",
            "java/text/",
            "java/util/concurrent/atomic/",
            "java/util/concurrent/locks/",
            "java/util/stream/",
            "sun/nio/fs/",
        ];
        for (c, m, d) in RETIRED_SHADOW_STATELESS_TRIPLES {
            assert!(
                ADJUDICATED.iter().any(|p| c.starts_with(p)),
                "{c}.{m}{d} is outside the prefixes adjudicated for this table. \
                 Arm CRATONVM_ENFORCE_NATIVE_SHADOW on its prefix, run \
                 regression-suite/run.sh with CRATONVM_ARGS=--jdk-only, and \
                 record the number before adding it — the 36-vector screen \
                 passed two prefixes the arm rejected."
            );
        }
    }

    /// A vacuity floor for the stateless wave, and a shape check: the atomics
    /// are its largest family and the reason the wave is worth its size.
    #[test]
    fn the_stateless_table_is_not_empty() {
        assert!(
            RETIRED_SHADOW_STATELESS_TRIPLES.len() >= 235,
            "expected 235 — the 2026-08-19 wave's 227 plus the eight \
             sun/nio/fs/WindowsFileAttributes triples added 2026-08-20 (H2-1), \
             which is the 2026-08-19 census's nine less the held `fileKey` — \
             got {}",
            RETIRED_SHADOW_STATELESS_TRIPLES.len()
        );
        assert!(triple_is_retired_shadow(
            "java/util/concurrent/atomic/AtomicInteger",
            "incrementAndGet",
            "()I"
        ));
    }

    /// The prefix list is a discriminator, not a definition: widening it must
    /// not retire anything the tables do not name.
    #[test]
    fn a_prefix_alone_retires_nothing() {
        assert!(!triple_is_retired_shadow(
            "java/text/SimpleDateFormat",
            "format",
            "(Ljava/util/Date;)Ljava/lang/String;"
        ));
        assert!(!triple_is_retired_shadow(
            "java/lang/module/ModuleDescriptor",
            "notAMethod",
            "()V"
        ));
        // HELD by the arm, and the prefix list does not admit it — belt and
        // braces, because a widening of that list must not silently re-retire
        // what RClassUnloadSweep rejected.
        assert!(!triple_is_retired_shadow(
            "java/lang/ref/Reference",
            "clear",
            "()V"
        ));
        // `sun/nio/fs/` IS in the prefix list as of 2026-08-20, so this arm now
        // has to earn its answer from the table rather than from the prefix.
        assert!(!triple_is_retired_shadow(
            "sun/nio/fs/WindowsFileAttributes",
            "notAMethod",
            "()V"
        ));
        assert!(!triple_is_retired_shadow(
            "sun/nio/fs/WindowsPath",
            "toString",
            "()Ljava/lang/String;"
        ));
    }

    /// Binary-searched like every sibling, so ordering is correctness.
    #[test]
    fn the_l0_table_is_sorted_and_unique() {
        // Refilled by wave 2, so `windows(2)` is real again and the
        // emptiness assertion that stood guard over the placeholder is
        // gone. 19 entries, so an out-of-order pair is reachable by this
        // loop rather than hypothetical.
        assert_eq!(
            RETIRED_SHADOW_L0_TRIPLES.len(),
            19,
            "wave 2 landed 19; the lane page's 7.2 carries the same count"
        );
        for w in RETIRED_SHADOW_L0_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "out of order or duplicated: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
    }

    /// An entry outside every prefix answers `false`, which reads as "not
    /// retired" and is invisible in a workload. L0 added `java/lang/Class` and
    /// `java/lang/Module` for exactly these rows.
    #[test]
    fn every_l0_entry_is_reachable() {
        for (c, m, d) in RETIRED_SHADOW_L0_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable entry: {c}.{m}{d}"
            );
        }
    }

    /// The 10 triples the corpus rejected and §7.2 attributed by dispatch.
    ///
    /// Each is consulted by at least one of `RClassUnloadSweep`,
    /// `RClassUnloadSweepGen`, `RJdkModule`, `RLoaderIdentity` or
    /// `RServiceLoaderDoubleSource`, every one of which failed with the 29-row
    /// table in and passed 5/0 on the control binary run concurrently through
    /// the same harness. They are withdrawn **as touched, not as convicted** --
    /// attribution by dispatch over-collects on purpose -- so re-adding one is
    /// legitimate, on a bisection that names it. It just cannot happen by
    /// hand, silently, which is what this test buys.
    #[test]
    fn the_l0_attributed_triples_are_not_retired() {
        const ATTRIBUTED: &[(&str, &str, &str)] = &[
            ("java/lang/Class", "desiredAssertionStatus", "()Z"),
            (
                "java/lang/Class",
                "forName",
                "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
            ),
            (
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;)Ljava/lang/Class;",
            ),
            (
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
            ),
            (
                "java/lang/Class",
                "getConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            ),
            (
                "java/lang/Class",
                "getDeclaredConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            ),
            (
                "java/lang/Class",
                "getMethod",
                "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
            ),
            ("java/lang/Class", "getPackageName", "()Ljava/lang/String;"),
            ("java/lang/Class", "isInterface", "()Z"),
            ("java/lang/Class", "isPrimitive", "()Z"),
        ];
        for (c, m, d) in ATTRIBUTED {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} was attributed to a failing vector; see 7.2"
            );
        }
    }

    /// The wave is exactly three class families, and nothing crept in from a
    /// neighbouring lane. `java/lang/Class` is a PREFIX of
    /// `java/lang/ClassLoader`, which is L7's, and of the two throwable
    /// classes that belong to the cross-cutting-registrar lane -- the first
    /// draft of the lane split got all three wrong, so this is asserted rather
    /// than trusted.
    #[test]
    fn the_l0_wave_stays_inside_lane_zero() {
        for (c, m, d) in RETIRED_SHADOW_L0_TRIPLES {
            let ok = *c == "java/lang/Class"
                || c.starts_with("java/lang/Class$")
                || c.starts_with("java/lang/ClassValue")
                || *c == "java/lang/Module"
                || c.starts_with("java/lang/ModuleLayer")
                || c.starts_with("java/lang/module/");
            assert!(ok, "not lane L0's class: {c}.{m}{d}");
            assert!(
                !c.starts_with("java/lang/ClassLoader")
                    && !c.starts_with("java/lang/ClassNotFound")
                    && !c.starts_with("java/lang/ClassCast"),
                "another lane's class matched L0's prefix: {c}.{m}{d}"
            );
        }
    }

    /// Sorted, for the reason every sibling table is: an out-of-order entry
    /// makes the predicate answer `false` for a row that IS present, which
    /// reads as "not retired" and is invisible in a workload.
    #[test]
    fn the_l3_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_L3_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "out of order or duplicated: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
    }

    /// An entry outside every prefix answers `false` silently. L3 added
    /// `java/lang/reflect/` and `java/lang/invoke/` for exactly these rows and
    /// deliberately did NOT add its other two scope prefixes, which retire
    /// nothing yet.
    #[test]
    fn every_l3_entry_is_reachable() {
        for (c, m, d) in RETIRED_SHADOW_L3_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable entry: {c}.{m}{d}"
            );
        }
    }

    /// Wave 1 is four classes and nothing crept in. In particular the two
    /// throwables inside lane 3's prefix set --
    /// `java/lang/reflect/InaccessibleObjectException` and
    /// `InvocationTargetException` -- belong to the cross-cutting registrar
    /// lane, not here, and `java/lang/reflect/` admits both.
    #[test]
    fn the_l3_wave_stays_inside_lane_three() {
        for (c, m, d) in RETIRED_SHADOW_L3_TRIPLES {
            let ok = *c == "java/lang/reflect/Field"
                || *c == "java/lang/reflect/Method"
                || *c == "java/lang/reflect/Constructor"
                || *c == "java/lang/invoke/MethodType";
            assert!(ok, "not lane L3 wave 1's class: {c}.{m}{d}");
            assert!(
                !c.ends_with("Exception"),
                "lane T's throwable matched L3's prefix: {c}.{m}{d}"
            );
        }
    }

    /// The triples lane L3 measured and DECLINED to retire, one per rejection
    /// rule, so a later wave cannot quietly take them on the family's
    /// reputation.
    ///
    /// * `Field.getInt`/`getChar`/`getBoolean` and the rest of the primitive
    ///   accessors are `OK -> BAD`: the same run's descriptor-coercion census
    ///   reports 105 field reads DESTROYED by primitive-into-reference
    ///   coercion, and yielded `getChar` answers `0` where HotSpot answers
    ///   `q`.
    /// * `Field.getGenericType` is `OK -> BAD` four ways: yielding erases
    ///   `Map<String,List<T>>` to `Map`, `T` to `Number`, `T[]` to
    ///   `[LNumber;`.
    /// * `Method.invoke` and `Constructor.newInstance` are `BAD -> BAD` on the
    ///   non-nestmate rows 247/248 -- the bytecode is wrong too, so the
    ///   retirement's own justification is false for them.
    /// * `Field.setBoolean` and `Method.isBridge` and their kind agree ONLY at
    ///   a value a blanket yield returns anyway. `setBoolean` is the sharp
    ///   one: its row reads back `false` through `getBoolean`, which row 15
    ///   proves broken.
    #[test]
    fn the_l3_held_triples_are_not_retired() {
        for (c, m, d) in [
            // Primitive field access -- the coercion census explains all of it.
            ("java/lang/reflect/Field", "getInt", "(Ljava/lang/Object;)I"),
            (
                "java/lang/reflect/Field",
                "getChar",
                "(Ljava/lang/Object;)C",
            ),
            (
                "java/lang/reflect/Field",
                "getBoolean",
                "(Ljava/lang/Object;)Z",
            ),
            (
                "java/lang/reflect/Field",
                "getLong",
                "(Ljava/lang/Object;)J",
            ),
            (
                "java/lang/reflect/Field",
                "getDouble",
                "(Ljava/lang/Object;)D",
            ),
            (
                "java/lang/reflect/Field",
                "getFloat",
                "(Ljava/lang/Object;)F",
            ),
            (
                "java/lang/reflect/Field",
                "getByte",
                "(Ljava/lang/Object;)B",
            ),
            (
                "java/lang/reflect/Field",
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            // Generic signatures erase to raw when yielded.
            (
                "java/lang/reflect/Field",
                "getGenericType",
                "()Ljava/lang/reflect/Type;",
            ),
            (
                "java/lang/reflect/Field",
                "getAnnotatedType",
                "()Ljava/lang/reflect/AnnotatedType;",
            ),
            // Annotations come back empty -- the same family L0 held.
            (
                "java/lang/reflect/Field",
                "getAnnotation",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
            ),
            (
                "java/lang/reflect/Field",
                "getDeclaredAnnotations",
                "()[Ljava/lang/annotation/Annotation;",
            ),
            // BAD -> BAD: the bytecode is wrong too.
            (
                "java/lang/reflect/Method",
                "invoke",
                "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            (
                "java/lang/reflect/Constructor",
                "newInstance",
                "([Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            // Agreement only at a DEFAULT value.
            (
                "java/lang/reflect/Field",
                "setBoolean",
                "(Ljava/lang/Object;Z)V",
            ),
            ("java/lang/reflect/Field", "isSynthetic", "()Z"),
            ("java/lang/reflect/Field", "isEnumConstant", "()Z"),
            ("java/lang/reflect/Field", "trySetAccessible", "()Z"),
            ("java/lang/reflect/Method", "isBridge", "()Z"),
            ("java/lang/reflect/Method", "isSynthetic", "()Z"),
            ("java/lang/reflect/Method", "isVarArgs", "()Z"),
            ("java/lang/reflect/Method", "isDefault", "()Z"),
            ("java/lang/reflect/Constructor", "isSynthetic", "()Z"),
            ("java/lang/reflect/Constructor", "isVarArgs", "()Z"),
            (
                "java/lang/reflect/Constructor",
                "getExceptionTypes",
                "()[Ljava/lang/Class;",
            ),
            ("java/lang/reflect/Constructor", "setAccessible", "(Z)V"),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} was HELD by lane L3's measurement and must not be \
                 retired"
            );
        }
    }

    /// The 21 triples lane L0 measured and DECLINED to retire.
    ///
    /// Each is `OK -> BAD` in `apps/probes/L0ClassModuleSurface.java`: the
    /// native answers as HotSpot does and the bytecode does not. Retiring any
    /// of them trades a correct answer for a wrong one -- and for most of these
    /// the wrong answer does not throw, which is worse. The table's doc comment
    /// carries the row numbers and the observed values.
    /// **23 triples, and it enumerated 21 until an arithmetic reconciliation
    /// found the other two.** Lane L0's dispatched population is 77 of 104, and
    /// 77 - 54 retired = 23; the array held 21, so
    /// `getAnnotationsByType` and `getDeclaredAnnotationsByType` were measured
    /// `OK -> BAD` (HotSpot `1`, yielded `0`, rows 75 and 76) and then left
    /// unpinned. Nothing was red: the retirement table did not contain them, so
    /// every test passed while two members of a family whose other five ARE
    /// pinned sat unguarded, one wave away from being retired on the family's
    /// reputation.
    ///
    /// The prose said 23 and the code said 21 for the same reason the prose was
    /// right: `getAnnotation*` is six methods, not four. **Close the population
    /// by subtraction and check the residue is empty** -- retired + held +
    /// undispatched must equal the surface, and here the residue was exactly
    /// the two rows nobody had typed out.
    #[test]
    fn the_l0_held_families_are_not_retired() {
        for (c, m, d) in [
            // VM-filled state that no Java code can fill.
            (
                "java/lang/Class",
                "descriptorString",
                "()Ljava/lang/String;",
            ),
            ("java/lang/Class", "getModifiers", "()I"),
            ("java/lang/Class", "newInstance", "()Ljava/lang/Object;"),
            ("java/lang/Module", "getLayer", "()Ljava/lang/ModuleLayer;"),
            ("java/lang/ModuleLayer", "boot", "()Ljava/lang/ModuleLayer;"),
            ("java/lang/ModuleLayer", "modules", "()Ljava/util/Set;"),
            (
                "java/lang/ModuleLayer",
                "configuration",
                "()Ljava/lang/module/Configuration;",
            ),
            (
                "java/lang/ModuleLayer",
                "findModule",
                "(Ljava/lang/String;)Ljava/util/Optional;",
            ),
            // The annotation subsystem comes back EMPTY when yielded.
            (
                "java/lang/Class",
                "getAnnotations",
                "()[Ljava/lang/annotation/Annotation;",
            ),
            (
                "java/lang/Class",
                "getDeclaredAnnotations",
                "()[Ljava/lang/annotation/Annotation;",
            ),
            (
                "java/lang/Class",
                "getAnnotation",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
            ),
            (
                "java/lang/Class",
                "getDeclaredAnnotation",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
            ),
            // WITHDRAWN 2026-09-10 after the corpus arm: retired on a dial
            // arm that leaked, `OK -> BAD` on a no-dial re-measure. See the
            // table's doc comment.
            ("java/lang/Class", "arrayType", "()Ljava/lang/Class;"),
            ("java/lang/Class", "componentType", "()Ljava/lang/Class;"),
            ("java/lang/Class", "isArray", "()Z"),
            ("java/lang/Class", "getTypeName", "()Ljava/lang/String;"),
            ("java/lang/Class", "getSimpleName", "()Ljava/lang/String;"),
            (
                "java/lang/Class",
                "getCanonicalName",
                "()Ljava/lang/String;",
            ),
            (
                "java/lang/Class",
                "getClassLoader",
                "()Ljava/lang/ClassLoader;",
            ),
            (
                "java/lang/ClassValue",
                "get",
                "(Ljava/lang/Class;)Ljava/lang/Object;",
            ),
            (
                "java/lang/Class",
                "getAnnotationsByType",
                "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
            ),
            (
                "java/lang/Class",
                "getDeclaredAnnotationsByType",
                "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
            ),
            (
                "java/lang/Class",
                "isAnnotationPresent",
                "(Ljava/lang/Class;)Z",
            ),
            // The export/open predicate family.
            ("java/lang/Module", "isExported", "(Ljava/lang/String;)Z"),
            (
                "java/lang/Module",
                "isExported",
                "(Ljava/lang/String;Ljava/lang/Module;)Z",
            ),
            ("java/lang/Module", "isOpen", "(Ljava/lang/String;)Z"),
            (
                "java/lang/Module",
                "isOpen",
                "(Ljava/lang/String;Ljava/lang/Module;)Z",
            ),
            // Held PENDING LANE L7: the builtin loader hierarchy does not link,
            // so yielding raises NoClassDefFoundError rather than answering.
            ("java/lang/Class", "getPackage", "()Ljava/lang/Package;"),
            (
                "java/lang/Class",
                "getResource",
                "(Ljava/lang/String;)Ljava/net/URL;",
            ),
            (
                "java/lang/Class",
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;",
            ),
            (
                "java/lang/Module",
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;",
            ),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is HELD by lane L0's measurement and must not be retired"
            );
        }
    }

    /// `Class.getName` and `Class.getModule` are reviewed `Intrinsic`s, NOT
    /// retirements, and the difference matters: an `Intrinsic` keeps winning at
    /// every dispatch door, while a retired triple stops being registered under
    /// `--jdk-only` at all. Putting either in a retirement table would yield to
    /// bytecode that answers the internal name form and a null module.
    #[test]
    fn the_two_reviewed_intrinsics_are_not_retirements() {
        for (c, m, d) in [
            ("java/lang/Class", "getName", "()Ljava/lang/String;"),
            ("java/lang/Class", "getModule", "()Ljava/lang/Module;"),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} is a reviewed Intrinsic, not a retirement"
            );
        }
    }

    /// `java/lang/ref/` stays whole, and this is the record of WHY — the
    /// header argues it, this fails if someone acts against it.
    ///
    /// The prefix's census-eligible set includes `Reference.<init>` and the
    /// three subclass constructors, and those are not shadows in the sense this
    /// table retires. They are the VM's ONLY mutator-side call to
    /// `NativeContext::discover_reference`: nothing in the collectors scans for
    /// `java.lang.ref.Reference` instances, so a reference whose constructor
    /// yielded to real bytecode is never discovered, never cleared, and
    /// `RClassUnloadSweep`'s `payload.class.unloaded` reads `false` — which is
    /// exactly what the 2026-08-19 arm reported. Retiring the ACCESSORS alone
    /// would leave `Reference.get()` without its SATB keep-alive,
    /// `SoftReference.get()` without its LRU touch, and `Reference.enqueue()`
    /// without `mark_reference_manually_enqueued`, each of which has a named
    /// in-tree defect behind it.
    #[test]
    fn the_reference_subsystem_stays_whole() {
        for (m, d) in [
            ("<init>", "(Ljava/lang/Object;)V"),
            (
                "<init>",
                "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
            ),
            ("get", "()Ljava/lang/Object;"),
            ("clear", "()V"),
            ("enqueue", "()Z"),
            ("isEnqueued", "()Z"),
            ("refersTo", "(Ljava/lang/Object;)Z"),
        ] {
            for c in [
                "java/lang/ref/Reference",
                "java/lang/ref/WeakReference",
                "java/lang/ref/SoftReference",
                "java/lang/ref/PhantomReference",
            ] {
                assert!(
                    !triple_is_retired_shadow(c, m, d),
                    "{c}.{m}{d} was retired. Reference constructors are the VM's \
                     only reference-discovery hook; retiring them disables weak/\
                     soft/phantom clearing outright."
                );
            }
        }
        for (m, d) in [
            ("<init>", "()V"),
            ("poll", "()Ljava/lang/ref/Reference;"),
            ("remove", "()Ljava/lang/ref/Reference;"),
            ("remove", "(J)Ljava/lang/ref/Reference;"),
        ] {
            assert!(!triple_is_retired_shadow(
                "java/lang/ref/ReferenceQueue",
                m,
                d
            ));
        }
    }

    /// The ninth `WindowsFileAttributes` triple the 2026-08-19 census found,
    /// and the one deliberately left live.
    ///
    /// JDK 25 `WindowsFileAttributes.fileKey()` is `return null;` — the Windows
    /// provider has no file identity at all. CratonVM's native answers a real
    /// `(volume, index)` key built from `GetFileInformationByHandle`, which is
    /// what makes `FileTreeWalker.wouldLoop` able to see a symlink cycle on
    /// this platform. Retiring it would be HotSpot-identical and strictly worse
    /// behaviour, and it is not a state-population question at all: there is no
    /// state the real body would read. That makes it a separate decision from
    /// the eight above, so it is a separate row here.
    ///
    /// Same shape as `Logger.log`'s eighth overload in the first table: the
    /// reason this list is per-TRIPLE.
    #[test]
    fn the_held_windows_attribute_triple_is_not_retired() {
        assert!(!triple_is_retired_shadow(
            "sun/nio/fs/WindowsFileAttributes",
            "fileKey",
            "()Ljava/lang/Object;"
        ));
        // The other eight are, so this test cannot pass by the table being empty.
        assert!(triple_is_retired_shadow(
            "sun/nio/fs/WindowsFileAttributes",
            "lastModifiedTime",
            "()Ljava/nio/file/attribute/FileTime;"
        ));
    }

    /// `sun/nio/fs/UnixFileAttributes` carries the SAME nine registrations, and
    /// none of them is retired.
    ///
    /// Not an oversight and not a Windows-only fix: the 2026-08-19 census held
    /// them under `class never loaded in 36 vectors`, and `G88-1`'s rule is
    /// that `class-not-loaded` is the absence of a verdict rather than a clean
    /// one. The Unix carrier's state was already stored in the JDK's own
    /// encoding (split `st_*_sec`/`st_*_nsec` pairs, `st_mode`), so there was
    /// no FILETIME-shaped defect to fix there — but "no defect found by
    /// inspection" is not the measurement this table takes entries on.
    #[test]
    fn the_unix_attribute_carrier_is_not_retired() {
        for m in [
            "creationTime",
            "lastAccessTime",
            "lastModifiedTime",
            "isDirectory",
            "isRegularFile",
            "isSymbolicLink",
            "isOther",
            "size",
            "fileKey",
        ] {
            assert!(
                !triple_is_retired_shadow("sun/nio/fs/UnixFileAttributes", m, "()Z")
                    && !triple_is_retired_shadow("sun/nio/fs/UnixFileAttributes", m, "()J")
                    && !triple_is_retired_shadow(
                        "sun/nio/fs/UnixFileAttributes",
                        m,
                        "()Ljava/nio/file/attribute/FileTime;"
                    ),
                "sun/nio/fs/UnixFileAttributes.{m} was retired without a Linux arm"
            );
        }
    }

    /// The table is binary-searched, so ordering is correctness.
    #[test]
    fn the_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_TRIPLES.windows(2) {
            assert!(
                w[0] < w[1],
                "out of order or duplicated: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
    }

    /// Every entry must be findable through the public predicate — the prefix
    /// discriminator and the table must not disagree about what is in scope.
    #[test]
    fn every_entry_is_reachable_through_the_predicate() {
        for (c, m, d) in RETIRED_SHADOW_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable entry: {c}.{m}{d}"
            );
        }
    }

    /// The retirement is per-TRIPLE, and `Logger.log` is where that earns its
    /// keep: SEVEN of its eight registered overloads shadow real bytecode and
    /// are retired, while the eighth resolves NOWHERE in the image and is held
    /// back. A per-class or even per-(class, method) rule cannot express that.
    #[test]
    fn the_one_logger_log_overload_the_image_lacks_is_held_back() {
        // Retired: the image declares these with a Code attribute.
        for d in [
            "(Ljava/util/logging/Level;Ljava/lang/String;)V",
            "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/Throwable;)V",
            "(Ljava/util/logging/Level;Ljava/lang/Throwable;Ljava/util/function/Supplier;)V",
            "(Ljava/util/logging/LogRecord;)V",
        ] {
            assert!(
                triple_is_retired_shadow("java/util/logging/Logger", "log", d),
                "{d}"
            );
        }
        // HELD BACK: `log(Level, Supplier, Throwable)` is not a JDK 25
        // signature at all — the real overload takes the Throwable SECOND —
        // so the census resolves it nowhere and there is no bytecode for it to
        // yield to. Refusing it would replace a shadow with an
        // `UnsatisfiedLinkError`.
        assert!(!triple_is_retired_shadow(
            "java/util/logging/Logger",
            "log",
            "(Ljava/util/logging/Level;Ljava/util/function/Supplier;Ljava/lang/Throwable;)V"
        ));
        assert!(triple_is_retired_shadow(
            "java/util/logging/Logger",
            "fine",
            "(Ljava/lang/String;)V"
        ));
    }

    /// Nothing outside the retired subsystem is touched.
    #[test]
    fn other_subsystems_are_untouched() {
        assert!(!triple_is_retired_shadow(
            "java/lang/String",
            "length",
            "()I"
        ));
        assert!(!triple_is_retired_shadow(
            "javax/management/MBeanServer",
            "getDomains",
            "()[Ljava/lang/String;"
        ));
        // A logging class that is not in the table answers false too.
        assert!(!triple_is_retired_shadow(
            "java/util/logging/Logger",
            "notARealMethod",
            "()V"
        ));
    }

    /// A vacuity floor. An empty table would make every test above pass and
    /// retire nothing — the 2026-08-11 measurement recorded 84 triples, the
    /// 2026-08-12 source-pair retirement added four, and the 2026-08-12
    /// collections wave added seven more (eight registrations).
    #[test]
    fn the_table_is_not_empty() {
        assert!(
            RETIRED_SHADOW_TRIPLES.len() >= 80,
            "expected 88 java.util.logging triples + 14 java.util collections = 102, got {}",
            RETIRED_SHADOW_TRIPLES.len()
        );
    }

    /// The `java/util` collections triples are retired — and the eight
    /// registrations they cover are `<init>(I)V` twice, which this table
    /// expresses as ONE entry because it retires TRIPLES.
    #[test]
    fn the_retirable_collections_slice_is_retired() {
        for (c, m, d) in [
            ("java/util/ArrayList", "<init>", "()V"),
            ("java/util/ArrayList", "<init>", "(I)V"),
            ("java/util/ArrayList", "<init>", "(Ljava/util/Collection;)V"),
            ("java/util/ArrayList", "add", "(Ljava/lang/Object;)Z"),
            ("java/util/ArrayList", "clear", "()V"),
            (
                "java/util/Arrays$ArrayList",
                "iterator",
                "()Ljava/util/Iterator;",
            ),
            (
                "java/util/Collections",
                "synchronizedMap",
                "(Ljava/util/Map;)Ljava/util/Map;",
            ),
        ] {
            assert!(triple_is_retired_shadow(c, m, d), "not retired: {c}.{m}{d}");
        }
    }

    /// The sixty registrations that are NOT retirable stay untouched — and this
    /// is the test that earns the widened `java/util/` prefix.
    ///
    /// Each family below keeps its entries in a Rust side table or a fabricated
    /// slot layout, so refusing its shadow hands real bytecode an EMPTY
    /// collection rather than a working one. That is not a reading: with
    /// `CRATONVM_ENFORCE_NATIVE_SHADOW` scoped to each of the three families
    /// under `--jdk-only` on 2026-08-12, `TreeMap.firstKey` answered the wrong
    /// key and `TreeSet` lost half its elements, `ConcurrentHashMap.size`
    /// answered 1 for a two-entry map, and `ArrayDeque` reported 39 after 40
    /// `addLast`s and then dropped an element across a drain/refill.
    ///
    /// `Arrays.copyOf` is in the list for the opposite reason: it is the one
    /// triple the five `ArrayList` retirements DEPEND on staying a `Bridge`.
    ///
    /// # RE-ASKED 2026-08-30, and the hold is CORRECT
    ///
    /// A hold list is a hypothesis with a date on it. Seven lanes had since
    /// made these classes' state more real, which is this module's own stated
    /// precondition for retiring a shadow, so the list was re-measured rather
    /// than assumed. It survived — but only because it was asked with the
    /// right instrument, and the wrong one said the opposite:
    ///
    /// ```text
    ///   14-vector regression corpus, ConcurrentHashMap armed   14/14 PASS
    ///   ChmShadowSweep, the family's OWN content probe         0 changed rows
    ///                                                          over 39 357 yields
    ///   MapViewsShadowSweep, ANOTHER family's probe            died at 261/302,
    ///                                                          53 rows changed
    /// ```
    ///
    /// **The rows it breaks are `java.util.Properties`', not
    /// `ConcurrentHashMap`'s.** JDK 25's `Properties` holds a
    /// `private transient volatile ConcurrentHashMap<Object,Object> map` and
    /// delegates its `Hashtable` methods to it, so retiring CHM's natives puts
    /// real CHM bytecode under a map whose state this VM keeps in a side
    /// table. Every `Properties` view empties out — `keySet()` returns `[]` on
    /// a three-entry table, and `keySet().remove` writes through to nothing —
    /// and the run then dies inside `ConcurrentHashMap$KeyIterator.next`.
    /// Silent data loss for forty rows before anything throws.
    ///
    /// So the entries below are held for a reason wider than the one above
    /// them: not only "this family's own collections empty out", but **a
    /// retirement's blast radius is its class's USERS**. The family's own
    /// probe being clean is the trap and not the reassurance — it asks about
    /// the operations the family declares, and a view is another class's
    /// method returning another class's object.
    ///
    /// The view and iterator classes are listed explicitly for the same
    /// reason `Logger.log`'s eighth overload is: a per-class sweep called all
    /// five CHM classes RETIRE-SAFE, and nothing but an entry here records
    /// that they were considered and rejected.
    #[test]
    fn the_held_collection_families_are_not_retired() {
        for (c, m, d) in [
            // needs-VM-support: state is not real.
            (
                "java/util/TreeMap",
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            ("java/util/TreeSet", "add", "(Ljava/lang/Object;)Z"),
            // 2026-09-10, L1 wave 1: `java/util/ArrayDeque` came OFF this
            // list. The `needs-VM-support` label above was true when it was
            // written and is not now — `ad_ensure_capacity` keeps the JDK's
            // own one-spare-slot emptiness invariant on the receiver's real
            // `elements`/`head`/`tail`, and slot 3 (`size`) is a spare the
            // real class does not declare, so no real body reads it. Armed
            // alone on `cratonvm-l1-base-20260910`, 44 probes: 0 worse, and
            // `NullArgMsgProbe` -2. See `RETIRED_SHADOW_L1_TRIPLES`.
            (
                "java/util/Hashtable",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            ("java/util/LinkedHashSet", "add", "(Ljava/lang/Object;)Z"),
            // 2026-09-11, L1 wave 3: `java/util/HashMap` and its six view and
            // iterator classes came OFF this list, all 98 triples together.
            // The hold was one observable — `entrySet().toArray()` reading
            // EMPTY under the dial — and it was a DIAL ARTEFACT: the native
            // that answers is `native_al_to_array` on `AbstractCollection`,
            // and the `size()` question it falls back on is asked from inside
            // a native, where there is no dispatch door to decline at.
            // Retirement removes the registration instead of declining at a
            // door, so that question reaches real bytecode. See
            // `RETIRED_SHADOW_L1_HM_TRIPLES` for the trace and the numbers.
            ("java/util/HashSet", "iterator", "()Ljava/util/Iterator;"),
            // 2026-09-10, L1 wave 1: `java/util/LinkedList` came OFF this
            // list, for the same correction. `ll_set` publishes `first`,
            // `last` and `size` to the receiver's own fields beside the
            // overlay, and `ll_alloc_node` uses the real `LinkedList$Node`
            // slot order (item@0, next@1, prev@2) — both changes landed
            // AFTER this entry was written, for Java serialization, and
            // they are what makes the real bodies readable. Armed alone,
            // 44 probes: 0 worse, `DequeListShadowSweep` 1291 yields.
            // `size`, `get`, `contains`, `isEmpty`, `iterator` and both
            // `toArray` overloads ALL came off this list on 2026-08-17. What was
            // left of the family was the ITERATOR CLASS, not the list methods:
            // `ArrayList$Itr` keeps `cursor`/`lastRet`/`expectedModCount`
            // through `al_itr_slots`, and no per-triple trial had been run.
            //
            // 2026-09-10, L1 wave 1: the trial was run and `ArrayList$Itr`,
            // `$ListItr`, `$SubList` and `$SubList$1` came off together. They
            // have to move as a set - `al_itr_slots` is the SAME three slots
            // the real `ArrayList$Itr` declares, so a half-retired family is a
            // split store in the one direction the class cannot survive.
            // `$Itr` contributes one row, not the `next` this entry named:
            // `hasNext` and `next` on that class are already `SyntheticStub`
            // before any table is consulted, so `remove()V` is its only
            // `Bridge`.
            //
            // `java/util/Arrays.copyOf` was held here as "load-bearing FOR the
            // retirements above" and came off in the same wave, which is the
            // only coherent move: the dependency and its dependents retire
            // together or the real `ArrayList.grow` bytecode calls a native
            // `copyOf` that answers from a model the real list no longer uses.
            // Armed alone, 44 probes, 0 worse - `ThrowableFamilySweep` alone
            // put 5,299 yields through the `java/util/Arrays` prefix.
            // 2026-09-09: the CHM family — `put`, `keySet`, `values` and the
            // key/value iterators — came OFF this list in the Phase 3 wave, and
            // the reason they were on it is the reason they left together. The
            // 2026-08-30 note held them as a set because "the producer and the
            // consumers have to be held together or the survivor is handed a
            // receiver the retired half built". That is the same coupling
            // argument `RETIRED_SHADOW_PHASE3_TRIPLES` acts on, run to its end:
            // the set that has to move together is the WHOLE class plus
            // `java/util/Properties`, which owns the `map` field every
            // `Properties` body reads. Holding any part of it is the split
            // store the note was written to prevent.
            //
            // `java/util/Hashtable` above is NOT part of that move and stays
            // held. `Properties extends Hashtable`, but JDK 9 moved the storage
            // into `Properties.map` and `Properties` overrides the `Map`
            // surface, so a dispatch door asking about the DECLARING class
            // reaches `Properties` for every overridden method and never
            // `Hashtable`.
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "wrongly retired: {c}.{m}{d}"
            );
        }
    }

    /// `LogRecord`'s source pair is retired as a SET of four.
    ///
    /// Not a restatement of the table: it is the property that keeps a later
    /// edit from retiring the getters and leaving the setters, which is
    /// strictly worse than retiring neither. The real getter honours
    /// `needToInferCaller`; the shadow setter never clears it; so a getter-only
    /// retirement makes an explicit `setSourceClassName("X")` get silently
    /// overwritten by the inferred caller on the next read.
    #[test]
    fn the_log_record_source_pair_is_retired_as_a_set() {
        for (m, d) in [
            ("getSourceClassName", "()Ljava/lang/String;"),
            ("getSourceMethodName", "()Ljava/lang/String;"),
            ("setSourceClassName", "(Ljava/lang/String;)V"),
            ("setSourceMethodName", "(Ljava/lang/String;)V"),
        ] {
            assert!(
                triple_is_retired_shadow("java/util/logging/LogRecord", m, d),
                "the source pair retires as a set; {m}{d} is missing"
            );
        }
    }

    /// G60-1 §5's two `java/util/ArrayList` nominations, plus the four its
    /// resolution held back and then retired on 2026-08-17.
    ///
    /// Seven triples, one measurement: the probe is byte-identical to HotSpot
    /// with all of them refused and both corpora are verdict-neutral. The four
    /// were held for a day on the strength of a `ConcurrentModificationException`
    /// under `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/ArrayList`, and the module
    /// docs record why that was the wrong reading — the exception came from the
    /// `java/util/Collection.iterator` INTERFACE DOOR handing back an
    /// `ArrayList$Itr` over a snapshot, not from this class's `iterator`.
    #[test]
    fn the_seven_array_list_rows_g60_1_settled() {
        for (m, d) in [
            ("contains", "(Ljava/lang/Object;)Z"),
            ("get", "(I)Ljava/lang/Object;"),
            ("isEmpty", "()Z"),
            ("iterator", "()Ljava/util/Iterator;"),
            ("size", "()I"),
            ("toArray", "()[Ljava/lang/Object;"),
            ("toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
        ] {
            assert!(
                triple_is_retired_shadow("java/util/ArrayList", m, d),
                "retired 2026-08-17 on a per-triple trial: {m}{d}"
            );
        }
    }

    /// The boundary the seven above stopped at, and where it moved to.
    ///
    /// `ArrayList$Itr` is a different receiver with its own state — `cursor`,
    /// `lastRet` and `expectedModCount` through `al_itr_slots` — and when the
    /// seven landed on 2026-08-17 no per-triple trial had been run on it, so
    /// retiring the LIST methods said nothing about the ITERATOR class.
    ///
    /// **L1 wave 1 ran that trial on 2026-09-10 and the iterator classes are
    /// now retired**, together with `$SubList` and `$SubList$1`. The reason
    /// they move as a SET is the reason they were separable before:
    /// `al_itr_slots` is the same three slots the real `ArrayList$Itr`
    /// declares, so once the list methods are the real bytecode's, a surviving
    /// native iterator is a second reader of one state — and the real
    /// `ArrayList.iterator()` hands back a real `Itr` the native cannot serve.
    ///
    /// `$Itr` contributes exactly ONE row to that wave, and the census is why:
    /// `hasNext()Z` and `next()Ljava/lang/Object;` are already
    /// `SyntheticStub` on this class before any table sees them, so
    /// `remove()V` is the only `Bridge` left on it. `$ListItr` contributes
    /// ten, three of which (`hasNext`, `next`, `remove`) are bucket **B** —
    /// inherited from `$Itr` — which is the same set from the other side.
    ///
    /// **The interface door has NOT moved**, and that is still the boundary
    /// this test defends. `java/util/Collection.iterator` is where the
    /// values-view iterator actually comes from (G63-1); retiring it is a
    /// different change with a much wider blast radius, and no wave has made
    /// it.
    #[test]
    fn the_iterator_classes_moved_and_the_interface_door_did_not() {
        assert!(triple_is_retired_shadow(
            "java/util/ArrayList$Itr",
            "remove",
            "()V"
        ));
        assert!(triple_is_retired_shadow(
            "java/util/ArrayList$ListItr",
            "previous",
            "()Ljava/lang/Object;"
        ));
        assert!(triple_is_retired_shadow(
            "java/util/ArrayList$ListItr",
            "next",
            "()Ljava/lang/Object;"
        ));
        assert!(!triple_is_retired_shadow(
            "java/util/Collection",
            "iterator",
            "()Ljava/util/Iterator;"
        ));
    }

    /// `Properties.getProperty` is retired, and the condition it waited on is
    /// the one this test used to enforce.
    ///
    /// This assertion is INVERTED from what it was between G60-1 and
    /// 2026-09-09, and the inversion is the point. It held both overloads back
    /// against a later wave adding them because "the real bytecode is
    /// String-keyed JDK code and correct by construction" — which is true, and
    /// still broke `System.getProperties().getProperty(..)`, because the object
    /// that call runs on was built by a native that never initialised the real
    /// `map` field JDK 25's `getProperty` reads. Its own words were: **retire
    /// them in the same change that makes that receiver real, or not at all.**
    ///
    /// Phase 3 is that change. `native-builtins`'
    /// `properties_sidetable::replace_real_map` fills the field from the
    /// `--jdk-only` arm of the `java/lang/System.getProperties` registration.
    ///
    /// This crate cannot see that function — `native-builtins` depends on
    /// `native-api` and not the other way round — so this test can only record
    /// the condition, never check it. The check is
    /// `the_jdk_only_get_properties_arm_fills_the_real_map` in
    /// `native-builtins/tests/registry_contracts.rs`, which fails if the call
    /// is removed while these rows stay in the table. A comment is not a
    /// compile-time link; that test is the link.
    #[test]
    fn properties_get_property_is_retired_with_the_receiver_that_made_it_safe() {
        for d in [
            "(Ljava/lang/String;)Ljava/lang/String;",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        ] {
            assert!(
                triple_is_retired_shadow("java/util/Properties", "getProperty", d),
                "retired 2026-09-09 with the Phase 3 union: getProperty{d}"
            );
        }
    }

    /// The Phase 3 table obeys the same two invariants as its siblings.
    ///
    /// Sortedness is not cosmetic: [`triple_is_retired_shadow`] answers with a
    /// binary search, so ONE out-of-order entry makes that entry answer `false`
    /// — which reads as "not retired" and is invisible in any workload.
    #[test]
    fn the_phase3_table_is_sorted_and_unique() {
        let mut sorted = RETIRED_SHADOW_PHASE3_TRIPLES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.as_slice(),
            RETIRED_SHADOW_PHASE3_TRIPLES,
            "RETIRED_SHADOW_PHASE3_TRIPLES must be sorted and duplicate-free"
        );
    }

    /// The wave is a PAIR of classes, and nothing else.
    ///
    /// The union is the whole finding — each class alone is a split store — so
    /// a later edit that slips a third receiver into this table is making a
    /// different, unmeasured claim. `java/util/Hashtable` is the specific one
    /// to keep out: `Properties` extends it, and it stays a `Bridge`.
    #[test]
    fn the_phase3_wave_is_exactly_two_class_families() {
        for (c, _, _) in RETIRED_SHADOW_PHASE3_TRIPLES {
            assert!(
                c.starts_with("java/util/concurrent/ConcurrentHashMap")
                    || c.starts_with("java/util/Properties"),
                "Phase 3 is the CHM/Properties union; {c} is a different wave"
            );
        }
        assert!(!triple_is_retired_shadow(
            "java/util/Hashtable",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ));
    }

    /// The L1 table obeys the two invariants every table here obeys.
    ///
    /// Sortedness is correctness, not tidiness: [`triple_is_retired_shadow`]
    /// answers with a binary search, so ONE out-of-order entry makes that
    /// entry answer `false` — which reads as "not retired" and is invisible
    /// in any workload.
    #[test]
    fn the_l1_table_is_sorted_and_unique() {
        let mut sorted = RETIRED_SHADOW_L1_TRIPLES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.as_slice(),
            RETIRED_SHADOW_L1_TRIPLES,
            "RETIRED_SHADOW_L1_TRIPLES must be sorted and duplicate-free"
        );
    }

    /// Every L1 entry is reachable through the real predicate.
    ///
    /// `RETIRED_SHADOW_PREFIXES` already carries `java/util/`, so this wave
    /// needed no prefix edit — which is exactly the situation where a missing
    /// one would go unnoticed, because the table would still compile, still
    /// be sorted, and still retire nothing.
    #[test]
    fn every_l1_entry_is_reachable() {
        for (c, m, d) in RETIRED_SHADOW_L1_TRIPLES {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "unreachable entry: {c}.{m}{d}"
            );
        }
    }

    /// No L1 entry is also in one of the five sibling tables.
    ///
    /// The predicate ORs, so a doubled row retires nothing twice — but it is
    /// two provenances for one decision, and the next reader cannot tell
    /// which measurement backs it. `RETIRED_SHADOW_L2_TRIPLES` is in the list
    /// because L1 and L2 landed on the same day: L2's prefixes are
    /// `java/lang/` and `java/math/` and L1's are disjoint from them by the
    /// ownership table, so this is a check on the ownership table as much as
    /// on the tables.
    #[test]
    fn the_l1_table_does_not_overlap_a_sibling() {
        for t in RETIRED_SHADOW_L1_TRIPLES {
            assert!(
                RETIRED_SHADOW_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_STATELESS_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_PHASE2_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_PHASE3_TRIPLES.binary_search(t).is_err()
                    && RETIRED_SHADOW_L2_TRIPLES.binary_search(t).is_err(),
                "{t:?} is in the L1 table and a sibling"
            );
        }
    }

    /// The L1 table is exactly the prefixes it measured, and nothing else.
    ///
    /// The dial arms a PREFIX and this table is per-TRIPLE, so the two can
    /// drift apart silently: a row added here under `java/util/TreeMap` would
    /// be retiring a family whose own armed run moved nine probes AWAY from
    /// HotSpot. This is the gate on that. Six prefixes from wave 1, five more
    /// from wave 2.
    #[test]
    fn the_l1_table_is_exactly_the_prefixes_it_measured() {
        for (c, _, _) in RETIRED_SHADOW_L1_TRIPLES {
            assert!(
                // wave 1
                c.starts_with("java/util/ArrayList")
                    || c.starts_with("java/util/ArrayDeque")
                    || c.starts_with("java/util/LinkedList")
                    || c.starts_with("java/util/Collections")
                    || c.starts_with("java/util/Optional")
                    || c.starts_with("java/util/Arrays")
                    // wave 2
                    || c.starts_with("java/util/zip/")
                    || c.starts_with("java/util/Stack")
                    || c.starts_with("java/util/Vector")
                    || c.starts_with("java/util/PriorityQueue")
                    || c.starts_with("java/time/"),
                "{c} is outside the prefixes L1 armed and measured"
            );
        }
    }

    /// The two families wave 2 measured LOAD-BEARING, and why they matter.
    ///
    /// `java/util/jar/` and `java/text/` both read `0 worse` in wave 1's
    /// sweep, over 44 probes, with 44 of 44 and 44 of 44 VACUOUS. Both turned
    /// red the moment `apps/probes/L1TailSweep.java` reached them: `jar`
    /// armed takes that probe from 11 diffs to 39 and truncates it, `text`
    /// armed to 22. They are pinned here rather than merely absent because
    /// "no row" and "measured and refused" read identically from outside, and
    /// because the first reading of these two was a green.
    #[test]
    fn the_vacuous_green_families_are_held() {
        for (c, m, d) in [
            (
                "java/util/jar/JarFile",
                "getJarEntry",
                "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
            ),
            // 2026-09-11, L1 wave 4: `java/util/jar/Manifest` came OFF this
            // list and `java/util/jar/JarFile` took its place. The vacuous
            // green was real and so was the red that replaced it — but the
            // red is ONE CLASS. Armed alone on `L1TailSweep` (base 13
            // diffs): `JarFile` +34, `JarEntry` +0 (reached 16), `Manifest`
            // +0 (reached 23), `Attributes` +0 (reached 156),
            // `Attributes$Name` +0 (reached 127).
            (
                "java/util/jar/JarFile",
                "getManifest",
                "()Ljava/util/jar/Manifest;",
            ),
            // 2026-09-11, L1 wave 4 put `java/text/BreakIterator` on this
            // list (`getWordInstance(Locale)` and `next()`), on the same
            // bisection that took `java/text/Normalizer` off it:
            // `java/text/` armed whole is +16 on `L1TailSweep` and
            // `BreakIterator` armed alone is the same +16.
            //
            // WAVE 6 TOOK BOTH ROWS OFF, and not by re-reading the dial.
            // The +16 was real and its CAUSE is now fixed: the family threw
            // `AbstractMethodError` because yielding sent real bytecode to a
            // provider chain that answered null, and then -- once wave 5
            // fixed that -- to a correct `sun.text.RuleBasedBreakIterator`
            // whose text had been written into slot 0 of an object whose
            // slot 0 is `charCategoryTable`. With `setText(String)` and
            // `preceding(int)` stepping aside for real receivers and the
            // BREAKITER pin gone, `L1BreakIterRealProbe` matches HotSpot on
            // all 30 rows and the family is retired WHOLE in
            // `RETIRED_SHADOW_L1_BI_TRIPLES`.
            //
            // A held row is held until the blocker is fixed, not forever;
            // this is what taking one off is supposed to look like -- a
            // named cause, a probe that measured it, and a trial binary.
            //
            // `java/text/DateFormat` is still out, and still for §7's
            // reason: +0 with `reached == 0`.
            //
            // Retiring `Normalizer` is what §9 said it could not do: with
            // `java/text/` held, the null-argument contract had to live in
            // `normalizer_reject_nulls` inside the native. It can now be the
            // image's own body again, in `--jdk-only`.
            (
                "java/text/DateFormat",
                "getDateInstance",
                "(I)Ljava/text/DateFormat;",
            ),
            // NOT a vacuous green — this one was clean over 51 probes with
            // the dial armed and turned red only on the trial binary, because
            // `TimeZone.getDisplayName`'s native reaches `ResourceBundle`
            // from INSIDE a native and the dial declines at dispatch doors
            // only. Ten rows across five probes went from
            // "Eastern Standard Time" to "Coordinated Universal Time".
            (
                "java/util/ResourceBundle",
                "getBundle",
                "(Ljava/lang/String;)Ljava/util/ResourceBundle;",
            ),
            (
                "java/util/ResourceBundle",
                "getString",
                "(Ljava/lang/String;)Ljava/lang/String;",
            ),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "wave 2 measured this load-bearing: {c}.{m}{d}"
            );
        }
    }

    /// Wave 2's eight undispatchable rows stay out, on wave 1's terms.
    ///
    /// Four `ResourceBundle` instance methods sit behind a live NPE that
    /// `L1TailSweep` records (`new PropertyResourceBundle(stream)` →
    /// `Cannot invoke "java.util.Collection.toArray()" because "c" is null`),
    /// so no probe can reach them until that is fixed; the other four are
    /// shared-secret accessor shims (`ResourceBundle$1`, `ZipFile$1`) that no
    /// bytecode can name.
    #[test]
    fn wave_2_excludes_the_row_no_probe_can_dispatch() {
        assert!(!triple_is_retired_shadow(
            "java/util/zip/ZipFile$1",
            "getManifestName",
            "(Ljava/util/jar/JarFile;Z)Ljava/lang/String;"
        ));
        // The control, from the same family and the same probe: a row
        // `L1TailSweep` DID reach, and it is retired. `CRC32C.update([BII)V`
        // is also a §1.4 defect the retirement repairs — the native accepted
        // `len` past the end of the array where HotSpot throws
        // `ArrayIndexOutOfBoundsException`.
        assert!(triple_is_retired_shadow(
            "java/util/zip/CRC32C",
            "update",
            "([BII)V"
        ));
    }

    /// A retired PRODUCER makes a zero-invocation CONSUMER reachable, and
    /// this is the test that says so.
    ///
    /// Seven rows were excluded from wave 1 for want of precondition 4: they
    /// own their slot, are `Bridge`, have image `Code`, and
    /// `apps/probes/L1Wave1Sweep.java` was written to call them and left
    /// every counter at 0 in BOTH modes.
    ///
    /// **Six of the seven had to come back in, and the first trial binary is
    /// what proved it.** `ArrayListShadowSweep` row 125 —
    /// `Arrays.asList((Object[]) null)` — went from matching HotSpot to
    /// `no-throw`. Real `Arrays.asList` is `return new ArrayList<>(a)` and
    /// real `Arrays$ArrayList.<init>` is `a = Objects.requireNonNull(array)`,
    /// so retiring `asList` routes the call into a constructor that had a
    /// counter of 0 *because the native `asList` never reached it*. The
    /// same argument covers `Collections$SetFromMap`: its producer
    /// `Collections.newSetFromMap` is retired in the same wave.
    ///
    /// So precondition 4 is measured on the UNRETIRED binary, and a zero
    /// there means "nothing reaches it TODAY", not "nothing can reach it".
    /// When the row that was serving the producer is in the same wave, the
    /// consumer moves with it or the family is a split store.
    ///
    /// `java/util/Collections.<clinit>()V` is the one that stays out, and it
    /// stays out because it has no producer to retire: a `<clinit>` is
    /// reached by class initialisation, which this table cannot change.
    #[test]
    fn a_retired_producer_pulls_its_zero_invocation_consumers_in() {
        for (c, m, d) in [
            (
                "java/util/Arrays$ArrayList",
                "<init>",
                "([Ljava/lang/Object;)V",
            ),
            ("java/util/Collections$SetFromMap", "size", "()I"),
            ("java/util/Collections$SetFromMap", "isEmpty", "()Z"),
            (
                "java/util/Collections$SetFromMap",
                "contains",
                "(Ljava/lang/Object;)Z",
            ),
            (
                "java/util/Collections$SetFromMap",
                "iterator",
                "()Ljava/util/Iterator;",
            ),
            (
                "java/util/Collections$SetFromMap",
                "toArray",
                "()[Ljava/lang/Object;",
            ),
        ] {
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} must move with the producer that reaches it"
            );
        }
        // Both producers, so the coupling is a compile-time link and not a
        // sentence in a doc comment.
        assert!(triple_is_retired_shadow(
            "java/util/Arrays",
            "asList",
            "([Ljava/lang/Object;)Ljava/util/List;"
        ));
        assert!(triple_is_retired_shadow(
            "java/util/Collections",
            "newSetFromMap",
            "(Ljava/util/Map;)Ljava/util/Set;"
        ));
        // The one with no producer stays out.
        assert!(!triple_is_retired_shadow(
            "java/util/Collections",
            "<clinit>",
            "()V"
        ));
    }

    /// The families L1 measured LOAD-BEARING are still held.
    ///
    /// The other half of `the_l1_wave_is_exactly_the_six_prefixes_it_measured`:
    /// that test stops a row being added, this one names what was refused, so
    /// a later wave has to change a test to change a verdict. Each row below
    /// moved at least one probe AWAY from HotSpot when its prefix was armed
    /// alone on `cratonvm-l1-base-20260910`, and "absent from the table" and
    /// "measured and refused" read identically from outside.
    #[test]
    fn the_l1_families_measured_load_bearing_are_still_held() {
        for (c, m, d) in [
            // 9 probes worse. State is `tm_array_table()`, a Rust side table;
            // the real `root`/`size`/`comparator` are never written.
            (
                "java/util/TreeMap",
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            ("java/util/TreeSet", "add", "(Ljava/lang/Object;)Z"),
            // 11 probes worse, one truncates. State is `lhm_overlay()`.
            (
                "java/util/LinkedHashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            // 2026-09-11, L1 wave 3: `java/util/HashMap.put` came OFF this
            // list. "9 probes worse, two truncate" was true of the DIAL and
            // false of the change: a trial binary that retires the map
            // surface is 0 diffs on all 142 rows of
            // `apps/probes/L1MapFamilySweep.java`. What the dial could not
            // see, and what still holds 77 of the family's 98, is on the
            // row below — see `RETIRED_SHADOW_L1_HM_TRIPLES` and
            // `wave_three_refused_the_iterators_the_views_and_lhm_s_inherited_eight`.
            (
                "java/util/HashMap$KeySet",
                "iterator",
                "()Ljava/util/Iterator;",
            ),
            // 4 probes worse.
            (
                "java/util/Hashtable",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            // 5 probes worse.
            ("java/util/Date", "getTime", "()J"),
            ("java/util/TimeZone", "getID", "()Ljava/lang/String;"),
            // 3 probes worse and `LocaleDateTzShadowSweep` truncates 125 -> 8.
            ("java/util/Locale", "getLanguage", "()Ljava/lang/String;"),
        ] {
            assert!(
                !triple_is_retired_shadow(c, m, d),
                "measured load-bearing by L1 wave 1: {c}.{m}{d}"
            );
        }
    }

    /// The dead registration stays out.
    ///
    /// `reduceEntries(JLjava/util/function/BiFunction;)` is registered with a
    /// descriptor returning `Ljava/lang/Object;`; the real erasure returns
    /// `Ljava/util/Map$Entry;`, so `image_declaring_method.declared` is `false`
    /// and there is no `Code` to yield to. It is the one row on these two
    /// prefixes that owns its slot, is a `Bridge`, and is still not retirable —
    /// precondition 3, as a live example rather than a rule.
    #[test]
    fn the_phase3_wave_excludes_the_registration_with_no_image_target() {
        assert!(!triple_is_retired_shadow(
            "java/util/concurrent/ConcurrentHashMap",
            "reduceEntries",
            "(JLjava/util/function/BiFunction;)Ljava/lang/Object;"
        ));
        // The sibling overload DOES have an image target and is retired, so the
        // exclusion above is about that descriptor and not about the name.
        assert!(triple_is_retired_shadow(
            "java/util/concurrent/ConcurrentHashMap",
            "reduceEntries",
            "(JLjava/util/function/Function;Ljava/util/function/BiFunction;)Ljava/lang/Object;"
        ));
    }
}
