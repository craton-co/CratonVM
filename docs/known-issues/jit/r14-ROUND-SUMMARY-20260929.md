# JIT round 14: what landed, what was measured, what is still open

Status: OPEN (index page: it closes when every page listed under "Still open" is FIXED or CLOSED)
Area: the JIT and everything round 14 touched (synchronized splices, call cost, deopt/chain resume, FFM, traces, monitors, `--compatible` collections, `--jdk-only` retirements)
Severity: index
Found by: round 14 orchestrator, 2026-09-29

Read with `r14-HANDOFF-next-jit-round-20260929.md` (how to pick the work up). Every landed change
has a kill switch registered in `types/src/flag_groups.rs`. The page that introduced a change names
its switch, and so does its retired copy under `docs/internal/fixed-bugs/`.

## Scope and shape

* Base `dev` `adb9178bc`, branch `claude/jit-compiler-perf-round-9bf7f6`. Seven waves of up to seven
  lanes (about 55 lane runs plus hand-backs), with `dev` merged once (interpreter round i1 wave 42).
  Commits: wave 1 `03921dad8`, 2 `20a1dbb4f`, 3 `6e4d18012`, dev merge `80d5e7529`, 4 `587fe553d`,
  5 `74355aa4e`, 6 `59a41a0d8`, 7 `a9753f2c5`, then the close.
* Work items: the open defect pages, plus the proposals the round queued. Wave 1's triage lane
  ranked those proposals (`jit-proposal-backlog-r14-20260929.md`, evidence in
  `docs/internal/jit-proposals/jit-proposal-triage-evidence-r14-20260929.md`: DONE 120, KEEP 210,
  DUPLICATE 78, SUPERSEDED 11, REJECT 77). The 77 round-13 books and the r13 backlog are retired
  to `docs/internal/jit-proposals/*-RETIRED-20260929.md`.
* **75 pages retired** to `docs/internal/fixed-bugs/` (23 after wave 2, 46 after wave 6, 6 after
  wave 7), each verified by a build, the crate tests and the probe battery.
* Declared switches: about 2030 to 2147.

## Default flips (opt-in triage)

`docs/internal/jit-proposals/jit-optin-switch-triage-r14-20260929.md` is the triage. Flipped ON in
wave 2, after the checklists, the w1b bench and a Spring flip arm (Spring def == noflip):
`CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER`, `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC` (SyncM -12 %),
and `CRATONVM_JIT_IR_PRUNE_LOOP_HEADER_LOCALS`. Measured and kept OFF:

* `CRATONVM_FFM_DOWNCALL_GC_SAFE`: the cost gate fails (`llabs` 4023 to 17259 ns/call, 4.3x against
  1.5x). It waits for the GC round's light in-native deposit.
* `CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS`: not needed. `mic_grace_lag_refused=0` on R12Mega6CellRefill,
  R13Mega7LoaderChurnIface and R14MicSpinnerMisses in both arms, because M8-1 plus the wave-7
  self-stamp answer every refusal.
* The chain arm (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES` + fences + multi-return) was neutral on w1b
  (-2 %) and -10 % on hashmap with `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES=128`. The flip still owes the
  eager-chain soak and a re-bench (handoff).

## What landed, by area

**Synchronized code (`SyncM`).**
* SR-1 splices a trap-free synchronized callee between MonitorEnter/Exit. The same holds for static
  callees (SS-2), inside held regions (SS-3), and for multi-return bodies (SS8-1b).
* A same-receiver synchronized call nested in a synchronized splice (SS8-1a) is spliced without a
  monitor pair.
* `Thread.holdsLock` folds: in splices and regions (SS-7), in synchronized instance methods (SS8-3),
  on the finished graph (S5-2), and for a static method's own class (SS8-5, RV6-1).
* Window elision inside regions and methods (SY3-2), plus a sync-splice census line.
* Wave 5 fixed a wrong answer in wave 4's holdsLock fold: it trusted an open loop phi.

**Call cost (`fib`).**
* Call-crossing residency (CC5-1), IC-hit / direct-call cold tails (CC3-1), and in-loop static sites
  priced hot (C14W3-1).
* The entry fold (wave 5): call-free activations past the fast return are answered from a CMOVE
  table.
* CE-1 (wave 6): direct self-calls in the folded range are answered at the call site.
* Wave 7: the fold evaluates through self-calls (fib folds n=2..5), and the parameter register is
  filled in the prologue.
* Literal `length/isEmpty/charAt/equals/hashCode` folds (C14W3-4, I7-4) and OD-1.

**Deopt and chain resume.**
* Every chain sink (doors, tier-up, call-site service, OSR transfers) rebuilds a redefined inner
  scope from its own spliced bytecode (R14DP-5, CH3W-3).
* The x64 framed trap names its own artifact (R13RP6-1).
* One per-bci de-spec rule (RS-3), withdrawn at the spliced site.
* The code-buffer cascade proof (the gcd-d10v crash did not reproduce in 270 Linux runs).
* The IC quiescent stamp (M8-1) and the catch-up self-stamp.

**FFM.**
* Auto arenas are freed after collection, with a single-sweeper claim that closes a use-after-free.
* Address segment scopes and store order; the process symbol lookup and default lookup.
* Over-aligned groups; upcall direct-static (about 2x on qsort upcalls).
* `arrayElementVarHandle` now works (stores were dropped and loads answered null).
* JDK `IndirectVarHandle` accesses are served through their `handleFactory`; they used to be dropped
  silently.
* Layout VarHandle index, bound, enclosing-layout, root-alignment and value-leaf checks.
* The null-ADDRESS NPE message only on the VarHandle roads, as HotSpot does.

**Traces (`getStackTrace`, `Thread.getStackTrace`, JMX).**
* The optimizing tier's frames match the interpreter's (safepoint-id chain).
* Full `StackTraceElement` origin, including module prefixes and the loader.
* `Thread.run` stand-in frames with a per-thread memo.
* `(Native Method)` leaf frames for throwing natives.
* Join / wait / sleep / argument-check stand-in chains, including the timed and two-site forms.
* Published-stack overload lines, `dumpThreads` format, JMX locked-monitor depths, and an
  interruptible `join(0)`.

**Monitors.**
* `notify()` wakes one waiter (per-waiter condvar), with poll backoff.
* An interrupt wakes only its target, with a SeqCst handshake.
* A timed wait parks once, with a 250 ms safety slice.
* Contention census rows, and an opt-in spin abort on hand-over.

**`--compatible` and `--jdk-only`.**
* Live reversed `LinkedHashMap` / `LinkedHashSet` views (keys, entries, values); `LinkedHashMap.reversed()`
  runs the JDK view.
* `copyOf` identity; `Collectors.toUnmodifiable*` results are really unmodifiable (they were mutable
  aliases).
* Optional callbacks left to bytecode.
* A retired row masks ancestor Bridges.
* Retired under `--jdk-only`: `OutputStream.write([B)`, the `java.sql` date/time family (it ignored
  `cdate`), and `Array.newInstance`.
* Liquibase dates read through `getTime()`.
* Per-VM ldc slots and bootstrap-appended classes (MISC11).
* `BigInteger`/`BigDecimal` limb roads (BD4-1/2).

## Verification (final builds)

* **Windows w7b** (waves 1-7):
  * crate tests: jit 4793, vm lib 4016, native-builtins 5371 (all pass after the close's fixes) and
    vm integration 17/17. misc has only the known `dev` red `the_known_issues_root_exists`.
  * `cargo check` passes with `--features experimental-debug` and with
    `--all-targets --features synthetic-jdk`.
  * Stub ratchet in all three arms, re-frozen (windows 5277/5250/5250).
* **R14 probe battery** (w7b default): 78 match HotSpot 25.0.3. The rest differ for known reasons:
  * `R14FfmBlockedDowncallPause`: GC_SAFE is opt-in.
  * `R14Trace4NativeLeaf` virtualWait and `R14Trace6ArgCheckSites` vjoin: no continuation support,
    so a virtual thread is a `BoundVirtualThread` (owner decision page).
  * `R14Ffm7IndirectVarHandle`: three `filterValue` roads throw NoSuchMethodError (the
    `MethodHandle.editor()` gap, residual page item 3).
  * Every wave-5 to wave-7 kill-switch arm, plus `--compatible`, `JIT_THRESHOLD=1` and
    `C2_ACCEPT=always`, matched on the new probes.
* **The full 8-arm matrix** ran on w2a (556/559 in every JIT arm) and w3a (572/576). On w6a it was
  stopped at 150-215 probes per arm with 0 failures, superseded by wave 7.
* **Linux host:** stub ratchet in 3 arms on w2b, w3b and w6b (re-frozen at w6b: 5237/5210/5210);
  the d10v loop in 270 runs; Spring (42 classes) w2b def == noflip, w3b no regression, w6b the same failing set apart from two flaky network integration tests (see the handoff).

## Benchmarks (Windows, interleaved, medians, ms; HotSpot 25.0.3 in the same loop)

| build | SyncM round | fib (wall) | hashmap (wall) |
|---|---|---|---|
| w1b default | 91 | 2901 | 1870 |
| w5a default | 90 | 2153 (fold off: 3542) | 2266 |
| w6a default | 92 | 1967 (CE-1 off: 2341) | 2316 |
| w7b default | 84 | 1055 (fold self-calls off: 1614) | 2148 |
| HotSpot | 32-37 | 2229-2451 | 678-779 |

C14W3-1 is neutral to positive (kept on). The parameter register fill is neutral in 3 reps (kept on;
re-measure). Nested same-receiver splices are neutral on SyncM, because its `n` shape does not
engage them.

## Still open (26 pages)

* **Performance:**
  * `r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow` (SyncM 2.6x).
  * `r12w8-monitor-where-a-contended-enter-spends-its-time` (census rows landed; the decisive run is
    listed there).
  * `r12w8-callcost-fib-call-anatomy` (fib is now faster than HotSpot's wall; the page lists the
    remaining anatomy).
  * `r13w11-chain5-chain-arm-hashmap-kernel-slower-than-default`.
  * `r13w2-irhash-gengc-young-premium` and `r12w7-iropt6-compiled-allocation-premium`: the hashmap
    3x gap is the GC round's.
  * `r12w3-mega2`, `r12w5-mega4`, `r12w7-mh5`, `r13w4-sync2-self-locking-bodies-with-deopt-exits`.
  * `r14w4-calls2-ic-hit-cold-tail-needs-an-out-of-line-region`.
  * `r14w7-mega-static-leaf-call-costs-an-interpreter-call-under-c2-never`.
* **Liveness / GC-owned:**
  * `r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world` and
    `r14w1-ffm-downcall-in-native-state-patch`: the flip waits for the light deposit.
  * `r11w19-g1store-band-root-rejects-drop-derived-pointer-pins`.
  * `r12w5-ffm2-arena-close-frees-shared-residuals`.
* **Correctness residuals (low):**
  * `r14w7-ffm7-indirect-var-handle-residuals` (the `filterValue` NSME and two WMTE refusals).
  * `r13w8-proxy5-proxy-identity-residuals` (item 4 needs a Linux Spring forked-loader run).
  * `r13w8-hashcompat3-compatible-collection-residuals` (items 4 and 8 wait on the HT-3 owner
    decision).
  * `r13w13-compat7-chm-tree-bins-ht3-cost` (accepted).
  * `r12w6-override-runtime-package-residuals`.
* **Owner decision:** `r14w7-trace6-virtual-thread-frames-reference` (match HotSpot default or
  `-XX:-VMContinuations`).
* **Index / other:**
  * `r13w7-shadow-jdk-only-bridges-over-bytecode-census`: families still to retire.
  * `r13w3-framestate-vm-chain-resume-gaps`: blocks the chain flip.
  * `r12w3-tier-eager-first-call-front-end-not-yet-equivalent`.
  * `aarch64-backend-runs-no-java-on-a-real-machine`.
