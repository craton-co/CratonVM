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
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "java/util/concurrent/ConcurrentHashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
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
