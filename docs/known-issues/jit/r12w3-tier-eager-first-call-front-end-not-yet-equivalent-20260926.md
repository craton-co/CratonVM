# The eager first-call door's own front end is not yet equivalent to the wrapped-entry door

Status: OPEN (measurement arm landed; no code left before the measurement; exact runs and stop conditions in the round 13 wave 8 section)
Area: `vm/src/runtime/interpreter.rs` (`execute`, the eager first-call door, ~2050-3360: `compile_gate::admit(.., EagerFirstCall, ..)` through `compile_with_param_slots` and the `first-compile` census), `vm/src/runtime/interpreter/jit_bridge.rs` (`try_jit_compile_wrapped_entry` -> `try_jit_compile_callee_slow` -> `jit::try_compile`)
Severity: LOW-MEDIUM (maintenance: a second, ~1 300-line front end that keeps missing what `try_compile` learns; reached only under `CRATONVM_BG_COMPILE=0` with `CRATONVM_JIT_C2_FIRST_CALL` unset)
Found by: round 12 wave 3 lane tier (tier proposal W2-1)

## What was asked

Retire the door's own front end if the wrapped-entry door (which it already
uses for methods with handlers and for category-2 parameters, round 12 wave 2)
covers it. Prove equivalence first.

## Why it is not retired

The two are not equivalent, and the differences are not all in the new door's
favour. Read side by side on `a35b85a27`:

| | eager door (`compile_with_param_slots`) | wrapped-entry door (`try_compile`, single-pass) |
|---|---|---|
| handler tables, precise frames | staged since interpreter round i1 wave 28 by the shared `BackendRequest::stage_exception_table`, RBC.6 through the shared `precise_handler_frame_blocking_site` (a method with a table reaches it only under `CRATONVM_JIT_EAGER_ORDINARY_DOOR=0`); none before | staged, RBC.6 gate |
| parameter layout | one slot per argument (`&[]`, `0`) | by JVM slot |
| `string_layout` | `None` (no String intrinsics) | resolved |
| branch / unroll hints | empty maps | from the profile (empty under `BG_COMPILE=0` at call 1 anyway) |
| static method, class still initializing (JVMS 5.5) | compiles | refuses; the route defers without a seal (`eager_deferred`) |
| `ldc` MethodHandle / MethodType / condy / wide string | seals `early-unsupported-ldc` for good | whatever `try_compile` does (not sealed here) |
| `athrow` with handlers under `CRATONVM_JIT_EAGER_ORDINARY_DOOR=0` | seals `early-rbc6-athrow-with-handler` | n/a |
| nested callee compiles | none | the eager callee chain (`MAX_EAGER_CALLEE_CHAIN_DEPTH` 6, `_PER_COMPILE` 96) |
| inlining at C1 | none | `plan_inline` over the (empty) profile |
| registered-native / FJP policy refusals | not asked (the `native_skip` / `fjp_skip` seals before it) | asked again, `policy_declined` |
| census | `[cratonvm-jitc] first-compile`, JFR tier 2 constant | the slow door's census and JFR event |
| compile cost per first call | one backend walk | `try_compile`'s full front end plus the callee chain |

The last row is the one that decides it. Under `CRATONVM_BG_COMPILE=0` this
door compiles EVERY method on its first call (there is no invocation
threshold), so the per-compile work of `try_compile` and the callee chain is
paid for every method a program touches. The proposal's own risk line says to
measure startup first, and this lane cannot run anything.

## What landed (round 12 wave 3)

`CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1` (default OFF, `interpreter.rs`
`eager_door_routes_every_method`) widens the wave-2 route to every method: the
door hands every method to `try_jit_compile_wrapped_entry(optimize = false)`,
under the same deferral and seal rules the route already has. With it unset
nothing changes.

## How to finish it

1. Measure, three interleaved runs each, `CRATONVM_BG_COMPILE=0` with and
   without `CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1`: startup of a Spring-sized
   classpath (or `CratonBench` start-to-first-iteration), the R11/R12 probe
   battery (answers must be identical), and the seal census under
   `CRATONVM_DBG_JITC=1` (`early-*` seals should vanish; `early-backend-bail`
   should not grow).
2. If startup is within noise: delete the door's front end (from the
   `compile_gate::admit(.., CompileDoor::EagerFirstCall, ..)` call to the end
   of the `or_else` closure), make the route unconditional, and retire
   `CompileDoor::EagerFirstCall` if nothing else names it
   (`rg -n "EagerFirstCall" jit/src vm/src`).
3. If startup regresses: keep the route for the table-needing populations only
   (today's default) and consider an invocation threshold at this door instead,
   which is what the default background pipeline already has.

Note: the `cfg!(not(target_arch = "x86_64"))` early return sits before the
route, so an aarch64 build compiles nothing here even with the switch. A full
retirement would move that return below the route (the route selects the
backend through `try_compile`).

## Round 12 wave 5 (lane tier3)

Re-read on `bb1addd1a`; the decision stands: NOT routed. Nothing in the
table above moved in the eager door's favour or against it:

- Wave 4's single-pass unsound-replay refusal
  (`r12w4-replay2-single-pass-unsound-replay-trap-patch`) lives in
  `x64::driver::compile_with_param_slots`, which both front ends reach, so it
  is not a new difference.
- The decisive row is still compile cost per first call. Under
  `CRATONVM_BG_COMPILE=0` this door compiles every method on its first call,
  and `try_compile` adds `plan_inline` and the eager callee chain (up to 96
  nested compiles per compile, depth 6) to each. By reading alone that is
  strictly more work per first call; whether it is "within noise" at startup
  is a measurement, which this lane cannot take.
- The route arm (`CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1`) is still the
  measurement lever, unchanged.

What the orchestrator can run to decide it (unchanged from "How to finish
it", step 1): CratonBench start-to-first-iteration and the R11/R12 battery,
three interleaved runs each, `CRATONVM_BG_COMPILE=0` with and without
`CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1`, plus the seal census under
`CRATONVM_DBG_JITC=1`. `R12TierEagerHandlerDoor` is the existing
answer-check probe for this door.

## Round 13 wave 1 (lane replay)

Re-read at `6d39e8dcc`. The decision stands (not routed wholesale): the
decisive row is still compile cost per first call, a measurement no lane
can take. One row of the table moved:

- **`ldc` MethodHandle / MethodType / condy / wide string: routed, no longer
  sealed.** The front end set `has_unsupported_ldc`, sealed the method
  (`early-unsupported-ldc`, into `jit_skip_set`) and returned `None`, so under
  `CRATONVM_BG_COMPILE=0` such a method was kept off every door that reads
  `jit_skip_set` for the life of the process, although the ordinary door
  (`jit::try_compile`, which the background worker uses for the same method in
  the default mode) wires those constants. It now closes its own admission and
  hands the method to `try_jit_compile_wrapped_entry(optimize = false)`, as
  the wave-2 route does for a method with handlers or a category-2 parameter
  (same static-initialization deferral, same `early-backend-bail` seal of a
  refusal after the closure). Kill switch
  `CRATONVM_JIT_EAGER_DOOR_ROUTE_UNSUPPORTED_LDC` (default ON; `0` restores the
  seal); also off with `CRATONVM_JIT_EAGER_ORDINARY_DOOR=0`. Confirm under
  `CRATONVM_BG_COMPILE=0 CRATONVM_DBG_JITC=1`: `early-unsupported-ldc` seals
  vanish and those methods compile (javac rarely emits such an `ldc`; bytecode
  generators such as ASM-based frameworks and Groovy do; the "wide" string is
  a `CONSTANT_Utf8` with lone surrogates, `ConstantPool::get_utf8_wide`).
- The door's tier-up SINK (not its front end) changed this wave too: see
  `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns`, round 13
  section (`CRATONVM_JIT_TIERUP_SINK_REFUSAL_THROWS`). It does not affect the
  equivalence question.

Measurement still owed (unchanged): CratonBench start-to-first-iteration and
the R11/R12/R13 battery, three interleaved runs each, `CRATONVM_BG_COMPILE=0`
with and without `CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1`, plus the seal census.
Status stays OPEN.

## Round 13 wave 3 (lane replay2)

Re-read; the decision stands (not routed wholesale: compile cost per first
call is a measurement). One thing this door now does that the ordinary door
does not yet: it stamps the body it publishes with the bytecode it compiled
(`deopt_resume::stamp_compiled_source_of`, the `padded` allocation the
backend read; proposal R13-1), so a trap out of an eager-door body whose
class an agent redefined resumes in the body's own code
(`r12w5-replay3-bytecode-identity-on-the-artifact-patch`, round 13 wave 3).
The `jit_bridge` doors stamp once
`r13w3-replay2-jit-bridge-source-stamps-and-rerun-answer-patch-FIXED-20260928.md`
is applied; a routed method goes through those doors, so routing loses
nothing here. The remaining seals (`early-jit-scan-reject`,
`early-rbc6-athrow-with-handler` under `CRATONVM_JIT_EAGER_ORDINARY_DOOR=0`,
`early-rbc6-handler-reads-unsafe-local`, `early-backend-bail`) mirror refusals
the ordinary door makes too (`jit_scan` is the same scanner; the RBC.6 rule
is the shared `precise_handler_frame_blocking_site`), so no further seal is
over-broad by reading. Measurement still owed (unchanged). Status stays OPEN.

## Round 13 wave 8 (lane irexc2): no code is left before the measurement; the exact runs

Re-read on the wave-8 tree (`vm/src/runtime/interpreter.rs` ~1868-2135, ~3037-3060, ~3483-3496).
No code is owed before the measurement:

- The measurement arm is in place and complete: `CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1`
  (`eager_door_routes_every_method`) joins the route condition at ~2061, which sits BEFORE every
  seal of the door's own front end (`early-jit-scan-reject` ~2145, `early-rbc6-athrow-with-handler`
  ~2155, `early-rbc6-handler-reads-unsafe-local` ~2186, `early-unsupported-ldc` ~3058). Under the
  arm the only seal left is `early-backend-bail` (~3496), which is the ordinary door's refusal.
  So the arm measures exactly the retirement.
- The arm is reached only under `CRATONVM_BG_COMPILE=0` with `CRATONVM_JIT_C2_FIRST_CALL` unset
  (the two branches before it return first) and with `CRATONVM_JIT_EAGER_ORDINARY_DOOR` on (default).
- The table's rows still hold; nothing moved this wave.

**What the measurement must answer, in order** (each is a stop condition):

1. **Coverage.** A method the door's front end compiled but `try_compile` refuses is SEALED under
   the arm (`early-backend-bail` into `jit_skip_set`) and then interprets for the life of the
   process, because under `CRATONVM_BG_COMPILE=0` nothing else compiles it. So compare the SETS of
   compiled methods, not only the counts. Signal: the arm's `early-backend-bail` set minus the base
   arm's `early-*` set must be empty or explained (each extra method is one `try_compile` refusal
   to read, `CRATONVM_DBG_JITC=1` names its reason).
2. **Answers.** R11+R12+R13 battery identical between the arms (`R12TierEagerHandlerDoor` is the
   door's own answer probe).
3. **Startup.** Wall time of a startup-shaped run, three interleaved reps. Retire if the arm's
   median is within 5% of the base median (or inside the base arm's own min-max spread); otherwise
   take step 3 of "How to finish it" (keep the route for the table-needing populations only).

**Commands** (orchestrator; `$EXE` = the wave binary; `run13.sh` passes the environment through):
```
cd /c/craton/jitr13-probes; JH="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
./prep13.sh R13Irexc2EagerStartup
# 2. answers
CRATONVM_BG_COMPILE=0 ./run13.sh $EXE eager-base > res-irexc2-eager-base.txt
CRATONVM_BG_COMPILE=0 CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1 ./run13.sh $EXE eager-all > res-irexc2-eager-all.txt
diff <(awk '{print $1,$2}' res-irexc2-eager-base.txt) <(awk '{print $1,$2}' res-irexc2-eager-all.txt)
# 1. coverage (seal sets and compile counts)
for a in base all; do e="CRATONVM_BG_COMPILE=0"; [ $a = all ] && e="$e CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1"
  env $e CRATONVM_DBG_JITC=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 $EXE --java-home "$JH" -cp cls13 R13Irexc2EagerStartup > eager-$a.log 2>&1
  grep -oE 'jit-skip-seal site=[a-z0-9-]+ .*' eager-$a.log | sort > eager-$a.seals
  echo "$a: $(grep -c 'jit-skip-seal' eager-$a.log) seals, $(grep -c 'first-compile' eager-$a.log) first-compile"
  cut -d' ' -f2 eager-$a.seals | sort | uniq -c; done
comm -13 <(awk '{print $3}' eager-base.seals | sort -u) <(awk '{print $3}' eager-all.seals | sort -u)
# 3. startup, interleaved
for r in 1 2 3; do for a in base all; do e="CRATONVM_BG_COMPILE=0"; [ $a = all ] && e="$e CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1"
  t0=$(date +%s%N); env $e CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 $EXE --java-home "$JH" -cp /c/craton/jitr11-probes/cls CratonBench none >/dev/null 2>&1; t1=$(date +%s%N)
  env $e CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 $EXE --java-home "$JH" -cp cls13 R13Irexc2EagerStartup > /dev/null 2>&1; t2=$(date +%s%N)
  echo "r$r $a bench-none=$(( (t1-t0)/1000000 )) startup-probe=$(( (t2-t1)/1000000 ))"; done; done
```
(`CratonBench none` runs no phase: VM start, the bench class's own startup, exit.) Expected
signals: under the arm, `first-compile` lines are 0 and every `early-*` seal but
`early-backend-bail` is gone; the `comm` line lists nothing (or only methods whose base-arm body
was unsound anyway, e.g. those the table's JVMS 5.5 row names); answers identical.

If all three pass: delete the front end (from the `compile_gate::admit(.., EagerFirstCall, ..)`
call to the end of the `or_else` closure), make the route unconditional, move the
`cfg!(not(target_arch = "x86_64"))` return below it, and retire `CompileDoor::EagerFirstCall`
(`rg -n EagerFirstCall jit/src vm/src`: `compile_gate.rs`, `jfr_compile_decision.rs`,
`x64/driver.rs`, `interpreter.rs`, plus the JFR crate's enum). Status stays OPEN until then.

## Round 13 wave 10 (lane callcost4): still no code owed; the measurement has not been taken

Re-read at `1e4c0055c`. Nothing moved since wave 8's section: the measurement arm is
`eager_door_routes_every_method` (`vm/src/runtime/interpreter.rs:966`), joined into the route
condition at ~2063, ahead of every seal of the door's own front end (`early-jit-scan-reject`
2145, `early-rbc6-athrow-with-handler` 2155, `early-rbc6-handler-reads-unsafe-local` 2186,
`early-unsupported-ldc` 3058); the only seal left under the arm is `early-backend-bail` (3496).
`CompileDoor::EagerFirstCall` is still named in `jit/src/compile_gate.rs` (5 arms),
`jit/src/jfr_compile_decision.rs:146` and `interpreter.rs:2110`, the retirement list step 2
gives. The census strings the wave-8 commands grep are current
(`[cratonvm-jitc] jit-skip-seal site=<site> C.m(D)`, `interpreter.rs:1479`, and
`[cratonvm-jitc] first-compile C.m(D) entry=.. len=..`, `interpreter.rs:3409`), so those
commands run as written.

The measurement itself has not been taken: no `eager-base` / `eager-all` result exists under
`C:\craton\jitr13-probes` and `ORCH-LOG.md` records none. Nothing this wave changes the door's
answer (this lane's changes are the `MethodHandle` doors and the JIT native site cache, neither
of which the eager door reaches). The three stop conditions and the commands of wave 8's section
are the whole of what is owed; one addition to stop condition 1: under the arm a method the
front end used to compile and `try_compile` now refuses shows up only as an
`early-backend-bail` seal line, so the `comm` line of the wave-8 commands IS the coverage delta
(an empty output passes). Status stays OPEN (measurement-only).

## Round 14 wave 1 (lane calls): decided -- a measurement, not a code change

Re-read at `adb9178bc`. No code is owed before the measurement, and none should be written:
the arm (`CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1`, `eager_door_routes_every_method`,
`vm/src/runtime/interpreter.rs:1046`, joined into the route condition at ~2156) still sits ahead of
every seal of the door's own front end (`early-jit-scan-reject` ~2254,
`early-rbc6-athrow-with-handler` ~2264, `early-rbc6-handler-reads-unsafe-local` ~2295,
`early-unsupported-ldc` ~3299); the only seal left under the arm is `early-backend-bail` (~3751).
The census strings the wave-8 commands grep are current (`jit-skip-seal site=` ~1559,
`first-compile` ~3664). `CompileDoor::EagerFirstCall` is named in `jit/src/compile_gate.rs`,
`jit/src/jfr_compile_decision.rs:146`, `jit/src/x64/driver.rs:4753`,
`vm/src/runtime/interpreter.rs:2211` and a comment in `vm/src/jit/helpers.rs:30406` -- add the
driver arm and the comment to step 2's retirement list. Also note that `interpreter.rs` is being
edited by the interpreter round; the retirement edit must be made on a tree that has their wave.

**The arms to run** (orchestrator; the wave-8 commands with the round-14 harness names; every run
under `CRATONVM_BG_COMPILE=0`, which is the only configuration that reaches this door):

| arm | env | what it answers |
|---|---|---|
| base | `CRATONVM_BG_COMPILE=0` | today's eager front end |
| all | `CRATONVM_BG_COMPILE=0 CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1` | the retirement |
| (control) | `CRATONVM_BG_COMPILE=0 CRATONVM_JIT_EAGER_ORDINARY_DOOR=0` | the pre-route door, to price the route itself |

1. coverage: the `comm` of the two arms' seal sets under `CRATONVM_DBG_JITC=1`
   (`R13Irexc2EagerStartup`, `R12TierEagerHandlerDoor`, `R13ReplayEagerWideLdc`) must be empty;
2. answers: `run14.sh` battery identical between base and all;
3. startup: `CratonBench none` and `R13Irexc2EagerStartup` wall time, three interleaved reps;
   retire if all's median is within 5% of base's (or inside base's min-max).

Status stays OPEN (measurement-only).
