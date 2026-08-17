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
//! `iterator()`, `toArray()`, `isEmpty()` and `contains()` are NOT retired here
//! and the `iterator()` result above is why: the dial says at least one of them
//! is load-bearing, and separating which needs its own per-triple trial.
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
    // `get` and `size`, retired 2026-08-17 — the two rows G60-1 §5 N1 asked
    // about. They were held in the 2026-08-12 wave because `Map.values()` was
    // answered as an `ArrayList` with its source map stashed in a trailing
    // capacity slot, making these two the view's implementation. **That is no
    // longer what happens, and the hold also imported a Compatible-mode hazard
    // into a strict-only decision.** See the G60-1 section of this module's
    // docs for the four arms and the eleven carrier classes.
    ("java/util/ArrayList", "get", "(I)Ljava/lang/Object;"),
    ("java/util/ArrayList", "size", "()I"),
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
    if !class_name.starts_with("java/util/") {
        return false;
    }
    RETIRED_SHADOW_TRIPLES
        .binary_search(&(class_name, method_name, descriptor))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is binary-searched, so ordering is correctness.
    #[test]
    fn the_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_TRIPLES.windows(2) {
            assert!(w[0] < w[1], "out of order or duplicated: {:?} then {:?}", w[0], w[1]);
        }
    }

    /// Every entry must be findable through the public predicate — the prefix
    /// discriminator and the table must not disagree about what is in scope.
    #[test]
    fn every_entry_is_reachable_through_the_predicate() {
        for (c, m, d) in RETIRED_SHADOW_TRIPLES {
            assert!(triple_is_retired_shadow(c, m, d), "unreachable entry: {c}.{m}{d}");
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
            assert!(triple_is_retired_shadow("java/util/logging/Logger", "log", d), "{d}");
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
            "java/util/logging/Logger", "fine", "(Ljava/lang/String;)V"));
    }

    /// Nothing outside the retired subsystem is touched.
    #[test]
    fn other_subsystems_are_untouched() {
        assert!(!triple_is_retired_shadow("java/lang/String", "length", "()I"));
        assert!(!triple_is_retired_shadow("javax/management/MBeanServer", "getDomains",
                                          "()[Ljava/lang/String;"));
        // A logging class that is not in the table answers false too.
        assert!(!triple_is_retired_shadow("java/util/logging/Logger", "notARealMethod", "()V"));
    }

    /// A vacuity floor. An empty table would make every test above pass and
    /// retire nothing — the 2026-08-11 measurement recorded 84 triples, the
    /// 2026-08-12 source-pair retirement added four, and the 2026-08-12
    /// collections wave added seven more (eight registrations).
    #[test]
    fn the_table_is_not_empty() {
        assert!(
            RETIRED_SHADOW_TRIPLES.len() >= 80,
            "expected 88 java.util.logging triples + 9 java.util collections = 97, got {}",
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
            ("java/util/Arrays$ArrayList", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/Collections", "synchronizedMap", "(Ljava/util/Map;)Ljava/util/Map;"),
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
    #[test]
    fn the_held_collection_families_are_not_retired() {
        for (c, m, d) in [
            // needs-VM-support: state is not real.
            ("java/util/TreeMap", "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/TreeSet", "add", "(Ljava/lang/Object;)Z"),
            ("java/util/ArrayDeque", "addLast", "(Ljava/lang/Object;)V"),
            ("java/util/concurrent/ConcurrentHashMap", "put",
             "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/Hashtable", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/LinkedHashSet", "add", "(Ljava/lang/Object;)Z"),
            ("java/util/HashMap", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/HashSet", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/LinkedList", "add", "(Ljava/lang/Object;)Z"),
            // `size` and `get` came OFF this list on 2026-08-17 — see
            // `the_two_rows_g60_1_asked_about`. `iterator` stays, and the
            // measurement that separates them is in the module docs: with the
            // whole class dialled to yield, `values().iterator()` after a `put`
            // throws `ConcurrentModificationException` from real
            // `ArrayList$Itr.checkForComodification`.
            ("java/util/ArrayList", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/ArrayList$Itr", "next", "()Ljava/lang/Object;"),
            // Load-bearing FOR the retirements above.
            ("java/util/Arrays", "copyOf", "([Ljava/lang/Object;I)[Ljava/lang/Object;"),
        ] {
            assert!(!triple_is_retired_shadow(c, m, d), "wrongly retired: {c}.{m}{d}");
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

    /// G60-1 §5's two `java/util/ArrayList` nominations, and the boundary next
    /// to them.
    ///
    /// The pair is retired; `iterator` is not, and that split is the finding
    /// rather than an oversight. With the whole class dialled to yield,
    /// `Map.values().iterator()` after a `put` throws
    /// `ConcurrentModificationException` out of real
    /// `ArrayList$Itr.checkForComodification`, while `size()`/`get()` retired on
    /// their own are byte-identical to HotSpot across 42 checks. A later wave
    /// that reaches for `iterator` needs its own per-triple trial and must not
    /// read this test as permission.
    #[test]
    fn the_two_rows_g60_1_asked_about() {
        assert!(triple_is_retired_shadow(
            "java/util/ArrayList",
            "get",
            "(I)Ljava/lang/Object;"
        ));
        assert!(triple_is_retired_shadow("java/util/ArrayList", "size", "()I"));
        assert!(!triple_is_retired_shadow(
            "java/util/ArrayList",
            "iterator",
            "()Ljava/util/Iterator;"
        ));
    }

    /// `Properties.getProperty` stays a `Bridge`, and the reason is a receiver
    /// rather than the method.
    ///
    /// Not a restatement of the table's absence: this is the guard against a
    /// later wave adding these two because "the real bytecode is String-keyed
    /// JDK code and correct by construction" — which is TRUE, and still breaks
    /// `System.getProperties().getProperty(...)`, because the object that call
    /// runs on is built by a native that never initialises the real `map` field
    /// JDK 25's `getProperty` reads. Retire them in the same change that makes
    /// that receiver real, or not at all. Module docs carry the transcript.
    #[test]
    fn properties_get_property_is_held_for_the_system_receiver() {
        for d in [
            "(Ljava/lang/String;)Ljava/lang/String;",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        ] {
            assert!(
                !triple_is_retired_shadow("java/util/Properties", "getProperty", d),
                "retiring getProperty{d} NPEs System.getProperties(); see the module docs"
            );
        }
    }
}
