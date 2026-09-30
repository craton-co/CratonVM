# The pinned young copy's default-flip gate: stale peer words, classified and pinned

> **STATUS (2026-09-29, gce ve2): OPEN -- KEEP OFF. The ON-arm MtChurn crash (2/2) is gce-x-osr-entry-holds-snapshot-reads-a-dead-local-in-a-decommitted-young-span, which also crashes the default arm 1-2/20; fix it first. y-4 was slower (steady 350.5 vs 325 ms); JniRoots ON 3/6 with no OFF twin run.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/y): OPEN -- the whole gate is now written as battery rows; nothing in the code blocks the flip.** Row 3 is MET on e1 (10/10). Both code prerequisites are in the tree: lane k's raw-JNI-locals count (`vm/src/native/jni.rs` `RawJniLocalsScope`) and the Phase 5 compaction veto (`major_no_compact`). Section 1 of `docs/internal/gc-design-perf-round-20260929/e2-y-report.md` gives every remaining row in `p` format, with its pass bound. The rows cover:
> - flip-matrix rows 1-8, both arms;
> - engagement (`pycopy_cycles` >= 90 % of `minor=` on the divert probe);
> - the stale gate (`stale_live=0` on every `cycle=pinned` summary);
> - row 2 (JNI), row 3a (`Gcd1PinnedCalleeOomeProbe`, `Gcd1ThreadExitSpillProbe`), row 4 (old gen);
> - G6 on the short evac arm (`[evac-timing] steady_median_ms`, ON <= OFF);
> - the take-over arm (informational);
> - the host-only rows: netty x 10 both ledgers, QDox, Spring Boot.
>
> The decision rule is unchanged. The flip commit must re-run `gc_quiescence::young_pin_ledger_tests`, `gen_heap::tests::r4w5_pinned_young_copy` and `-p cratonvm-vm --lib young_pin` with the default ON.

> **STATUS (2026-09-29, gce e1/x): KEEP -- row 3 is MET; the flip is not decided.** Row 3: `pinned_row3_1..10` (`verify-e1/ve1`, `CRATONVM_GEN_PINNED_YOUNG_COPY=1`, backoff default, `GenR4W6JitOomRootProbe -Xmx64m`) = HotSpot 10/10 with no timeout on e1 (base 9/10). The battery's `pinned_young_copy`, `pinned_gauntlet` and `w3_gauntlet_pinned_par` = HotSpot on every e1 battery. **Remaining:** row 3a's other two probes with the flag (`Gcd1PinnedCalleeOomeProbe`, `Gcd1ThreadExitSpillProbe`), the flag-on netty 3-class loop (pin8's gate), and the take-over arm on `GenR4W4EvacThroughputProbe`; then the flip decision.

> **STATUS (2026-09-29, gce e1/y): NARROWED -- row 3's last failure (`armD_backoff_2`, `FAIL oome-compiled-callee`, d7 build `307f0c6a2`) is a retention whose only pinned-copy-specific path found by reading is closed on the current base (`adb9178bc`); row 3 needs its 10-run gate on a current build, not a code change. The flip stays the orchestrator's.**
>
> - **What the row still reads.** On d7 the backoff-default arm D failed 1 of 2 with a RETENTION line (not a timeout); its two timeouts were the backoff-OFF controls, whose livelock is `../../internal/gc/gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles-FIXED-20260928.md` (fixed on d9: `pcopy_jor*`, `CRATONVM_GEN_PINNED_YOUNG_COPY=1 GenR4W6JitOomRootProbe -Xmx64m`, backoff default, 6/6, no rc=124).
> - **Why the pinned arm could retain `oome-compiled-callee` and the default arm not** (re-read end to end, `collect_garbage_inner_with_pins` -> `run_non_moving_young_cycle` -> `sweep_old_gen_non_moving_impl`): the three `System.gc()` calls after the drop are requested majors and never take the pinned copy (`explicit_full_gc` diverts them), so the copy can matter only through the heap it leaves. It leaves a chain that ALTERNATES between the generations (pinned nodes stay young while their neighbours are promoted; with the backoff at its default, spilled nodes are also allocated straight into old gen), i.e. OLD -> YOUNG edges inside dead data. A requested major whose young half promoted anything used to fall back to the LEGACY young seed (every young survivor, including one kept only by a dead old node's dirty card, an old-gen root) -- the nepotism the d5/s page describes -- and on d7 that fallback was live. gcd d9/a made the true-root seed resolve a promoting cycle's promotions instead (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE`, default ON, `true_root_wide` in `sweep_old_gen_non_moving_impl`), so a dead old node is no longer marked through a young island on ANY requested major: the path is closed on every build since d9. Nothing else on the pinned copy's own path outlives the pause (identity forwards and kept survivors are rebuilt by `finish_in_place_young_cycle`; L5's page roots are one cycle and only on pinned cycles, which requested majors are not).
> - **What is left, by elimination:** a genuine root (a stale conservative word, README item 1), which the default arm shares; the in-progress lane c fix for `gcd-d9a-true-root-seed-skips-young-targets-of-close-live-set-rescues-20260928.md` (gce e1/c) touches the same seed and must be in the build that runs the gate.
> - **Gate (row 3), unchanged in substance:**
>   ```
>   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
>   for i in $(seq 10); do CRATONVM_GEN_PINNED_YOUNG_COPY=1 timeout 300 cratonvm $P GenR4W6JitOomRootProbe > d.$i.out 2> d.$i.err; echo "D rc=$?"; done
>   for i in 1 2 3; do timeout 300 cratonvm $P GenR4W6JitOomRootProbe > b.$i.out; echo "default rc=$?"; done
>   ```
>   Expected: every arm-D run ends (no rc=124) and prints HotSpot's nine lines (`PASS catch-staged-arg` ... `PASS oome-thread-exit`, `PASS all 8`; HotSpot 25 `-XX:+UseSerialGC -Xmx64m` is the oracle), 10/10; the default arm the same. A `FAIL oome-compiled-callee` in arm D only: rerun that seed with `CRATONVM_DBG=oldmark-root-census,root-source,gc-stats RUST_LOG=cratonvm::gc=info` and read (1) `oldsz_true_root_fallbacks` (must be 0 -- a non-zero `oldsz_true_root_fb_<reason>` names a fallback the WIDE seed did not cover), (2) the root section that holds the chain.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): NOT flipped. Row 3a (retention) now PASSES on its main probe; row 3 (liveness) still fails.**
>
> - **Row 3a:** `GenR4W6JitOomRootProbe -Xmx64m`, backoff off: arm D (`CRATONVM_GEN_PINNED_YOUNG_COPY=1`) `PASS all 8` 5/5 (`armD_1..5`), D+hold 5/5 (`armDh_1..5`), C 4/4 finished (`armC_1..4`); the flag-off control B fails 2/5 (`FAIL oome-thread-exit`). `../../internal/gc/gcd-d5s-pinned-copy-keeps-a-dropped-chain-across-requested-majors-FIXED-20260928.md` retires this wave. The row's other two probes still fail in the DEFAULT arm (`Gcd1PinnedCalleeOomeProbe` 0/3, `Gcd1ThreadExitSpillProbe` 2/9: the true-root fallback, on their own pages), so they cannot judge the flag yet.
> - **Row 3 (liveness):** with the backoff at its default, arm D 1/2 (`armD_backoff_2`: `FAIL oome-compiled-callee`); with it off, two 300 s timeouts (`armC_5`, `armDhk0_5`), both the livelock filed as `../../internal/gc/gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles-FIXED-20260928.md` (it also hits the default thrash arm, so it is not the pinned copy's alone). Required unchanged: 10/10 finish in arm D with the backoff at its default.
> - **Take-over arm:** now engages on `Gcd1PinnedTakeoverYoungProbe` (`ptko_taken` 39-43 of as many candidates, `takeover_1..3`), still inert on `GenR4W4EvacThroughputProbe`.
> - **Not run on d7:** the netty 3-class x 10 loop with the flag (the pin8 gate; `netty-d7-off` is the flag-off arm only: `AdvancedLeakAwareByteBufTest` 426/426, `DuplicatedByteBufTest` 416/416, `BigEndianDirectByteBufTest` 413/413, `BigEndianHeapByteBufTest` 414/414, rc 0, 3 runs each).

> **STATUS (2026-09-28, gcd d6/s): NOT flipped. Rows 3 and 3a still fail on
> the d5 build (`89532cb58`), and row 3 now has a liveness half.**
>
> - *Row 3 (liveness), measured:* `GenR4W6JitOomRootProbe -Xmx64m` with the
>   backoff off and the pinned copy on (arm D): 1 pass, 1 FAIL, 1 TIMEOUT at
>   300 s, of 3. The timeout is filed with the two runs that tell a livelock
>   from a hang, and the backoff-off debug arm from the default:
>   `gcd-d6s-pinned-copy-heap-full-run-can-time-out-FIXED-20260929.md`. Required
>   now: 10/10 finish in arm D with the backoff at its default, and the
>   timeout's run 1 attributed.
> - *Row 3a (retention), measured:* arm C 1/3 (3/3 on d4), arm D 1/3, D plus
>   `CRATONVM_GEN_REQUESTED_MAJOR_NO_PROMOTE=1` 2/3, default 3/3. The hold
>   helps but is not the whole fix; the 10-run table that names the rest is on
>   `../../internal/gc/gcd-d5s-pinned-copy-keeps-a-dropped-chain-across-requested-majors-FIXED-20260928.md`.
>   `Gcd1PinnedCalleeOomeProbe` also fails in the DEFAULT arm (lane u's
>   holder), so row 3a reads `GenR4W6JitOomRootProbe` and
>   `Gcd1ThreadExitSpillProbe` until that lands.
>
> **Previous STATUS (2026-09-28, gcd d5/s): NOT flipped. Row 3 FAILS on the d4 build
> and gains a retention row (3a) with its own page; everything else as d4/m
> left it (below).** Unbuilt when written.
>
> *Row 3, as measured on `d916d1c40` (orchestrator, `GenR4W6JitOomRootProbe
> -Xmx64m`, 3 runs each):* arm B (`CRATONVM_GC_FUTILE_YOUNG_BACKOFF=0`) 3/3
> `PASS all 8`; arm C (B plus the proof flag and the pinned copy) 3/3 `PASS
> all 8` (d4/m's revert fixed the hang); arm D (B plus
> `CRATONVM_GEN_PINNED_YOUNG_COPY=1` alone) 3/3 `FAIL oome-compiled-callee`,
> `FAIL 1 of 8`. `Gcd1ThreadExitSpillProbe` under arm C: 2/2 `[probe]
> thread-exit cleared=false usable=false`. So the pinned copy on its own
> keeps dropped data that the default path frees.
>
> *Row 3a (new), retention.* Cause by reading and a candidate fix (opt-in
> `CRATONVM_GEN_REQUESTED_MAJOR_NO_PROMOTE`) on
> `../../internal/gc/gcd-d5s-pinned-copy-keeps-a-dropped-chain-across-requested-majors-FIXED-20260928.md`:
> pinned objects stay young while their neighbours are promoted, which
> builds OLD -> YOUNG edges inside the chain; a requested major's old half
> then falls back to the legacy young seed whenever its young half promoted
> anything, and a dead old node keeps the rest alive through its young
> island. Required, 3/3 each, with the pinned copy on and
> `CRATONVM_GC_FUTILE_YOUNG_BACKOFF=0`: `Gcd1PinnedCalleeOomeProbe` prints
> `callee-oome caught=true`, `callee-oome cleared=true`,
> `callee-oome usable=true`, `PASS`; `GenR4W6JitOomRootProbe` prints `PASS
> all 8`; `Gcd1ThreadExitSpillProbe` prints what its flag-off arm prints.
> The flip needs whichever fix that page's runs select to be on by default
> with the pinned copy.
>
> *The take-over arm (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`) is not part
> of this gate:* it has never engaged (`ptko_candidates=0`), because the
> helper windows the measured probes produce refuse the pin, see
> `../../internal/gc/gcd-d5s-takeover-arm-sees-only-refusing-helper-windows-FIXED-20260928.md`.
>
> **Previous STATUS (2026-09-28, gcd d4/m): the gate as it now stands. NOT flipped
> (the lane does not flip it). The two risks d3/m reported are closed on the
> flag-on path, one of them pending a lane-k wiring; the d3/m Phase 5 change
> is reverted; three rows are new.** Unbuilt when written.
>
> *Risk 1, raw JNI local references: closed by construction once lane k's
> wiring lands.*
>
> - Which cycles the flip newly makes moving while a thread holds a raw JNI
>   local: exactly the cycles a young pin ledger licenses. Those are term 4's
>   pinned arm, the take-over arm and option B. Today they are sweeps.
> - The ledger does not name every raw local. It names one only when the
>   local sits inside a depositing thread's swept band. A thread with no JIT
>   entry deposits nothing: an interpreter-called native parked in a JNIEnv
>   up-call, or a foreign-attached thread between calls. A local stored off
>   the stack is in no band at all.
> - Landed (`gc/src/gc_quiescence.rs`):
>   - a per-VM standing count of threads holding raw locals
>     (`PauseLedger::raw_jni_locals`, `RawJniLocalsScope`,
>     `note_raw_jni_locals_open` / `_closed`);
>   - `pause_young_pin_read` reads INCOMPLETE while it is non-zero.
> - Result: those three arms never relocate while any thread of the VM holds
>   raw locals. Such a pause is the sweep it is today, so the flip never
>   widens `common-w2c`'s exposure. JIT-cold Cheney cycles are unchanged
>   (that exposure predates the flip).
> - The count is fed by lane k's JNI dispatch. Exact diff in
>   `docs/internal/gc-defects-round-20260927/d4-m-report.md`, request 1.
>   Until it lands the count reads 0, and this row is open.
>
> *Risk 2, two Generational VMs: fails closed, exactly.*
>
> - `pause_young_pin_read` now also reads INCOMPLETE whenever more than one
>   relocatable heap is live (`published_bounds_represent_every_live_heap`).
> - The ledger's young-range screen reads the single-tenant
>   `JIT_REGION_BOUNDS`, so a second heap could make a deposit drop this
>   heap's words while claiming completeness.
> - Before this change the refusal depended on the coverage verifier. That
>   runs only on threads with compiled frames, and
>   `CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD` switches it off.
> - Now the pinned copy and option B never run with two heaps. That is a
>   missed optimisation only.
>
> *Reverted: the d3/m Phase 5 consumption.* d3/m consumed the fragmentation
> compaction request on every ledger-pinned cycle. That also suppressed the
> fragmentation-triggered OLD SWEEP, which the sweep path still runs; it
> declines only the compaction. This is a liveness suspect for the livelock
> page's arm C hang. The compaction-only veto is lane n's (Phase 5): exact
> diff in the d4/m report, request 2. Until it lands, a ledger-pinned cycle
> may slide old gen on a fragmentation request, as before d3/m.
>
> **The gate, all rows required, on one build with the lane-k and lane-n
> diffs merged:**
>
> 1. G1-G7 below, re-run with `CRATONVM_GEN_PINNED_YOUNG_COPY=1`.
> 2. **(new) JNI.** `Gcd1JniRootsProbe` (`common-w9g` page STATUS for the
>    build lines), arm A (defaults) against arm A plus
>    `CRATONVM_GEN_PINNED_YOUNG_COPY=1`, 3 runs each, Generational. Every
>    line's PASS count with the flag must be at least the flag-off arm's.
>    Also run the unit test lane k adds for the wiring.
> 3. **(new) Liveness.** The livelock page's arms C and D (its STATUS), 3/3
>    each: no timeout. `GenR4W6JitOomRootProbe` prints the same eight
>    verdict lines as the flag-off arm B on the same build.
> 4. **(new) Old gen.** `GenR5W5HumongousTopProbe -Xmx128m` and
>    `GenR4W4HumongousFragProbe` with the flag print the flag-off arm's lines.
> 5. Two VMs: no row. The pinned copy does not engage there by construction.
>
> **Previous STATUS (2026-09-27, gcd d3/m; its Phase 5 fix is reverted by d4/m): re-read end to end for the default flip.
> One flag-on defect FIXED; two risks for the orchestrator to weigh; nothing
> flipped.** The netty rows are clean on the fixed build (20/20, triage).
>
> *What was read:*
>
> - the decision: `term4_alone`, the ledger read `pause_young_pin_read`, and
>   the 1/8 bound;
> - the plan: `build_pinned_young_plan`, `plan_takes_blocked_peer_words`, the
>   card walk-gap words;
> - `arm_in_place_young_cycle`, and `forward_object_impl`'s pinned, kept and
>   in-place-destination arms;
> - `scan_in_place_young`, `finish_in_place_young_cycle`, and the
>   post-cycle `young_in_place_vacated`;
> - the parallel arm is opt-in on its own flag and not part of this flip.
>
> *Fixed (flag-on only; default path unchanged):* a ledger-pinned cycle ran
> Phase 5 of the moving path, and that major SLIDES old gen when a
> fragmentation compaction is requested. The allocation-failure ladder
> requests one by default (`CRATONVM_GC_OLD_OOM_COMPACT`). The pinned copy
> pins YOUNG pages only, so an old object named by a parked peer's
> native-band or register word, or by an unmapped compiled slot (a derived
> cursor into an old array), could slide under that word. The cycle the
> pinned copy replaces, the sweep, takes the request and declines to
> compact on a JIT-warm pause. Now `collect_garbage_inner_with_pins`'s
> moving prologue consumes the request the same way on every ledger-pinned
> cycle (`pinned_young_words.is_some() && forced_pin_words.is_none()`).
> Under `CRATONVM_OLDGEN_COMPACT` (debug) term 4's arm still compacts. The
> same gap on the DEFAULT Cheney path is filed:
> `../../internal/gc/gcd-d3m-phase5-compacts-old-gen-under-unmapped-compiled-words-FIXED-20260928.md`.
> Effect on this gate: G1-G7 were measured before this change. It only
> removes compactions on pinned cycles, so a re-run of G6 and an OOM-ladder
> probe with the flag on (`GenR5W5HumongousTopProbe -Xmx128m`,
> `GenR4W4HumongousFragProbe`) should show the flag-off arm's lines.
>
> *Risks, not defects of the copy:*
>
> 1. **Raw JNI local references (`common-w2c`).** Today a JIT-warm term-4
>    cycle is a sweep, so nothing moves under a thread in an
>    INTERPRETER-called JNI native holding raw `jobject`s in C locals; only
>    JIT-cold Cheney cycles expose it. A native called from compiled code is
>    covered: its C frames lie inside the JIT band its deposit sweeps. The
>    flip turns every term-4 cycle into a relocating one and so widens that
>    exposure to JIT-warm programs. Pair the flip with the JNI out-of-process
>    run d2/i wrote (`Gcd1JniRootsProbe` arm A with the pinned copy on,
>    3/3), or with `CRATONVM_JNI_INDIRECT_LOCALS`.
> 2. **Two Generational VMs in one process.** It fails closed, and
>    therefore slowly. `JIT_REGION_BOUNDS` is single-tenant, so the coverage
>    verifier refuses (`published-bounds-describe-another-heap`) while two relocatable
>    heaps live, unless `CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD` is set. The
>    ledger's depth test compares against the process-wide JIT depth, so
>    another VM's compiled frames read as a shortfall. Either way the pinned
>    copy declines. Correct, but the flip buys nothing for an embedder
>    running two VMs.
>
> *Checked, sound:*
>
> - the ledger is per VM (w36-c); the orphan-write counter is process-wide
>   (conservative: it fails every VM's read);
> - the evacuator's refusal ledger is per heap as of this wave (diagnostic
>   only);
> - `young_in_place_vacated` is per heap and cleared at the next
>   collection's start;
> - a frozen or unread peer, or an unregistered frame, fails the ledger read;
> - a destination never lands on a pinned page (`subtract_spans` against
>   the pinned pages);
> - pinned objects are roots of the cycle;
> - the T-3 reserved-tail refusal precedes the arm.
>
> *Noted, not changed:* the sizer10 unrecorded-root refusal
> (`unrecorded_young_root_refuses_cycle`) is skipped on pinned cycles by
> design, since conservative words are legitimately interior. A PRECISE
> root naming an unrecorded from-space address on such a cycle is left
> unmoved, and its bytes may be zeroed at `finish_in_place_young_cycle`.
> That is the sizer10 producer hunt, unchanged.

## ORCHESTRATOR GATE RUN (2026-09-27, end of gen round 5): every pinned-copy row GREEN; netty rows red on a shared, non-pinned-copy bug; NOT flipped
**G1-G7 on the wave-5 build `g6w5c`** (the rows the STATUS below defines;
every run interleaved on and off):
`/data/wt-g6/gate-w5c/sum.txt` on the build host):

| Row | Result |
|---|---|
| G1 netty, 4 buffer classes | off 8/12 (3 × the pin-ledger read past the young end, since fixed in `d83130282`; 1 × `jit_monitor_enter`). On 37/40: 1 × `jit_monitor_enter`, and 2 × `BigEndianDirectByteBufTest.testInternalNioBuffer` timing out at 120 s on a loaded host, with walls 170 s and 279 s |
| G2 audit arm (`root-write-audit=ring`, stale-objref) and G3 option B (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1`) | 12/12 each, `stale=0 notown=0` |
| G4 QDox 4-thread under gc-stress | on 3/3, output identical to the off arm |
| G5 `GenR4W4JitWarmDivertProbe` | `moving=125 non_moving=0` 3/3; stress arm 3/3; off 3/3; same checksum |
| G6 `GenR4W4EvacThroughputProbe` | on 10-11 s, off 10-12 s, same checksum |
| G7 Spring Boot sample | on 3/3, off 1/1 |

**Netty rows again on the round-5 tip `c6c98c760`** (5 runs × 4 classes,
interleaved): **on 19/20, off (the default) 16/20**, and no timeouts.

Every one of the 5 crashes, on both arms, is the compiled monitor path:
[`../../internal/gc/gengc-r5w6-orch-jit-monitor-enter-gets-a-stack-address-as-its-vm-pointer-FIXED-20260928.md`](../../internal/gc/gengc-r5w6-orch-jit-monitor-enter-gets-a-stack-address-as-its-vm-pointer-FIXED-20260928.md).
That is `jit_monitor_enter` with a stack-address VM pointer, plus the
`LockSlots::lease` variant. The same bug crashes the default configuration
more often than the pinned copy.

**Verdict.** No row shows a defect of the pinned copy itself. The gate's
netty rows are still red because of the shared bug, so by the letter of the
gate the default was **not** flipped. Flipping now is a user decision.

To decide by the letter instead, re-run G1 after the monitor-path page is
fixed. Expect 20/20 on both arms.

## STATUS (2026-09-27, gen r5w5/pin9): the last crash signature is a validator bug, now FIXED; the gate below decides the flip in wave 6

**What the audit-arm crash was.** The orchestrator measured on `d8a690353`
(2 of 6 audit runs): `SIGSEGV addr=0x100000004`, `rax=0x100000000 r15=1`, pc
in `ObjectHeader::resolved_shape` under `is_object_address`. That is NOT the
instrument and NOT a dangling root. It is `GenerationalHeap::is_object_address`
sizing a conservative candidate through a FORWARD it never bounded:

- The candidate is the payload word of a `Value::Object` cell, a JIT field
  address held in `rbx=rsi=rdi=r14`.
- Its "mark" is the high half of a heap pointer, `0x7673`, which reads
  FORWARDED on a heap placed at `0x..3_xxxx_xxxx`.
- Its "second word" is the next cell's `Value::Int(1)` = `0x1_0000_0000`.
- `shape_source` followed it past `plausible_heap_pointer` and read the mark at
  `0x1_0000_0004`.

It depends on ASLR (1 heap placement in 16), which is why it came and went
between identical runs, between arms, and with the pinned copy off under
`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4` (the earlier "`PointerMap` signature",
same registers). The `addr=0x540e81f4` variant (`rax=0x540e81f0`) has the same
shape. Full analysis:
`../../internal/gc/gengc-r5w5-pin9-header-screens-follow-forwards-out-of-the-heap-FIXED-20260928.md`.

**Landed (`gc/src/gen_heap.rs`, default-on, a SIGSEGV becomes a correct "not
an object"):**

- `forward_chain_stays_inside` plus the screen in `is_object_address`: a
  candidate whose forward leaves this heap's committed arenas is declined,
  and `FORWARD_SCREEN_DECLINED` counts it.
- No real object can be declined: every forward a collector installs targets
  this pause's published arenas.
- The roots6 audit half: `young_vacated_probe` / `young_vacated_contains` see
  a pinned cycle's vacated bytes. That is flag-on only in effect.

**Found, not fixed:**

- The same hole in two old-gen screens (old9's region; the diff is on the new
  page).
- Nibble `0b1011` turns the same candidates into a 262 144-round spin per
  screen, a plausible cause of the 394 s / 500 s outliers:
  `../../internal/gc/gengc-r5w5-pin9-busy-looking-candidates-spin-a-quarter-million-rounds-FIXED-20260928.md`.

Neither blocks the flip by itself. But a timeout in G1 below that coincides
with a heap at `0x..b_xxxx_xxxx` is that page, not the pinned copy.

### The default-flip gate (all rows required; run on the merged wave-5 release build)

Setup: `JDK` = the JDK 25 home, `R=/data/cratonvm/apps/netty-suite-runner`,
`ON='CRATONVM_GEN_PINNED_YOUNG_COPY=1'`, `OFF='CRATONVM_GEN_PINNED_YOUNG_COPY=0'`,
`BASE='cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m'`.
Interleave ON and OFF runs (host timing noise is about 3x). Keep every `.err`.

**G0. Unit tests.** All must pass.

```bash
cargo test -j 5 -p cratonvm-gc --lib is_object_address_
cargo test -j 5 -p cratonvm-gc --lib the_forward_screen_stops_where_shape_resolution_stops
cargo test -j 5 -p cratonvm-gc --lib r4w5_pinned_young_copy
cargo test -j 5 -p cratonvm-gc --lib gc_quiescence::young_pin_ledger_tests
cargo test -j 5 -p cratonvm-gc --lib root_write_audit
cargo test -j 5 -p cratonvm-vm --lib a_ledger_that_licenses_moves_excuses_only_rewritten_slots
cargo test -j 5 -p cratonvm-vm --lib young_pin_frame_screen_excludes_rewritten_slots_and_keeps_interior_words_raw
```

**G1. Netty buffer matrix, plain.** 4 classes x 10 reps ON, and 4 x 3 OFF as
the reference:

```bash
cd $R
for i in $(seq 1 10); do
  for cls in AdvancedLeakAwareByteBufTest BigEndianDirectByteBufTest \
             BigEndianHeapByteBufTest DuplicatedByteBufTest; do
    start=$SECONDS
    env $ON CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 900 \
      cratonvm --compatible --java-home "$JDK" --Xmx 1500m -XX:+UseGenerationalGC \
      @common.args -Dcraton.batch=1 CratonRunner io.netty.buffer.$cls > $cls.on.$i.out 2> $cls.on.$i.err
    rc=$?; echo "$cls $i rc=$rc wall=$((SECONDS - start))s"
  done
done
```

Expected:

- 40 of 40 `rc=0`.
- Each class's passed/total equals its OFF run: BigEndianHeap `414/414`,
  BigEndianDirect `413/413`, Duplicated `416/416`, AdvancedLeakAware = its OFF
  count.
- No run's wall time above 2x the class's OFF median (Heap ~23 s, Direct
  ~54 s on `d8a690353`).
- `grep -c 'SIGSEGV\|panicked' *.on.*.err` is `0` for every file.

**G2. Netty audit arm.** BigEndianHeap and BigEndianDirect x 3 reps, G1's
command with `CRATONVM_DBG=root-write-audit=ring CRATONVM_DBG_STALE_OBJREF=1`
added.

- 6 of 6 `rc=0`.
- No `CRATONVM_DBG_STALE_OBJREF: stale ObjectRef detected` line.
- No `SCANNER LOCAL CHANGED` line.
- Every ring record with `written=true` reads `place=own-live`.

**G3. Arm D (option B, same ledger), pinned copy OFF.** BigEndianHeap and
BigEndianDirect x 3 reps, G1's command with
`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1` instead of `$ON`. Expected: 6 of 6
`rc=0`, same counts as G1. This arm shared the `0x100000004` signature; green
here confirms the validator was the common cause.

**G4. QDox, 4 threads, under stress.** 3 reps ON, 1 rep OFF:

```bash
env $ON CRATONVM_DBG=gc-stress=250000 cratonvm -XX:+UseGenerationalGC -Xmx256m \
  -cp <dir>:qdox-2.2.0.jar QDoxMtDriver tools/bench 4 60 > qdox.on.$i.out 2> qdox.on.$i.err; echo "rc=$?"
```

Expected: `rc=0` in each run, and `diff qdox.on.$i.out qdox.off.1.out` empty.

**G5. `GenR4W4JitWarmDivertProbe`.** 3 reps each:

| Command (append to `$BASE`) | env | Expected stdout |
|---|---|---|
| `-cp tools/bench GenR4W4JitWarmDivertProbe` | `$ON CRATONVM_DBG=gc-stats` | `PASS jitwarm threads=4 calls=1500000 checksum=-4668312146048759300` |
| `-cp tools/bench GenR4W4JitWarmDivertProbe 4 200000` | `$ON CRATONVM_DBG=gc-stress=250000` | `PASS jitwarm threads=4 calls=200000 checksum=-622814496315717380` |
| `-cp tools/bench GenR4W4JitWarmDivertProbe` | `$OFF` | the first line |

Engagement, from the gc-stats run:

- `moving-pinned-pages` covers at least 90 % of that run's young cycles. The
  reference is 126/126 on `d8a690353`, idle host.
- `pycopy_cycles=` on `[GC] young_pinned_copy:` equals it.

**G6. `GenR4W4EvacThroughputProbe` (pause and throughput).** 3 reps each,
interleaved:

```bash
env $ON  CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 $BASE -cp tools/bench GenR4W4EvacThroughputProbe
env $OFF CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 $BASE -cp tools/bench GenR4W4EvacThroughputProbe
```

Expected, both arms:
`PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`.
Gate:

- ON median wall time <= OFF median (reference 14.5 s OFF; 11.2 s with the
  parallel pinned arm).
- ON median young pause (`[gcpause]` lines of `moving-pinned-pages` cycles)
  <= 1.25x the OFF median of `nonmoving-*` young cycles.
- `pycopy_overflow_promotions / pycopy_cycles` recorded, not gated.

**G7. Spring Boot sample, exploded.** 3 reps ON, 1 OFF:

```bash
cd /data/sbjar-flat
env $ON cratonvm --compatible --java-home "$JDK" -XX:+UseGenerationalGC \
  -cp "$(cat cp.txt)" smoketest.simple.SampleSimpleApplication > sb.on.$i.out 2>&1; echo "rc=$?"
```

Expected: `Started SampleSimpleApplication` and `Hello Phil` in every run,
`rc=0`, start-up time within 1.25x of OFF.

**G8. The flip matrix rows 1–8 and the stale gate** (sections below,
unchanged). All expected lines in both arms. The stale gate's `cycle=pinned`
summaries count `0` not ending in `stale_live=0`.

**G9. Loaded host.** Repeat G5's first row ON while a `cargo build -j 5` runs.

- The checksum must hold.
- Engagement may fall (the documented load sensitivity: helper windows
  freeze peers). Record it, do not gate on it.

### Decision rule

Flip when G0–G8 are all green. G9 must be correct; its engagement is only
recorded.

The flip is the one-line diff from pin8's block below, in `types/src/flags.rs`:
`gen_pinned_young_copy` becomes
`non_empty_non_zero_default_true(src, "CRATONVM_GEN_PINNED_YOUNG_COPY")`, so
`CRATONVM_GEN_PINNED_YOUNG_COPY=0` is the switch back.

Its follow-ups, in the same commit:

- the field's doc ("Default **on** since <date>; `=0` turns it off");
- the unit tests that assume the default arm on a JIT-warm cycle is
  `nonmoving-unrewritable-conservative-jit-roots`: set
  `CRATONVM_GEN_PINNED_YOUNG_COPY=0` in them explicitly. Search
  `gen_heap::tests::r4w5_pinned_young_copy`,
  `gc_quiescence::young_pin_ledger_tests` and
  `vm/src/jit/conservative_roots.rs` tests for `young_pin_ledger_licenses_moves`.
  The last one changes meaning with the flip: the strict ledger rule becomes
  the default.

**Which failure means what:**

- A G1–G3 crash with `addr=<small>+4` and `[rax+r15*4]` after this fix: a
  screen other than `is_object_address`. `addr2line` the pc and check the
  open old-gen screens on the new page first.
- A crash in hashbrown or mimalloc: pin8's reading instructions below.
- A timeout: record the young base address (`/proc/<pid>/maps`). Bits 32..35
  = `0xb` points at the busy-spin page.

> **Earlier status (2026-09-27, gen round 5 orchestrator, final build;
> superseded by the block above): the flip stays
> BLOCKED.** Gate run on the round's last build (pin8's ledger fix in; the
> share sizer off), `CRATONVM_GEN_PINNED_YOUNG_COPY=1`, netty
> `io.netty.buffer` classes, first 12 runs: AdvancedLeakAware 3 pass / 1
> timeout (500 s), BigEndianDirect 2 pass / 1 timeout / 1 SIGSEGV
> (`addr=0x100000004`, the `PointerMap` signature), BigEndianHeap 3 pass /
> 1 SIGSEGV (`addr=0x540e81f4`, `[rax+r15*4]` with `rax=0x540e81f0`, pc in
> `GenerationalHeap::collect_garbage_inner_with_pins`; the same address as
> on the wave-4 build before the sizer revert, so not the sizer).
> BigEndianDirect, which crashed 2/2 in the split runs before pin8, passed
> 2/3; JIT-warm engagement is unchanged (126/126 cycles moving). With the flag
> off every one of these classes passes. Next step: the census and ring-audit
> runs on pin8's section below (`CRATONVM_DBG=root-write-audit=ring
> CRATONVM_DBG_STALE_OBJREF=1`) on the two crashing classes.


## Earlier status (2026-09-26, gen r5w4/pin8; superseded by the gen r5w5/pin9 block above): one "read but not rewritten" root class closed; still BLOCKED until the netty matrix below is green

**Verdict: do not flip yet.** This lane found and closed one defect that
fits the netty crash exactly, and added the three instruments that name the
writer if it is not the whole story. The flip waits for the matrix below.

**The defect is NOT specific to the in-place copy** (orchestrator split
runs on `897210f83`, netty buffer classes, 2 reps each):

| Arm | Flags | Result |
|---|---|---|
| A | `CRATONVM_GEN_PINNED_YOUNG_COPY=1` | BigEndianDirect SIGSEGV 2/2 (`addr=<aligned>+6`, pc suffix `...4f6`). The other classes pass. |
| B | A + `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1` | Still crashes, plus timeouts. Uninformative: that switch makes the ledger incomplete (no pinned cycle can run) and restores the blocked-peer remap hole on the ordinary Cheney path. |
| C | A + `..._PARALLEL=1` | Same crash. |
| D | pinned copy OFF + `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1` | BigEndianDirect SIGSEGV with the same signature; BigEndianHeap SIGSEGV `addr=0x100000004`. |
| — | no flags | Every class passes. |

Arm D shows the fault lies in any young cycle that MOVES objects while
compiled frames are live and would otherwise have diverted. The two routes
to such a cycle share one gate:

- the pinned copy moves around the pause ledger's words;
- option B runs a Cheney copy when that ledger is complete and EMPTY.

The defect below is in that ledger, so it explains all four arms. Under
option B, a dead-claimed word left the ledger empty, so the cycle moved the
object anyway.

**The defect (fixed, flag-on only).** The pin ledger EXCUSED compiled-frame
words on two deadness claims: a base in a Java-local / operand-spill slot the
active oop map does not name ("dead by the map"), and an operand-spill word
above the safepoint's live cursor. But the marking band scan
(`scan_one_frame_filtered`) READS the first class as a root, and nothing
rewrites either class (`remap_one_jit_frame` rewrites only the slots the map
names). Under the non-moving sweep a wrong deadness claim costs at most an
object whose ONLY reference it was. Under the pinned copy it relocates any
object the word names, and it does so even when the object stays reachable
through other references. The frame then resumes on the vacated address,
which the copy re-serves in the same pause. That is exactly "a root that is
read but not rewritten", and the known JIT liveness gaps
(`GenR5W2OsrDeadSlotProbe`, `WRONG_MAP`/`NEVER_MAPPED` oracle hits, stale
sp-id selection) supply wrong claims.

Now, whenever the ledger licenses moves (`CRATONVM_GEN_PINNED_YOUNG_COPY` or
`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`), only a word a channel REWRITES is
excused. The census-only ledger (both flags off) keeps its old rule, so the
default path is byte-identical. The change is in `young_pin_slot_excuses`,
`young_pin_frame_words` and `young_pin_scan_own_chain` in
`vm/src/jit/conservative_roots.rs`. It is recorded in
`gengc-r5w4-pin8-the-pin-ledger-spent-deadness-claims-the-marking-scan-does-not-trust-20260926.md`.

**Everything else the pinned pause writes was re-read. None of it can reach
a Rust frame or a Rust heap block.**

- Copies, forwarding words, zeroing, fillers and the free-list rebuild stay
  inside `[young_base, young_base + capacity)`. `young_base` is captured
  after the only grow in the cycle, which is to-space's, and the destinations
  are committed before use.
- The JIT remaps, the register images, the native-slot write-back and the
  shadow homes are all value-gated on a `pointer_map` key and bounded to the
  writer's own live stack.
- `verify_precise_covers_conservative` only READS between building its sets
  and dropping them, and every writer above runs either before or after it,
  never during.

So the victim set is most likely incidental: the allocator's own metadata is
damaged by a stale-reference store (for netty, a clobbered `ByteBuf` field
driving an `Unsafe` store into native memory). The per-write table is in the
lane report `docs/internal/reviews/gengc-round5-w4-pin8-20260926.md`.

**Instruments (landed; all default-off):**

1. **`CRATONVM_DBG=root-write-audit=ring`.** The same audit, but nothing is
   printed per write. Stores go to an in-memory ring (the last 16 384, plus
   the first 1 024 that are not `own-live`), which is dumped at exit, before
   every audit abort, and from the crash handler once the cross-lane hook
   lands.
2. **The victim's own watch.** With the audit on in any mode,
   `verify_precise_covers_conservative` fingerprints its two `HashSet`s when
   it builds them and checks them before they drop.
   - `SCANNER LOCAL CHANGED ... handle_changed=true` means a store into the
     scanning thread's own frame.
   - `handle_changed=false table_count a -> b` means a store into the set's
     table.
   - A crash in the drop with neither line means the allocator metadata was
     corrupted, so the set is not the target.
3. **The quarantine** (`CRATONVM_DBG_STALE_OBJREF=1`). This is the roots6
   proposal, as an extension of the existing flag. A pinned cycle stamps every
   span it vacated as a walkable `int[]` filler whose body words read
   `0xdeadfa11deadfa11`, and keeps it off the free list for one cycle. Any
   `get_header` read of it panics with `stale ObjectRef detected at .. a
   pinned in-place young cycle moved this object and quarantined its old
   bytes (span [..) vacated by young cycle N)`. A compiled-code read gets
   poison: grep registers for `deadfa11`.

**What the orchestrator runs to decide the flip** (Linux, release build of
this branch; `R=/data/cratonvm/apps/netty-suite-runner`):

```bash
cargo test -j 5 -p cratonvm-gc --lib root_write_audit
cargo test -j 5 -p cratonvm-gc --lib r4w5_pinned_young_copy
cargo test -j 5 -p cratonvm-vm --lib young_pin_frame_screen_excludes_rewritten_slots_and_keeps_interior_words_raw
cargo test -j 5 -p cratonvm-vm --lib a_ledger_that_licenses_moves_excuses_only_rewritten_slots
cd $R
for cls in AdvancedLeakAwareByteBufTest BigEndianDirectByteBufTest BigEndianHeapByteBufTest; do
  for i in $(seq 1 10); do
    CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 900 \
      cratonvm --compatible --java-home "$JDK" --Xmx 1500m -XX:+UseGenerationalGC \
      @common.args -Dcraton.batch=1 CratonRunner io.netty.buffer.$cls > $cls.$i.out 2> $cls.$i.err
    echo "$cls $i rc=$?"
  done
done
```

Use the same extra environment the crashing runs had. If
`CRATONVM_DBG_VERIFY_OOP_MAPS` was set, keep it.

**Expected:**

- 30 of 30 `rc=0`.
- Each class's pass count matches its flag-off run.
- No wall time above twice the flag-off median (the 394 s outlier).

**Arm D must turn green too.** Run the same loop with
`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1` instead of the pinned copy. With the
strict ledger this arm should also go 30 of 30. A crash that survives in arm
D but not in arm A would point past the ledger. The remaining candidates for
that case are listed in the lane report:

- a named slot the remap does not reach;
- a blocked peer's interior or misaligned stack or register word, which
  option B never consults.

**Then two more runs, both required:**

- **Engagement.** Run one class with `CRATONVM_DBG=gc-stats`. It must show
  `moving-pinned-pages` above zero.
- **Rows 1–8 of the matrix below, with the stale gate, flag on.** Row 1's
  `moving-pinned-pages` should stay near the 129-of-129 reference. The
  stricter ledger pins more pages, so a large rise in
  `nonmoving-pinned-pages-over-bound` or `nonmoving-pin-ledger-incomplete` is
  the price. Record it; if it erases the gain, see
  `../../internal/gc/gengc-r5w4-pin8-proposal-exact-pins-for-dead-claimed-frame-words-REJECTED-20260928.md`.

**If a netty run still crashes:**

1. Rerun that class with
   `CRATONVM_DBG=root-write-audit=ring CRATONVM_DBG_STALE_OBJREF=1`.
2. Read the result:
   - A `stale ObjectRef ... quarantined` panic names the Java or native
     reader of a vacated address. Its backtrace is the missing root channel.
   - A `SCANNER LOCAL CHANGED` line names a stack or table writer. Match its
     `@addr` against the ring's `slot=` values.
   - Neither line, a crash still in the set's drop, and no `deadfa11` in the
     registers: the corruption is not a stale young reference. Take the
     page's no-mimalloc build next:
     `cargo build --release -p cratonvm-cli --no-default-features --features zgc`,
     with `MALLOC_CHECK_=3`.
3. A/B the fix on the base binary `897210f83`: add
   `CRATONVM_GC_NO_BAND_MAP_LIVENESS=1 CRATONVM_GC_DEAD_SPILL_ROOTS=0`. These
   switch off the same two claims, though they also change marking. If that
   makes the base binary green, the ledger fix is the cure.

**The flip (one line, NOT applied), once all of the above is green.** In
`types/src/flags.rs`:

```diff
-            gen_pinned_young_copy: non_empty_non_zero(src, "CRATONVM_GEN_PINNED_YOUNG_COPY"),
+            gen_pinned_young_copy: non_empty_non_zero_default_true(src, "CRATONVM_GEN_PINNED_YOUNG_COPY"),
```

The doc and test follow-ups are the ones the young5 block below lists.

## Earlier status (2026-09-26, gen r5w3/evac7; superseded by the block above): the flip is BLOCKED by the netty SIGSEGVs; the in-place copy's own writes stay in young; L1 now exists behind a flag

**Verdict: do not flip.** The orchestrator's data on `ebdc885de` with
`CRATONVM_GEN_PINNED_YOUNG_COPY=1` and no audit flag:

- **The upside is real.** `GenR4W4JitWarmDivertProbe` ran `moving=129` of
  129, all `moving-pinned-pages`, with `pycopy_overflow_promotions=0`.
  QDox 4x40 under gc-stress passed 2/2, and the Spring Boot sample starts.
- **Netty crashes.** `DuplicatedByteBufTest` passed 2/2, but 3 of the other
  4 netty buffer-class runs SIGSEGV (AdvancedLeakAware, BigEndianDirect,
  BigEndianHeap). One BigEndianHeap run passed but took 394 s against the
  usual ~52 s.
- **Two of the crashes share a signature:** `addr=<64 KiB/1 MiB-aligned>+6`.
  This is mimalloc reading a segment header while freeing a corrupt pointer.
  The frames are hashbrown `free_buckets`, reached from the drop of a local
  `HashSet<i16>` in `verify_precise_covers_conservative`, called from
  `scan_one_frame_precise` (`vm/src/jit/conservative_roots.rs`).
- **The audit hides it.** Under `CRATONVM_DBG=root-write-audit` the test
  passed 6/6 with zero anomalies, so the failure is timing-sensitive.

**What this lane re-read (the in-place copy, `gen_heap.rs`), and what it
rules out.** Every byte the pinned cycle writes lies in
`[young_base, young_base + capacity)`:

- **Copy destinations** are the start-of-cycle free blocks (filtered to
  `off + len <= used`) and a tail window capped at `capacity - used`, both
  committed before use.
- **The rebuild zeroes** `complement(live, used_at_start) - initial_free`,
  offsets below `used_at_start`.
- **Sliver fillers** go only between survivors below the new cursor.
- **Forwarding words** go only into from-space sources.
- **The free list** is re-published only below the new cursor.

The audit's three in-place kinds (`EvacuationCopy`, `InPlaceZero`,
`InPlaceFiller`) cover exactly these writes, and it reported zero anomalies.
The victim, by contrast, is the scanning thread's own Rust local, and no
collector write targets a Rust heap block or a Rust stack word. The local's
function also runs only under `CRATONVM_DBG_VERIFY_OOP_MAPS` (x86-64; armed
automatically on aarch64), so those runs had the oop-map oracle on.

That function does nothing between allocating the set and dropping it but
READ its own stack and the heap. So the set's pointer was most likely bad from
the moment it was allocated: mimalloc handed out a block from a page whose
free list a stray store had corrupted EARLIER, possibly on another thread. The
drop is then just the first point that notices. On this reading the victim
site is incidental, and the question is who wrote into the allocator's memory.

A build without the `mimalloc` feature
(`cargo build --release -p cratonvm-cli --no-default-features --features zgc`)
would detect the stray store closer to the writer. So would the `mimalloc`
crate's `secure` feature, which encodes and checks the free lists. Run the
same netty classes on either build, with glibc `MALLOC_CHECK_=3` for the
first.

**What the flag changes, so what the crash most likely is.** The pinned copy
is the first thing that makes JIT-WARM young cycles RELOCATE. Every root
channel that rewrites compiled-frame, register or native-stack state therefore
runs on cycles where it used to do nothing:

- the JIT-frame remaps (`remap_one_jit_frame`,
  `remap_one_frame_register_images`, value-gated, bounded to
  `[walk SP, stack top)`);
- the blocked-wake native-slot write-back (read-compare-write, own-stack
  bound);
- `apply_pointer_map_to_thread` at resume.

And the vacated young bytes are free-listed and re-served in the SAME pause.
A reference any channel failed to rewrite therefore lands on a NEW object
instead of faulting on a decommitted semi-space, as the Cheney path's stale
reference does. For netty that new object can be a buffer whose
`memoryAddress` or index then drives an `Unsafe` store into native memory,
and the allocator heap holding the scanner's `HashSet` table is one such
place (the roots6 quarantine proposal describes this shape). The lane found
no write in its own code that fits; the candidates are root channels in
`vm/src/jit/conservative_roots.rs` and `vm/src/threading/` (lane live7 /
not this lane's files).

**Runs that would split it** (none needs a code change):

1. **The same netty runs with `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1`** and
   the pinned copy OFF. JIT-warm cycles whose ledger is complete and empty
   then run the ordinary Cheney copy, whose vacated semi-space is decommitted.
   If the crash follows the Cheney cycles as a FAULT at a young address, the
   cause is a stale reference that some channel fails to rewrite, and the
   in-place reuse only turns the fault into corruption. If it vanishes, look
   at the in-place-only state (the identity entries, the in-place
   `pointer_map` values inside from-space).
2. **Without `CRATONVM_DBG_VERIFY_OOP_MAPS`** (if it was on). The victim
   moves: the table is merely the first Rust heap block a stray store hits.
3. **With `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1`.** Rules the
   blocked-wake write-back in or out.
4. **With the roots6 quarantine**
   (`../../internal/gc/gengc-r5w2-roots6-proposal-quarantine-vacated-young-memory-DONE-20260928.md`)
   once it exists. A stale store then faults inside a quarantined span, with
   the storing Java frame on the stack.

**What this lane changed on this path.** For the serial pinned arm:

- the arming was factored out (`arm_in_place_young_cycle`, same computation);
- the scans take a prefetch flag (off by default, `forward_ref_slots` itself
  when off);
- `zero_spans_parallel` now splits huge spans across workers. The set of
  bytes zeroed is identical; only which thread zeroes them changes.

None of these can cause or cure the netty crash; the last shortens the
pinned pause (L4). The PARALLEL pinned arm
(`CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL`, gate item 2's pause concern) is
new and default-off. It must not be flipped before the serial arm is
unblocked.

## Earlier status (2026-09-26, gen r5w1/young5; superseded by the block above): re-read; no correctness reason in the young code not to flip; four things the gauntlet must read

Re-read against `9e252c8b2` (the pinned branch after `divert_non_moving`,
`pinned_young_pages`, `InPlaceEvac`, `forward_object_impl`'s in-place arms,
`finish_in_place_young_cycle`). The probe matrix and the stale gate below are
right as written; nothing this lane changed alters them (the per-heap commit
screen and the grid probe are decision-identical on this path). What the
application gauntlet should additionally read before the flip:

1. **The flip widens the Linux register residual from the moving-no-JIT
   population to every JIT-warm cycle.** Blocked peers' MISALIGNED register
   words are filtered through `is_obj` before `record_peer_reg` on Linux, so
   they are neither censused nor pinned (this page's "Residuals"). Today
   JIT-warm programs are almost all `nonmoving-*`, so the hazard is confined
   to cycles with no JIT frames; with the flag default-on they relocate on
   nearly every cycle. Gate: the stale gate's `interior_relocated=` must read
   0 on the gauntlet's `cycle=pinned` summaries, not only `stale_live=0`.
2. **Pause time on a large young generation.** The pinned cycle is serial
   (limit L1) and zeroes dead from-space bytes inside the pause (L4), where
   the non-moving sweep it replaces has a parallel prefix. Gate:
   `CRATONVM_DBG=gcpause` medians of `moving-pinned-pages` cycles against the
   OFF arm's `nonmoving-unrewritable-conservative-jit-roots` cycles on a
   JIT-warm workload at `-Xmx2g` with 8 allocating threads; the flip should
   not raise the young pause median by more than noise.
3. **Early promotion (L2).** Watch `pycopy_overflow_promotions / pycopy_cycles`
   and the major count on the gauntlet; a high-survival JIT-warm phase tenures
   at age 1.
4. **The generated inventory already says default-on.**
   `docs/config/flag-inventory.md` lists `CRATONVM_GEN_PINNED_YOUNG_COPY` as
   `default-on | on` today, while the parse is `non_empty_non_zero` (off):
   the generator reads any row with `off_word` as default-on. The flip makes
   that row true; not flipping leaves it a lie (filed:
   `../../internal/gc/gengc-r5w1-young5-flag-inventory-default-column-misreads-off-word-FIXED-20260927.md`).

**The flip (one line, NOT applied):** in `types/src/flags.rs`,

```diff
-            gen_pinned_young_copy: non_empty_non_zero(src, "CRATONVM_GEN_PINNED_YOUNG_COPY"),
+            gen_pinned_young_copy: non_empty_non_zero_default_true(src, "CRATONVM_GEN_PINNED_YOUNG_COPY"),
```

(`non_empty_non_zero_default_true` keeps `=0` and `=` as off, matching the
current `=0` semantics; `on_unless_zero` would read `=` as ON.) With it, the
field's doc ("Default **off** ...") becomes "Default **on** since <date>;
`=0` turns it off", and the unit tests that assume the default arm is
`nonmoving-unrewritable-conservative-jit-roots` on a JIT-warm cycle must set
`CRATONVM_GEN_PINNED_YOUNG_COPY=0` explicitly (search
`gen_heap::tests::r4w5_pinned_young_copy` and
`gc_quiescence::young_pin_ledger_tests` for a default-arm assertion). The
`flag_groups.rs` row already carries `off_word: Some("0")`, so the generated
docs do not change.

---

Slug: `gengc-r4w6-pinstale6-pinned-copy-default-flip-gate`
Filed 2026-09-24 by generational GC round 4, wave 6, lane `pinstale6`.

- **Earlier status (superseded by the block above):** FIX LANDED and VERIFIED by the orchestrator (wave 6 final,
  binaries `w6m`/`w6n`; see "Orchestrator verification" at the end). Every row
  of the flip matrix passed. The default is NOT flipped yet: the application
  gauntlet (QDox 4-thread with its jar, Spring/Tomcat start-up) has not run,
  because neither is installed on the verification host.
- **Severity:** correctness, only with `CRATONVM_GEN_PINNED_YOUNG_COPY` on.
  It is the gate for making that flag the default.

## Location

`gc/src/gc_quiescence.rs`:
- `record_peer_reg`, `note_peer_reg_pin_word`;
- `YoungPinLedger::note_peer_reg_word` / `read`;
- `pause_young_pin_read`;
- `peer_stack_slot_values_into`, `peer_stack_slots_saturated`;
- `PEER_REG_STALE_LIVE`.

`gc/src/gen_heap.rs`:
- the pinned branch after `divert_non_moving` (the bound) and the
  `pinned_plan` build (the plan);
- the `[peer-reg-stale]` pairing: `PeerWordSource`, `PeerWordFate`,
  `peer_word_fate`, `peer_word_stale_live`;
- `pinned_young_pages` (the low-slack edge).

## Evidence

The wave-5 final binary ran with the flag on and
`CRATONVM_DBG_PEER_REG_PAIRING=1`. It printed `[peer-reg-stale] stale=125`
over 4 of 16 moving cycles, and every checksum held. The census could not say
whether any of those words would be read after resume.

## Where a captured word can come from on a relocating cycle

Read end to end from the code at `28f4acd3a`:

1. **Every capture site is in `vm/src/jit/xt_root_scan.rs`.**
   `PEER_REG_CAPTURE` is filled only by `record_peer_reg`. That function is
   called by the Windows take-over (`scan_context`: registers 0..=15, stack
   `0xfe`), the Windows helper window (`helper_window_pass`: registers 0..=15,
   stack `0xff`), the Linux take-over (`scan_slot_with_regions`) and the
   Linux helper window (`classify_parked_snapshot`). A peer that parks
   cooperatively at a safepoint is never read this way. It deposits its own
   callee-saved registers and its band into the pause's ledger
   (`conservative_roots::deposit_pause_young_pin_words`).
2. **A frozen peer, or a blocked peer with a compiled frame, rules out any
   relocation.** Either makes the take-over verdict non-`NONE`
   (`gc_and_alloc.rs`, `publish_takeover_verdict`: `frozen`,
   `helper_windows`). `pause_young_pin_read` then answers "incomplete", and
   the pinned copy declines. `takeover_forbids_unpinnable_move` also makes
   the coverage term divert the Cheney path.
3. **So on any cycle that relocates, every captured word belongs to a BLOCKED
   peer whose band held no JIT code** (`has_jit == false` in the helper
   window). Its registers and native stack belong to VM Rust frames that are
   parked inside a blocked region.
4. **No calling-convention argument makes those registers dead.** The thread
   was suspended asynchronously inside a blocked region. It is not at a call
   boundary, so even a caller-saved register may be live in the function it
   was suspended in. Whether a Rust frame holds a raw oop across a blocked
   region is a question about the handle discipline, not the ABI. The
   blocked-wake stack remap exists because such frames were found
   (`bytebuf-multiplethreads-npe`). So the answer to the gate's question is:
   **a stale register can be live at resume.** It cannot be proven dead, and
   nothing rewrites it.

The three populations inside `stale=`:

| population | channel | before this wave, on a pinned cycle |
|---|---|---|
| helper-window stack BASE (`reg=r255`) | the blocked-wake fold rewrites it at wake (exact `pointer_map` lookup) | repaired: counted as stale, but not live |
| blocked-peer REGISTER word (`reg=r0..r16`) | none | **live hazard** |
| any word INTO a relocated object (either population) | none (the fold matches exact bases only) | **live hazard**; counted under `interior=` |

## Fix (landed)

1. **Registers into the ledger, always.** `record_peer_reg` now screens every
   REGISTER word against the published young range, with or without the
   pairing flag, and hands it to `YoungPinLedger::note_peer_reg_word`.
   - The words are stamped with the pause, deduplicated and capped at
     `PEER_REG_PIN_WORDS_CAP` = 1024; an overflow makes the ledger incomplete.
   - `pause_young_pin_words` returns them with the deposits.
   - Result: a pinned cycle pins their pages, and option B can no longer clear
     term 4 over one of them.
2. **Non-base blocked-stack words into the plan.** Once the object-start
   bitmap exists, the pinned plan adds every value in the blocked-wake capture
   buffer (`peer_stack_slot_values_into`) that lies in from-space and is not
   an object start.
   - The decision's 1/8 bound is taken over the ledger words plus ALL of
     those values, so the plan can never exceed the bound it was checked
     against.
   - The capture buffer is process-global, so production reads it and the
     forced unit-test path does not.
3. **Fail closed where the stack capture is not whole.** The ledger reads
   incomplete when `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1` or when the
   capture buffer is saturated.
4. **The low-slack edge.** The ledger admits words in
   `[base - YOUNG_PIN_LOW_SLACK, base)` on purpose: they are base-minus-offset
   derived pointers to the first object. `pinned_young_pages` used to drop
   them. They now pin page 0.
5. **The census classifies.** Each captured word is split by source
   (`register` / `takeover-stack` / `helper-stack`) and fate (`RelocatedBase`
   / `RelocatedInterior` / `Untouched`). The summary line keeps its old
   fields and appends:

   ```
   cycle=pinned|cheney stale_register= stale_takeover_stack= stale_helper_stack_remapped= interior_relocated= helper_stack_remap=on|off stale_live=
   ```

   - `stale_live=` counts the words no channel rewrites and no pin keeps.
     With (1)–(3), it is **zero by construction on `cycle=pinned`**.
   - On `cycle=cheney` nothing is pinned, with the flag or without it. A
     non-zero value there is the exposure the default moving path already has
     (`moving-no-jit-frames-live`), and flag-on and flag-off should read alike.

## Residuals (not closed)

- **Misaligned words are captured by neither the census nor the pins.**
  - The helper-window stack capture takes `is_obj` words only, which on the
    generational heap means 8-aligned and in the heap.
  - The Linux arms also filter REGISTERS through `is_obj` before calling
    `record_peer_reg`.
  - So a misaligned interior word (a `*const u8` cursor into a `byte[]`) held
    by a blocked peer is invisible to both the census and the pins. The
    Windows register capture is whole.
  - Cross-lane request (Linux register half) is in
    `docs/internal/reviews/gengc-round4-w6-pinstale6-20260924.md` §6.
- **The default moving path pins nothing.** The Cheney path relocates under
  blocked peers' register and interior words, with the flag or without it.
  The pinned copy is the only mechanism that could pin them on every cycle.
  That is proposal 1 of the review.

## The flip matrix

Setup:
- `cratonvm` is this branch's release binary.
- `$JDK` is the JDK 25 home.
- `BASE='cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m'`.
- OFF is no environment. ON is `CRATONVM_GEN_PINNED_YOUNG_COPY=1`.

Run each row in both arms.

### Correctness

| # | command (append to `$BASE`) | env | expected line |
|---|---|---|---|
| 1 | `-cp tools/bench GenR4W4JitWarmDivertProbe` | — | `PASS jitwarm threads=4 calls=1500000 checksum=-4668312146048759300` |
| 2 | `-cp tools/bench GenR4W4JitWarmDivertProbe 4 200000` | `CRATONVM_DBG=gc-stress=250000` | `PASS jitwarm threads=4 calls=200000 checksum=-622814496315717380` |
| 3 | `-cp tools/bench GenR4W5PinnedYoungCopyProbe` | — | `PASS pinned threads=4 calls=600000 bad=0 checksum=-6144781693192940271` |
| 4 | `-cp tools/bench GenR4W5PinnedYoungCopyProbe 4 100000` | `CRATONVM_DBG=gc-stress=250000` | `PASS pinned threads=4 calls=100000 bad=0 checksum=8172764217083875233` |
| 5 | `-cp tools/probes MtChurnProbe 4 60 48` | — | `MTCHURN_OK threads=4 rounds=60 growMiB=48 wallMs=<timing, ignore> checksum=2350878600` |
| 6 | `-cp tools/probes MtChurnProbe 4 60 48` | `CRATONVM_DBG=gc-stress=250000` | same as 5 |
| 7 | `-cp tools/bench GenR4W6PinnedDefaultGauntletProbe` | — | `PASS gauntlet threads=4 calls=100000 depth=24 bad=0 checksum=45190562680832` |
| 8 | `-cp tools/bench GenR4W6PinnedDefaultGauntletProbe 4 20000` | `CRATONVM_DBG=gc-stress=250000` | `PASS gauntlet threads=4 calls=20000 depth=24 bad=0 checksum=4123483131904` |

### Engagement (ON arm, rows 1, 3 and 7, with `CRATONVM_DBG=gc-stats`)

- `[GC] decision histogram:` shows a `moving-pinned-pages` row above zero.
- `pycopy_cycles` on `[GC] young_pinned_copy:` equals that row.
- The wave-5 reference is 152 of 153 cycles on row 1.
- A large rise in `nonmoving-pin-ledger-incomplete` or
  `nonmoving-pinned-pages-over-bound` against wave 5 on row 1 is the cost of
  the wider pin set. Record it; it is not a failure.

### The stale gate (ON arm, rows 1, 3, 5 and 7)

Add `CRATONVM_DBG_PEER_REG_PAIRING=1`. Then:

```
... 2>&1 | grep '\[peer-reg-stale\] cycle summary' | grep 'cycle=pinned' | grep -vc 'stale_live=0$'
```

- The count must be `0`.
- The per-word `[peer-reg-stale] os_tid=` lines on pinned cycles must all
  read `src=helper-stack live=false`.
- For the record, not as a gate, keep the `cycle=cheney` lines of both arms.
  Their `stale_live=` distributions should match.

### Unit tests

```
cargo test -p cratonvm-gc --lib gc_quiescence::young_pin_ledger_tests
cargo test -p cratonvm-gc --lib gen_heap::tests::r4w5_pinned_young_copy
```

### Decision rule

Flip `gen_pinned_young_copy`'s default in `types/src/flags.rs`, and nothing
else, only when all of these hold:
- rows 1–8 print their expected lines in both arms;
- the stale gate reads `0` on all four probes;
- engagement is above zero on rows 1, 3 and 7;
- both unit-test modules pass.

The QDox 4-thread repro, `HashMapOnly` (4 threads) and `BinT 18` under
`gc-stress=250000` should be run with the flag on before the flip too. They
have no fixed expected line in this tree's probe docs, so compare them
against the OFF arm's output.

## Orchestrator verification (wave 6 final)

The merged binary includes this lane plus the orchestrator's fix for review6
finding M: `pinned_young_pages` now pins up to `YOUNG_PIN_LOW_SLACK` (256)
bytes ABOVE a word, not 64, so a base-minus-offset word keeps its object's
page. It also includes `=0` meaning off for `CRATONVM_GEN_PINNED_YOUNG_COPY`.

| Row | Result |
|---|---|
| `GenR4W4JitWarmDivertProbe`, flag on | checksum OK, `moving-pinned-pages=153` of 154 moving cycles |
| same, `=0` | checksum OK, no pinned cycles |
| same, `gc-stress=250000`, `4 200000` | checksum OK |
| `GenR4W5PinnedYoungCopyProbe` on, and on under stress | both checksums OK |
| `GenR4W6PinnedDefaultGauntletProbe` off / on / stress / `1 100000` | all four OK (on: 437 pinned moving cycles, 19 `nonmoving-coverage-incomplete`) |
| `MtChurnProbe`, `HashMapOnly`, `BinT 14`, flag on, under stress | all pass |
| stale gate (`CRATONVM_DBG_PEER_REG_PAIRING=1`), three probes | **708 `cycle=pinned` summaries, 0 not ending in `stale_live=0`** (wave 5 had stale=125 over 4 of 16 cycles) |
| `regression-suite/run.sh`, flag on | 107 of 108; the one failure, `RFileTimes`, fails identically with the flag off |

**To flip:** run the application gauntlet with the flag on, then flip it in
`types/src/flags.rs` (`gen_pinned_young_copy`) to a default-on opt-out, with
this table as the evidence. One side effect to expect: adaptive tenuring
(`CRATONVM_GC_ADAPTIVE_TENURING`) and `-XX:+PrintTenuringDistribution` only
see moving cycles, and JIT-warm programs have none without this flag.

**Load sensitivity, observed on `w6q`:** a `GenR4W4JitWarmDivertProbe` run
that overlapped a `cargo test -j 5` compile took 0 pinned cycles. All 150
were `nonmoving-coverage-incomplete`, with the reason
`xt-helper-window-conservative-scan` on each fallback. The checksum was
correct. Idle runs of the same binary took 155 of 155 pinned cycles, and so
did `w6m`. Under CPU contention, peers are more often caught inside a helper
window and frozen by take-over, which leaves coverage incomplete; the cycle
then safely stays non-moving. So the pinned copy's benefit depends on load,
and a flip evaluation should also run the matrix on a loaded host.
