# W7-56 — `--jdk-only`: the source pair was null because a shadow CONSTRUCTOR dropped `needToInferCaller`

| | |
|---|---|
| **Status** | **CLOSED BY MEASUREMENT 2026-08-12 (A34). Built, run, green in all three arms.** The "still unbuilt" status this row used to carry is retired: `scratchpad/bin/cratonvm-merged-dev.exe` (15:27) carries all three parts, and every row of §"What to rebuild, and what the probes must become" now reads its **must become** value. See §8. |
| **Vector** | `regression-suite/src/RJdkLogging.java`, `formattedOutputIsRealBytes` — the last red in the `--jdk-only` strict corpus (69 passed / 1 failed). |
| **Predecessor** | jdk-only-jul-logrecord-infercaller-SUPERSEDED-20260812.md (the handoff; both of its candidate causes are refuted below). It is no longer in this directory — it was retired to the internal tree on 2026-08-12 and carries a SUPERSEDED marker pointing here. |
| **Oracle** | HotSpot 25.0.3.9 renders `RJdkLogging formattedOutputIsRealBytes`. |

## The symptom

```
AssertionError: SimpleFormatter must render the inferred source class and method;
got [Aug 12, 2026 4:19:43 AM rjdklogging.stream
```

`rjdklogging.stream` is the LOGGER NAME — `SimpleFormatter`'s documented
fallback when a record carries no source pair. Not a formatting bug.

## The cause, in one line

`LogRecord.getSourceClassName()` is
`if (needToInferCaller) inferCaller(); return sourceClassName;`. A shadow
**constructor** never set `needToInferCaller`, so the real getter read `false`
and never called `inferCaller()`. Everything downstream of that flag works.

## What was eliminated, and how

Four candidate causes were proposed across two sessions. All four are refuted
by measurement, not by reading.

| Candidate | Verdict | Evidence |
|---|---|---|
| The setters do not stick | **refuted** | SrcProbe3 A: `A_CLASS / a_method`; D: same on a record the LOGGER built, read inside `publish` |
| `StackWalker` omits the `Logger` frames | **refuted** | SrcProbe3 C under `--jdk-only` is byte-identical to HotSpot's frame list |
| `CallerFinder` cannot find the caller at `inferCaller`'s depth | **refuted** | SrcProbe4 runs JDK 25's `CallerFinder` verbatim from a Formatter under a real `StreamHandler.publish` and returns `SrcProbe4 main` |
| `class_manager.rs` MINTS a synthetic `LogRecord` whose methods are all `PUBLIC \| NATIVE` | **refuted** | SrcProbe5, printed not inferred (below) |

### The mint is not active — printed, not inferred

`classloading/src/class_manager.rs` `synthetic_stub_ctor_methods` does mint a
`java/util/logging/LogRecord` with 24 body-less `PUBLIC | NATIVE` methods. Both
of its call sites (`class_manager.rs:4028`, `:9118`) are class-FABRICATION
paths, reached only when a class has no real bytes. Under `--jdk-only` with a
real JDK, `LogRecord` resolves from the jimage and the mint never runs.
SrcProbe5 prints it:

```
                       methods  fields  nativeDeclaredMethods  needToInferCaller  inferCaller
HotSpot                     33      17                      0            PRESENT     DECLARED
CratonVM --jdk-only         33      17                      0            PRESENT     DECLARED
(the mint would be)         24       -                     24             ABSENT       ABSENT
```

`module=java.logging`, `loader=null`, `CallerFinder LOADABLE` on both. The real
class file is what loads. Same for `Logger` (84 methods / 20 fields on both).

### The accessor retirement DID take — the census kind was misread

The `--jdk-only` census lists refusals and live shadows under *different* kinds,
and both are "violations":

* `"kind":"native-shadows-bytecode"`, with `native_kind` — a native that is
  REGISTERED and DISPATCHING over real bytecode. This is what the four
  accessors reported before the retirement.
* `"kind":"synthetic-native-registered"`, with `registered_by` — a
  `JdkOnlyViolation::SyntheticNativeRegistered`, pushed by `register_inner`
  on the path that "Return[s] WITHOUT inserting". This is a REFUSAL record.
  This is what the four accessors report after the retirement.

The transition between those two kinds is the proof the retirement landed. The
VM will also say it outright — `CRATONVM_DBG_DROPPED_STUBS=1`:

```
[JDK-ONLY-REFUSED] java/util/logging/LogRecord.getSourceClassName()Ljava/lang/String;
[JDK-ONLY-REFUSED] java/util/logging/LogRecord.setSourceClassName(Ljava/lang/String;)V
[JDK-ONLY-REFUSED] java/util/logging/LogRecord.getSourceMethodName()Ljava/lang/String;
[JDK-ONLY-REFUSED] java/util/logging/LogRecord.setSourceMethodName(Ljava/lang/String;)V
```

So the real lazy getter WAS running, on the merged binary, and the pair was
still null.

## The measurement that located it

`--add-opens` makes `needToInferCaller` readable, and it is the only line that
separates "the getter did not run" from "the getter ran and the flag was
false":

```
                        A4 fresh needToInferCaller     A4 fresh sourceClassName
HotSpot                                       true                         null
CratonVM --jdk-only                          FALSE                         null
CratonVM --real-jdk                          FALSE                         null
```

JDK 25's constructor ends `needToInferCaller = true` and assigns
`sequenceNumber = globalSequenceNumber.getAndIncrement()`. The shadow in
`register_phase54_logging_extras` writes neither.

**CORRECTED 2026-08-12 — the launcher parses BOTH spellings correctly.** This
record originally said `--add-opens M/P=T -cp DIR Main` made the launcher
swallow the `-cp`, and told readers to prefer the `=` spelling. That is wrong.
Re-measured on the then-current binary AND on the pre-merge control
`7d4d545e0`: all eight combinations of {`--add-opens`, `--add-exports`,
`--add-reads`, `--add-modules`} x {space, `=`} run the main class, and neither
spelling ever produced "Could not find or load main class" on either binary.
`normalize_java_launcher_argv` rewrites `-cp` to `--classpath` before clap sees
it, and every `--add-*` flag is in `VALUE_TAKING_OPTS`.

What almost certainly happened is the ordinary one: the classpath directory did
not yet hold the compiled probe when that first pass ran, and "Could not find or
load main class" — which is exactly what an empty classpath produces — was
attributed to the flag spelling standing next to it. The investigation did lose
a pass; the cause was not the launcher.

Pinned by `add_star_flags_accept_both_spellings_without_eating_the_next_arg` in
`vm-cli/src/main.rs`, which asserts the flag's own value AND that `-cp` and the
main class survive. Mutation-checked: injecting a `--add-opens` arm that
swallows the following token turns it red.

## Why the table entry for the constructor was inert

`("java/util/logging/LogRecord", "<init>", "(Ljava/util/logging/Level;Ljava/lang/String;)V")`
has been in `RETIRED_SHADOW_TRIPLES` since the 2026-08-11 wave. It did nothing,
because the retag arm in `NativeMethodRegistry::register` fires only when
`effective_category() == NativeKind::Bridge`, and the ambient category of
`register_phase54_logging_extras` is `Intrinsic`.

The triple is registered **twice**. Under `--jdk-only` the `Bridge` one
(`native-builtins/src/lib.rs`) is refused, and the `Intrinsic` one
(`phases_early.rs`) silently owns the slot. That is why retiring the first
measured verdict-neutral on 08-11: nothing changed, because the other one kept
running.

> **A refusal list is not a "this triple has no native" list.** The
> `[JDK-ONLY-REFUSED]` output above also names `<init>`, `getLevel`,
> `getMessage` and `getSequenceNumber` — every one of which still has a LIVE
> `Intrinsic` registration in `phases_early.rs`. The 08-11 wave's "84 triples
> retired" over-counts for `LogRecord` by exactly this mechanism.

This is the **fourth** instance of the ambient-category defect on this one
function's JUL rows: W7-25 lifted the two `LogManager` rows, W7-35 lifted
`Formatter.formatMessage`, W7-56 lifted the four source accessors, and now the
constructor.

## The fix

1. **`native-api/src/retired_shadow.rs`** — the four source accessors added to
   `RETIRED_SHADOW_TRIPLES`, as a SET of four. The shadow setter omits the real
   setter's `needToInferCaller = false`, so retiring only the getters would let
   an explicit `setSourceClassName("X")` be silently overwritten by the inferred
   caller on the next read. A test pins the set.
2. **`native-builtins/src/phases_early.rs`** — the constructor lifted out of the
   ambient `Intrinsic` to `Bridge`, via the explicit set/restore idiom this file
   already uses at line 101, so the existing table entry takes effect and the
   real constructor runs under strict. The shadow also now mirrors
   `needToInferCaller = true` for its remaining Compatible-mode life.
3. **`native-builtins/src/logmanager.rs`** — `stamp_inferred_caller` clears
   `needToInferCaller` alongside its two field writes, so a stamped record
   cannot be re-inferred over by the now-live real getter.

## Landed state, and the registrar census that decides it

Re-verified in the tree on 2026-08-12 by grepping every registration of every
triple in the fix, because that is the check this record's own history says
cannot be skipped. Nothing was built.

| part | site | present |
|---|---|---|
| 1, the four accessors | `native-api/src/retired_shadow.rs`, table entries + `the_log_record_source_pair_is_retired_as_a_set` | yes; table is 88 entries, floor `>= 80` |
| 2, the `<init>` retag | `native-builtins/src/phases_early.rs`, `let __ctor_cat = r.current_category(); r.set_category(…Bridge); … r.set_category(__ctor_cat);` | yes |
| 3, the flag clear | `native-builtins/src/logmanager.rs`, `stamp_inferred_caller` | yes, guarded on `log_record_real_layout` |

**The four accessors have exactly ONE registrar**, tree-wide:
`register_phase54_logging_extras`, inside an explicit
`r.with_category(NativeKind::Bridge, …)`. So the retirement retag fires and
nothing can hand them back — the failure mode this record's own §"Why the table
entry for the constructor was inert" describes cannot recur for them.

**`LogRecord.<init>(Level,String)V` has THREE**, and only two of them ship:

| registrar | file | ambient kind | ships? |
|---|---|---|---|
| `register_phase54_logging_extras` | `phases_early.rs` | explicit `Bridge` (the part-2 retag) | yes — called from `register_essential_natives_with_shims` AND from `register_phase54_natives` |
| `register_essential_natives_with_shims`, inline | `native-builtins/src/lib.rs` | `Bridge` (ambient, set once near the head and restored to itself around the regex block) | yes |
| `register_p71_logging_extras` | `native-builtins/src/phases_late.rs` | ambient of `register_phase71_natives` | **no** — reached only from `register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]` and runs only on the `use_synthetic_jdk` arm |

Both shipping registrations are `Bridge`, the triple is in
`RETIRED_SHADOW_TRIPLES`, so both are retagged `SyntheticStub` and **both are
refused under `--jdk-only`** — `register_inner`'s refusal arm returns before the
push, so the slot is never created and the real constructor bytecode runs. That
is what makes `needToInferCaller` true on a fresh record, and it is the whole
closure. This does not depend on registrar ORDER, which is the reason to prefer
it to any ordering argument.

### Correction to fix item 2's Compatible sentence

Item 2 says the shadow "also now mirrors `needToInferCaller = true` for its
remaining Compatible-mode life". **In `Compatible` that write never executes.**
The `phases_early.rs` registration is made at `register_essential_natives_with_shims`'
call to `register_phase54_logging_extras`, and the `lib.rs` inline registration
of the same triple is made LATER IN THE SAME FUNCTION BODY — `register()` is
last-write-wins on the callback, so the surviving Compatible body is `lib.rs`'s,
and that one writes `needToInferCaller` to **`Int(0)`**.

Nothing is broken by it: in `Compatible` all four source accessors are still
natives and none of them reads the flag, so `0` and `1` are indistinguishable
there, and `stamp_inferred_caller` has already filled the pair in. The mirror is
worth keeping as documentation of what the real constructor does. But the
sentence as written asserts a live effect the tree does not have, and the same
misreading — "my registration is the one that runs" — is what made the `<init>`
table entry inert for a day. **Two registrations in one registrar function are
ordered by line, and the later line wins.**

## Why Compatible cannot regress

* A `SyntheticStub` registers and dispatches normally in `Compatible`; only
  `--jdk-only` refuses it. Nothing in this change alters registration order or
  which of the two constructor registrations wins there.
* Measured, and this is the load-bearing one: SrcProbe4 returns **`EMPTY`**
  under `--real-jdk`, because the native `Logger` chain leaves no
  `java.util.logging.Logger` frame to trip `CallerFinder`'s latch. Compatible is
  correct *only* via `stamp_inferred_caller`'s eager stamp plus the shadow
  getter. A mode-symmetric retirement would have taken Compatible from
  `SrcProbe4 main` to null. The retirement table is per-mode by construction.

## What to rebuild, and what the probes must become

Rebuild `dev` with this branch merged, then, with
`--add-opens=java.logging/java.util.logging=ALL-UNNAMED` and `--jdk-only`:

| Probe | line | today | must become |
|---|---|---|---|
| SrcProbe3 | `A4 fresh needToInferCaller` | `false` | **`true`** |
| SrcProbe3 | `B during-publish` | `null / null` | **`SrcProbe3 / main`** |
| SrcProbe4 | formatted line | `srcprobe4.one` | **`SrcProbe4 main`** |
| SrcProbe5 | `B during-publish` | `null / null` | **`SrcProbe5 / main`** |
| RJdkLogging | `formattedOutputIsRealBytes` | fails | **passes** |

`A4 fresh needToInferCaller=true` is the one to read first: if it is still
`false`, the constructor retag did not take and nothing downstream matters.
Under `--real-jdk`, SrcProbe3 `B` must still read `SrcProbe3 / main` and
SrcProbe4's formatted line must still read `SrcProbe4 main` — that is the
Compatible non-regression check.

If the real constructor misbehaves under strict, the symptom will be broad
(every `LogRecord` construction), not this one assertion — the narrow fallback
is to revert item 2's retag and keep only its `needToInferCaller = true` write,
which reaches the same flag through the shadow. That fallback is strictly worse
(the shadow still drops `sequenceNumber`) and should not be the first move.

## Gates

* `scripts/baselines/jdk-only-kind-map-25-linux.tsv` — the four accessor rows
  amended by hand and disclosed in the header. **The constructor row now moves
  too**: `LogRecord.<init>` ordinal 0 goes `intrinsic 0 1` → `synthetic-stub 1 1`,
  amended here on the same basis.
* `scripts/baselines/jdk-only-bridge-ratchet.json` — no edit. Its gates are
  `<=` with `SLACK = 0`, and retiring shadows moves the bridge counts DOWN.
* **Pre-existing drift a linux re-freeze still owes:** `3b20b83b5` retagged six
  rows `Bridge` at 22:34 on 2026-08-11 and this baseline was frozen at 12:20 the
  same day, so `Formatter.formatMessage` and its neighbour still read
  `intrinsic` where the tree produces `bridge`.
* `RETIRED_SHADOW_TRIPLES` vacuity floor stays `>= 80` (84 → 88).

## Left open, deliberately

* `getLevel`, `getMessage`, `getSequenceNumber` on `LogRecord` are in the
  retirement table and still shadowed by live `Intrinsic` registrations in
  `phases_early.rs`, exactly like the constructor was. They are not retired here
  because they currently answer correctly and this vector does not implicate
  them — but the table says something about them that is not true today.
* ~~CratonVM's launcher mis-parses the space-separated `--add-opens M/P=T`
  form, swallowing the following argument.~~ **RETRACTED 2026-08-12.** It does
  not; see the correction above. Both spellings were re-measured on this binary
  and on the pre-merge control, for all four `--add-*` flags, and a regression
  test now pins it.

## 8. 2026-08-12 (A34) — the rebuild happened. Every predicted row is green.

The binary this record asks for exists: `scratchpad/bin/cratonvm-merged-dev.exe`,
built 15:27 on 2026-08-12, carrying all three parts. Run against Microsoft JDK
25.0.3.9 with `--add-opens=java.logging/java.util.logging=ALL-UNNAMED`, one
binary per arm with only the mode flag differing, HotSpot beside them.

**The row this record says to read first is `true`.**

| row | HotSpot | `--real-jdk` | `--jdk-only` | record's prediction |
|---|---|---|---|---|
| fresh `LogRecord` `needToInferCaller` | `true` | `false` | **`true`** | *"must become `true`"* — met |
| fresh `getSourceClassName()` | `null` | `null` | `null` | unchanged, correct (lazy) |
| fresh `getSequenceNumber() >= 0` | `true` | `true` | `true` | — |
| `SimpleFormatter` during-publish | `SrcProbe6 main` | `SrcProbe6 main` | **`SrcProbe6 main`** | *"must become `<Class> / main`"* — met |

The formatted line renders the **inferred source pair** and not the logger name,
which is the whole symptom: `rjdklogging.stream` was `SimpleFormatter`'s
documented fallback for a record carrying no source pair, and it is gone.

**The vector is green, and it is scheduled.** `RJdkLogging` — *"the last red in
the `--jdk-only` strict corpus (69 passed / 1 failed)"* — now reports
`PASS RJdkLogging (79 checks)` on HotSpot, `--real-jdk` and `--jdk-only` alike.
Unlike most evidence in this directory this record's instrument is a
`regression-suite/src` vector rather than a `probes/` file, so `run.sh` re-runs
it at every `SUITE=` and the closure cannot silently rot.

**The Compatible non-regression check passes, and it passes in the specific
shape §"Correction to fix item 2's Compatible sentence" predicted.**
`--real-jdk` reads `needToInferCaller = false` on a fresh record while
`--jdk-only` reads `true`. That is not a defect and it is not asymmetry for its
own sake: in Compatible the `lib.rs` inline registration is made later in the
same function body and `register()` is last-write-wins, so the surviving body is
the one writing `Int(0)`; all four source accessors are still natives there and
none reads the flag, so `0` and `1` are indistinguishable, and
`stamp_inferred_caller` has already filled the pair in. **The correction
predicted a value that a naive reading would have called a bug, and the run
produced exactly it** — which is the strongest form of confirmation available
here, and worth more than the green.

**What is NOT closed by this run**, and it is the item §"Left open,
deliberately" already names: `getLevel`, `getMessage` and `getSequenceNumber`
remain in `RETIRED_SHADOW_TRIPLES` while live `Intrinsic` registrations in
`phases_early.rs` still shadow them, exactly as the constructor did. They answer
correctly today and this vector does not implicate them, so nothing here is
red — but the table still says something about them that is not true, and the
mechanism that made the `<init>` entry inert for a day is unchanged for these
three. That is a table-accuracy defect, not a behavioural one, and it is the
residual this record should be retired against.
