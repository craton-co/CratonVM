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
//! # `java/util` collections — adjudicated 2026-08-12, EIGHT of 68 retirable, HELD
//!
//! A `--jdk-only --explain-jdk-only` run reported 226 `native-shadows-bytecode`
//! rows actually taken; 62 of them are `java.util` collections triples, 68
//! registrations by the frozen kind map. All 68 were adjudicated against the
//! rule this module's header states — *a class's state has to become real
//! before its shadow can be retired* — in
//! docs/known-issues/jdk-only/P2-COLLECTIONS-SHADOWS-20260812.md. **Eight are
//! retirable. Sixty are not, and most of them never will be by this route.**
//!
//! The eight, in the sorted position they would take (at the HEAD — every
//! `java/util/A…`, `C…` and `H…` key sorts before `java/util/logging/`):
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
//! ## What holds it, and it is not doubt about the eight
//!
//! Two things, and the first is the `java/io/PrintWriter` hold above, verbatim:
//! `java/util` is Compatible-visible, so eight rows moving
//! `Bridge` -> `SyntheticStub` move `BASELINE_SYNTHETIC_STUBS_*` (`SLACK = 0`),
//! `bridge_shadows_bytecode` and the per-row kind freeze, all three keyed
//! `25/linux`, all three re-frozen from ONE Linux census in the SAME commit.
//! Both baselines are ALREADY stale and say so in their own bodies, so the
//! predicted deltas (+8 on the stub ratchet, -8 on four bridge counters, exactly
//! eight kind-map rows) are deltas on a base nobody has taken. The derivation is
//! §4 of the P2 record; **it is a diff to check, not a number to paste.**
//!
//! The second is new, and is the reason this is held rather than merely
//! sequenced. **The acceptance measurement has not been run, and it needs no
//! source edit at all.** `CRATONVM_ENFORCE_NATIVE_SHADOW` takes a prefix list
//! and yields on the same §1.4 predicate a retirement uses, so
//! `=java/util/ArrayList,java/util/Arrays$ArrayList,java/util/Collections`
//! under `--jdk-only` simulates this exact change with no rebuild — the way the
//! `java/util/logging/` wave was accepted. Three negative controls
//! (`java/util/TreeMap`, `java/util/ArrayDeque`,
//! `java/util/concurrent/ConcurrentHashMap`) must go RED in the same session; if
//! one comes back verdict-neutral, the state-model reading above is wrong for
//! that family and the eight are not safe either.
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
//! ## Widening the discriminator is half of it
//!
//! [`triple_is_retired_shadow`] answers `false` for anything outside
//! `java/util/logging/`. All seven keys are under `java/util/`, so ONE prefix
//! covers both populations — but an entry added without that widening reads as
//! "not retired" and is invisible. `every_entry_is_reachable_through_the_predicate`
//! is the test that catches it, and it already works; no new test is needed.
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
/// `java/util/logging/`, and almost no registration is, so the common case
/// costs one prefix compare and nothing else.
pub fn triple_is_retired_shadow(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    if !class_name.starts_with("java/util/logging/") {
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
    /// retire nothing — the 2026-08-11 measurement recorded 84 triples, and the
    /// 2026-08-12 source-pair retirement added four.
    #[test]
    fn the_table_is_not_empty() {
        assert!(
            RETIRED_SHADOW_TRIPLES.len() >= 80,
            "expected the measured java.util.logging population (88), got {}",
            RETIRED_SHADOW_TRIPLES.len()
        );
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
}
