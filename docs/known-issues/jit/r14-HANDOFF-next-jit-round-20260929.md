# Handoff: where JIT round 14 stopped, and how the next round picks it up

Status: OPEN (handoff page for the next JIT round; retire it when round 15 writes its own summary)
Area: the JIT and everything round 14 touched (see `r14-ROUND-SUMMARY-20260929.md`)
Severity: index
Found by: round 14 orchestrator, 2026-09-29 (written at the close; the owner stopped the round after wave 7)

Read this page, then `r14-ROUND-SUMMARY-20260929.md`, then the page for whatever you pick up.
Each open page says what is left and how to confirm a fix. A page with an exact patch says so in
its `Status:` line. The ranked idea list is `jit-proposal-backlog-r14-20260929.md`. The round-14
lane books (`jit-r14-<lane>-proposals.md`, 53 of them) are ideas, not work items, unless the owner
queues them.

## 1. State of the branch

* Branch `claude/jit-compiler-perf-round-9bf7f6`, merged into `dev` and pushed at the close.
  Nothing is half-applied: every patch page is either applied (retired to
  `docs/internal/fixed-bugs/`) or OPEN with its exact patch.
* Every behaviour change has a kill switch in `types/src/flag_groups.rs`. Flag docs
  (`docs/flag-tokens.md`, `docs/config/flag-inventory.md`) are regenerated.

## 2. Owner decisions that bind the next round

These carry over from round 13:
* Correctness fixes are ON in both modes, each behind a kill switch.
* `--compatible` changes only through such switched fixes.
* The AGENTS.md registration rules apply.
* At most seven agents; `cargo -j 5`, one cargo at a time.
* This round fixed only defect pages and the most promising queued proposals.

## 3. Default-OFF switches waiting for a decision

| switch(es) | state at close | what decides it |
|---|---|---|
| chain arm: `CRATONVM_JIT_IR_SPLICE_FRAME_STATES` + `_CHAIN_FENCES` + `_MULTI_RETURN` | w1b: neutral (-2 %). With `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES=128`, hashmap -10 %. CH5-4 (the `OnceLock` reader) is fixed | `CRATONVM_DEOPT_EAGER_CHAINS=1` soak on the probe battery with the range pins, a `bench14.sh` rebench (default / chain / chain+HOMES128), `r13w3-framestate-vm-chain-resume-gaps`, then flip (keep `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` off) |
| `CRATONVM_FFM_DOWNCALL_GC_SAFE` | cost gate FAILS (4.3x on `llabs`) | the GC round's light in-native deposit (`docs/known-issues/gc/gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md` §3), then re-run `ffmcost.sh` |
| `CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS` | not needed today (0 lag refusals on three probes) | keep OFF |
| `CRATONVM_MONITOR_SPIN_HANDOVER_ABORT` (new, opt-in) | needs `CRATONVM_MONITOR_LAZY_PARK_US` | the census run on `r12w8-monitor-where-a-contended-enter-spends-its-time` (probe `R14Monitor3Crowd`) |
| `CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS` (CH5-5), `CRATONVM_JIT_IR_PRECISE_FRAMES_SYNCHRONIZED` (W11-2), `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS` census | opt-in | their pages / books name the runs |
| older opt-ins | `docs/internal/jit-proposals/jit-optin-switch-triage-r14-20260929.md` | -- |

Default-ON but worth a re-measure: `CRATONVM_JIT_IR_PARAM_REGISTER_FILL` (wave 7, neutral in 3 reps).

## 4. Left pending at the close (do these first)

* **Linux bridge-ratchet / kind-map census.** The two kind-map TSVs were hand-edited for the
  `java/sql` (11 rows) and `Array.newInstance` (2 rows) retirements. Lane ffm6 added 26 explicit
  `Bridge` rows for `MemoryLayout.arrayElementVarHandle`; its removal record is
  `docs/jdk-only/ffm-array-element-var-handle-bridge-20260929.md`. Those 26 rows are not in the kind
  map, and `scripts/baselines/jdk-only-bridge-ratchet.json` was not re-frozen. On the Linux host,
  run `sh regression-suite/bridge-ratchet.sh --update-baseline --note "round 14"` and read the diff.
* **Full probe matrix on the final tree.** The 8-arm `pmatrix14.sh` last completed on w3a. The w6a
  run was stopped at about 200 probes per arm with 0 failures. Run it on the merged tree with
  `TMO=600` (`R13Compat6TreeBins` needs more than 300 s under load).
* **Spring on the final tree.** w6b (waves 1-6) completed: 42 classes, the same set of failing
  classes as w3b except two network integration tests that flake across builds.
  `RSocketClientToServerCoroutinesIntegrationTests` had 2, 0 and 4 failures on w2b, w3b and w6b;
  `WebClientIntegrationTests` had 1 failure on w3b and 0 on w6b (both `rc=124` timeouts). Rerun
  those two alone, 5 times each on the merged tree, before reading anything into them. Wave 7 was
  not run on Linux Spring.
* **The static-leaf page** (`r14w7-mega-static-leaf-call-costs-an-interpreter-call-under-c2-never`):
  one `CRATONVM_C2_ACCEPT=never CRATONVM_DBG=mic-prof CRATONVM_DBG_JITC=1 R12Mega4OneSite` run picks
  between its two hypotheses. If the second holds, it matters in the default `evidence` policy.

## 5. What to do next, in order

1. **hashmap (3x):** mostly the GC round's young-gen allocation premium (`r13w2-irhash-*`,
   `r12w7-iropt6-*`). On the JIT side, the chain-arm flip (§3), then GS-6 / CH5 follow-ups.
2. **SyncM (2.6x):** the uncontended synchronized method path. Read
   `r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow` section 1 (every wave appended
   what is left), and S6-2 / S7-* in `jit-r14-sync6-proposals.md` / `jit-r14-sync7-proposals.md`.
3. **The IndirectVarHandle residuals** (`r14w7-ffm7-*`, item 3). `filterValue` handles throw
   NoSuchMethodError through `MethodHandle.editor()`. Then F7-1 (typed invocation, no Object[]) and
   F7-2 (retire the 26 `arrayElementVarHandle` Bridges once the served JDK road is green with
   `CRATONVM_FFM_ARRAY_ELEMENT_VAR_HANDLE=0`).
4. **The virtual-thread frames owner decision** (`r14w7-trace6-virtual-thread-frames-reference`).
5. **Monitors:** run the census the monitor page lists, then decide the lazy-park budget (MC3-2) and
   the spin abort.
6. **The jdk-only census** (`r13w7-shadow-*`): the next contained families. CA6-1 in
   `jit-r14-compat6-proposals.md` proposes finding the thin-wrapper Bridges mechanically.

## 6. How round 14 worked (reuse it)

* **Worktrees:** the session worktree off `dev`; lanes read and write only there. Builds run in
  `C:\craton\wt-r14dev` (target `C:\craton\tgt-r14dev`, binaries
  `C:\craton\jitr14-bin\cratonvm-jitr14-<tag>.exe`) via `BASE=<sha> snap.sh <tag>` and
  `build.sh <tag>`.
* **Probe kit** in `C:\craton\jitr14-probes`:
  * `prep14.sh` (javac + HotSpot refs), `run14.sh <exe> <label> [args]` (`SETS=r14`,
    `PROBES="A B"`, `TMO=`);
  * `pmatrix14.sh`, `testall.sh <tag>`, `bench14.sh <reps> <out> <arm>...`, `ffmcost.sh`;
  * `ORCH-LOG.md` (running log), `LANE-BRIEF-COMMON.md`, `WAVE<N>-ASSIGNMENTS.md`;
  * `regflags.py` (spec `GROUP|token|KEY|anchor[|OPTIN]`; the anchor must be a token unique in
    `flag_groups.rs`, because `ir-entry-fold` exists in both DBG and JIT);
  * `retire.py` (strips a trailing `-FIXED`/`-CLOSED` from the stem).
* **Linux host:** one persistent ssh pipe (`ssh/rsh.sh`). Upload `git diff --binary` as base64 in
  60000-byte chunks, check the md5, `git apply` on `/data/wt-jitr14`. Then
  `/data/jitr14/build.sh <tag>; stub.sh <tag>; spring/run.sh <tag> def X=1` under `setsid nohup`.
  `build.sh` waits for every other cargo, and other sessions often hold the host's cargo for hours.
* **Lanes:** each got a wave assignment with owned regions. Region-split files had targeted edits
  only. Every lane's switches were registered by the orchestrator after the report. Hand-backs
  (SendMessage to the finished lane) fixed build and probe failures in the lane's own context.

## 7. Traps round 14 hit

* **Gates that scan source text read tests too.** `unconstructed_carrier_gate` took test tuples
  `("java/sql/Connection", ...)` in `retired_shadow.rs` as retired rows; write negative lists as
  `[..]` arrays. `r13_misc10_create_string_ratchet` counts any `create_string(<computed>)`;
  `typed_array_producer_ratchet` counts test fixtures too.
* **A new per-thread optimization can invalidate an old unit test's model.** The IC catch-up
  self-stamp lifted the refusal `a_receiver_refused_only_for_a_lagging_grace_is_reported` staged on
  one thread; the test now pins the switch off.
* **"Silently answers null" hides whole families.** `arrayElementVarHandle` and every
  `IndirectVarHandle` access looked like working code until a probe read the stored value back. A
  probe must check a store by reading it back, not only that no exception was thrown.
* **HotSpot is the reference, not the lane's expectation.** The null-ADDRESS NPE message differs by
  road (VarHandle: helpful; `MemorySegment.set` and downcalls: null). Always regenerate the ref after
  a lane rewrites its probe.
* **Stub ratchet:** the gate fails only on a rise, so a platform whose count falls keeps a stale
  constant (Linux sat 21 above its real count for four waves). Re-freeze both platforms from printed
  counts (`-- --nocapture`).
* **Retirement renames:** a rename whose new name contains the old one doubles on a second replace.
  Rewrite references in one pass.
* Known red on `dev` (not round 14): `the_known_issues_root_exists`. Timing test
  `t19_6_wake_dedupes_concurrent_calls` flakes under load; rerun it alone.
