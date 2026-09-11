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
static RETIRED_SHADOW_PHASE2_TRIPLES: &[(&str, &str, &str)] = &[
    ("sun/nio/ch/FileChannelImpl", "truncate", "(J)Ljava/nio/channels/FileChannel;"),
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

/// Lane 2 (`java/lang/` remainder, `java/math/`), 2026-09-10.
///
/// Lane 2's own population is **390 shadows over 57 classes**, once lane T's
/// boundary-crossing registrars are carved out of the 992 rows under its prefix
/// set. This table holds the rows that earned a retirement; every other row in
/// that population is dispositioned in
/// `docs/known-issues/jdk-only-lanes/lane-2-lang-values.md`.
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
/// **`remainder`, `mod`, `gcd`, `and`, `or`, `xor` — the JIT drops the
/// message.** These do reach bytecode, and interpreted they are HotSpot-exact.
/// Once the real body is JIT-compiled the `NullPointerException` arrives with
/// no message at all. It is not a `BigInteger` fact — `apps/probes/L2JitNpeProbe.java`
/// asks five null-deref shapes cold and hot with no JDK class involved and
/// every one of them loses its message when hot. See
/// `docs/known-issues/jit/the-helpful-npe-message-is-lost-in-compiled-code-20260910.md`.
/// Retiring them today would trade a correct message for none on exactly the
/// rows a caller reads when something has already gone wrong.
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
    ("java/lang/Package", "getPackages", "()[Ljava/lang/Package;"),
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
pub fn triple_is_retired_shadow(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    if !RETIRED_SHADOW_PREFIXES
        .iter()
        .any(|p| class_name.starts_with(p))
    {
        return false;
    }
    let key = (class_name, method_name, descriptor);
    RETIRED_SHADOW_TRIPLES.binary_search(&key).is_ok()
        || RETIRED_SHADOW_STATELESS_TRIPLES.binary_search(&key).is_ok()
        || RETIRED_SHADOW_PHASE2_TRIPLES.binary_search(&key).is_ok()
        || RETIRED_SHADOW_L2_TRIPLES.binary_search(&key).is_ok()
        || RETIRED_SHADOW_PHASE3_TRIPLES.binary_search(&key).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_l2_table_is_disjoint_from_the_other_three() {
        for key in RETIRED_SHADOW_L2_TRIPLES {
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
            ] {
                assert!(
                    other.binary_search(key).is_err(),
                    "{key:?} is in both lane 2's table and {name}. Two tables \
                     claiming one triple means two measurements claim it, and \
                     only one of them can be the record."
                );
            }
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
            ("java/util/ArrayDeque", "addLast", "(Ljava/lang/Object;)V"),
            (
                "java/util/Hashtable",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            ("java/util/LinkedHashSet", "add", "(Ljava/lang/Object;)Z"),
            (
                "java/util/HashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            ("java/util/HashSet", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/LinkedList", "add", "(Ljava/lang/Object;)Z"),
            // `size`, `get`, `contains`, `isEmpty`, `iterator` and both
            // `toArray` overloads ALL came off this list on 2026-08-17. What is
            // left of the family is the ITERATOR CLASS, not the list methods:
            // `ArrayList$Itr` still keeps `cursor`/`lastRet`/`expectedModCount`
            // through `al_itr_slots`, and no per-triple trial has been run on it.
            ("java/util/ArrayList$Itr", "next", "()Ljava/lang/Object;"),
            // Load-bearing FOR the retirements above.
            (
                "java/util/Arrays",
                "copyOf",
                "([Ljava/lang/Object;I)[Ljava/lang/Object;",
            ),
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

    /// The boundary the seven above stop at, and it is not arbitrary.
    ///
    /// `ArrayList$Itr` is a different receiver with its own state — `cursor`,
    /// `lastRet` and `expectedModCount` through `al_itr_slots` — and no
    /// per-triple trial has been run on it. Retiring the LIST methods says
    /// nothing about the ITERATOR class, and this test exists so a later reader
    /// does not take the seven as covering it.
    #[test]
    fn the_iterator_class_is_a_separate_question() {
        assert!(!triple_is_retired_shadow(
            "java/util/ArrayList$Itr",
            "next",
            "()Ljava/lang/Object;"
        ));
        // And the interface door, which is where the values-view iterator
        // actually comes from (G63-1). Retiring it is a different change with a
        // much wider blast radius, and this table does not make it.
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
