# W7-56 — `--jdk-only`: the `LogRecord` source pair was null because the shadow getter deleted `inferCaller()`

| | |
|---|---|
| **Status** | FIXED on `fix/infercaller-strict-source-pair-20260812`, pending the orchestrator's build. Compatible untouched by construction. |
| **Vector** | `regression-suite/src/RJdkLogging.java`, `formattedOutputIsRealBytes` — the last red in the `--jdk-only` strict corpus (69 passed / 1 failed). |
| **Predecessor** | `jul-logrecord-infercaller-is-inert-under-jdk-only-20260812.md` (the handoff; both of its candidate causes are refuted below). |
| **Oracle** | HotSpot 25.0.3.9 renders `RJdkLogging formattedOutputIsRealBytes`. |

## The symptom

```
AssertionError: SimpleFormatter must render the inferred source class and method;
got [Aug 12, 2026 4:19:43 AM rjdklogging.stream
```

`rjdklogging.stream` is the LOGGER NAME — `SimpleFormatter`'s documented
fallback when a record carries no source pair. Not a formatting bug: a record
arrived with a null pair and the formatter papered over it.

## Neither candidate cause. Measured, not read.

The handoff offered two, and warned they produce identical nulls. They do, so
`probes/SrcProbe3.java` measures all three arms on one binary:

| | explicit set/get | pair during publish | `StackWalker` frames at publish |
|---|---|---|---|
| HotSpot | `A_CLASS / a_method` | `SrcProbe3 / main` | `Logger.log, doLog, log, warning, SrcProbe3.main` |
| CratonVM `--real-jdk` | `A_CLASS / a_method` | `SrcProbe3 / main` | *(chain is native: no `Logger` frame)* |
| CratonVM `--jdk-only` | `A_CLASS / a_method` | **`null / null`** | **identical to HotSpot** |

* **Candidate (2) refuted.** The setters stick — and not only on a record the
  probe built: section D sets and reads the pair back on a record the LOGGER
  built, inside `publish`, and gets `D_CLASS / d_method`. `log_record_real_layout`
  answers `true` for a bytecode-constructed record; the writes land in the real
  fields.
* **Candidate (1) refuted.** Our `StackWalker` presents
  `LogRecord$CallerFinder` exactly the frame list HotSpot's walks, `Logger.log`
  and `doLog` included.

## The actual cause

Nothing was broken. The code that would have CALLED the working mechanism never
ran. `native-builtins/src/phases_early.rs` registers

```
java/util/logging/LogRecord.getSourceClassName  ()Ljava/lang/String;
java/util/logging/LogRecord.getSourceMethodName ()Ljava/lang/String;
java/util/logging/LogRecord.setSourceClassName  (Ljava/lang/String;)V
java/util/logging/LogRecord.setSourceMethodName (Ljava/lang/String;)V
```

and the getters are JDK 25's getters with the `inferCaller()` call deleted:

```java
getSourceClassName() { if (needToInferCaller) inferCaller(); return sourceClassName; }   // real
getSourceClassName() { return sourceClassName; }                                          // ours
```

Under `--jdk-only` the real ctor runs and sets `needToInferCaller = true`; then
a bare field read answers `null` forever. The VM's own census names it without
ambiguity — `cratonvm --jdk-only --jdk-only-report`:

```
"kind":"native-shadows-bytecode",
"native_kind":"bridge-ran-over-bytecode",
"class":"java/util/logging/LogRecord","method":"getSourceClassName"
```

**A `Bridge` retag retires nothing.** `3b20b83b5` (2026-08-11 22:34) retagged
these four `Bridge` "so the retirement can see them" and added no entry to
`RETIRED_SHADOW_TRIPLES`. `NativeMethodRegistry::register` only re-tags a
`Bridge` row to `SyntheticStub` when the TABLE names the triple, so the retag
bought a census line and changed no dispatch. The `Bridge` tag was still a
necessary precondition — the retag arm is gated on
`effective_category() == NativeKind::Bridge`, and the ambient category of
`register_phase54_logging_extras` is `Intrinsic`, which is exempt.

## The fix: retire, do not improve

Four rows added to `RETIRED_SHADOW_TRIPLES` in
`native-api/src/retired_shadow.rs`. Under `--jdk-only` the natives are refused
and the real lazy-inference bytecode runs.

**All four or none.** The shadow setter is the real setter with
`needToInferCaller = false` deleted. Retire only the getters and the real getter
starts honouring a flag the surviving shadow setter never clears, so an explicit
`setSourceClassName("X")` is silently overwritten by the inferred caller on the
next read — strictly worse than retiring neither. A test,
`the_log_record_source_pair_is_retired_as_a_set`, pins that.

One companion change: `stamp_inferred_caller` (`native-builtins/src/logmanager.rs`)
wrote `sourceClassName`/`sourceMethodName` by name and left `needToInferCaller`
alone. Inert while the getter was a bare field read; a live hazard once the real
lazy getter runs, because it would re-infer over the stamp. It now clears the
flag on real-layout records. Every `java/util/logging/` entry point into that
file is itself retired in strict mode, so the reachable route today is a non-JUL
receiver (the `org/jboss/logmanager/` bridges are not in the retirement table);
the flag is written rather than the reachability relied on.

## Why the fix works, measured before the build

`probes/SrcProbe4.java` transcribes JDK 25's `LogRecord$CallerFinder` verbatim
and runs it from `inferCaller`'s REAL depth — inside a `Formatter` invoked by a
real `StreamHandler.publish`, not at `Handler.publish` depth where SrcProbe3
sampled. Whatever it returns is what the retired-shadow build stamps:

| | `CallerFinder` returns | `SimpleFormatter` renders |
|---|---|---|
| HotSpot | `SrcProbe4 main` | `SrcProbe4 main` |
| CratonVM `--real-jdk` | `EMPTY` | `SrcProbe4 main` |
| CratonVM `--jdk-only` | **`SrcProbe4 main`** | **`srcprobe4.one`** |

The `--jdk-only` row is the whole case in one line: on today's unfixed binary
the walk **already finds the caller**, and the record is still formatted with
the logger name, because the shadow getter never runs the walk. Retire the
shadow and the two columns agree. `RJdkLogging.formattedOutputIsRealBytes` is
`l.warning("BYTES-MARK-ONE")` called directly from that method — SrcProbe4's
exact shape — so it will render `RJdkLogging formattedOutputIsRealBytes`.

## Why Compatible cannot regress

Two independent reasons, and the second is the interesting one.

1. **Mechanically.** A `SyntheticStub` registers and dispatches normally in
   `Compatible`; only `--jdk-only` refuses it. All four natives still answer in
   Compatible exactly as they did, over records `stamp_inferred_caller` filled
   at construction. The `needToInferCaller` write is inert there: none of the
   four accessors reads that flag, and if real bytecode ever did run the getter
   in Compatible the write makes it return the stamped value, which is the
   desired one.
2. **Why a mode-symmetric retirement would have been a REGRESSION.** The
   `--real-jdk` row above measures `CallerFinder = EMPTY`. In Compatible the
   whole `Logger.warning → publish` chain is native, so no
   `java.util.logging.Logger` frame exists to trip `CallerFinder`'s latch and
   the real `inferCaller()` returns nothing. Compatible is byte-identical to
   HotSpot today *only* because the bridge stamps eagerly and the shadow getter
   reads the stamp back. Retiring these rows in both modes would have taken
   Compatible from `SrcProbe4 main` to null. The two modes reach this record by
   genuinely different routes and the fix has to be per-mode; the retirement
   table is per-mode by construction.

## Gates

* `scripts/baselines/jdk-only-kind-map-25-linux.tsv` — the four rows amended by
  hand (`intrinsic 0 1` → `synthetic-stub 1 1`), derived from the deterministic
  retag rather than guessed, and disclosed in the file's header. **Pre-existing
  drift this does not fix:** `3b20b83b5` retagged six rows at 22:34 on
  2026-08-11 and the baseline was frozen at 12:20 the same day, so two rows
  (`Formatter.formatMessage` and its neighbour) still read `intrinsic` where the
  tree produces `bridge`. A linux re-freeze is owed for those.
* `scripts/baselines/jdk-only-bridge-ratchet.json` — no edit. Its gates are
  `<=` with `SLACK = 0`, and retiring four bridge-shadows-bytecode rows moves
  `bridge.without_acc_native` and `bridge.shadows_bytecode_anywhere` DOWN by
  four.
* `scripts/baselines/jdk-only-strict-corpus-25-linux.txt` — unaffected;
  `RJdkLogging` is not in that probe set.
* `RETIRED_SHADOW_TRIPLES` vacuity floor stays `>= 80` (the table goes 84 → 88).

## Left open, deliberately

`java/util/logging/Formatter.formatMessage` is the sixth row `3b20b83b5`
retagged `Bridge`, and it is still a live shadow under `--jdk-only`. Its
behaviour was repaired in place (`8eb60d3c1`) rather than retired, and
`RJdkLogging.recordPayloads` is green in both modes, so it is not this vector's
business. `W7-35-jul-supplier-and-payload-residuals.md` already argues retiring
that row is the strictly better end state; it needs its own measurement, not a
ride on this one.
