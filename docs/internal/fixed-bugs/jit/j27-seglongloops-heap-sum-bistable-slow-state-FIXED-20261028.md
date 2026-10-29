# `R26SegLongLoops` `heap-sum` settles in a 2.4 ms/op state in most runs since the j27 flips

> **j27 wave 8 (lane W8FFM, 2026-10-28): REVIEWED.** No code change. `jit/src/ic_seed_claim.rs` and `vm/src/jit/helpers/fast_native.rs` `note_ic_profile_seed`: the claim is still taken once per method profile slot inside the VM budget (`claim_ic_seed_recompile`), and a `Refused` retry gives back both the slot flag and the budget unit (`release_ic_seed_recompile`, a no-op for a method holding no claim). A method whose retries keep being refused can ask again on each NEW receiver class at the armed site (the seed's own trigger), so the re-asks are bounded by the site's class count; `AlreadyQueued` keeps the claim (a compile is owed). Host-verified at s8 as below; nothing further.
>
> **j27 (lane HANGHUNT fix, 2026-10-28): FIXED IN CODE (candidate 2), HOST-VERIFIED s8 (2026-10-28).**
> - **Host result (snapshot 8)**, `R26SegLongLoops` `heap-sum`, 8 interleaved runs, 60 s timeout:
>   - s8 defaults: 0/8 hang (3.6-4.5 µs/op).
>   - `CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE=0`: 8/8 hang.
>   - Before the fix: s5 defaults hung 6/8, and the pre-flip base 0/8.
>   - The slow-run trace showed `ir-ic-profile-seed ... receiver class 808 already in the profile` (runs 3 and 4). The fast run did not.
> - **Host trace** (6 runs at 2000 iterations): every slow run has `ir-ffm-element-seed CALL ... heapSum` and only one `retry ENQUEUED`; no fast run has a seed CALL.
>   - The one ENQUEUED is the trap's eager C2 re-queue (`TRAP_RECOMPILE_KEEPS_C2`). That compile read the receiver row while it was still empty and built the seed-call body.
>   - The seed then found the row already filled by the resumed interpreter (`seed_ic_observed_receiver` returned false). Under the old rule that path asks for no recompile, so the starved body kept its IC call for good.
> - **Fix:** `CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE`, now **default ON**, `=0` kills.
>   - The seed asks for its one recompile even when the row was already filled (`ic_seed_claim::ic_seed_asks_recompile`).
>   - A `Refused` retry hands the claim back (`ProfileStore::release_ic_seed_recompile`).
> - **Candidate 1** (`CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_OWN_GUARDS_ONLY`) stays opt-in: the trace never showed WITHDRAWN.
> - **Verify:**
>   - 8 interleaved runs at the defaults should show 0/8 hangs. With `CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE=0`, the old rate (about 6/8) should return.
>   - In a slow-shaped run under `CRATONVM_DBG_JITC=1`, the `heapSum` lines should read in this order:
>     1. `ir-ffm-element-seed CALL`
>     2. `ir-ic-profile-seed ... already in the profile`
>     3. `ir-ic-profile-seed recompile requested`
>     4. `ic-profile-seed retry ENQUEUED`
>     5. `ir-ffm-element-inline PLANNED ... in R26SegLongLoops.heapSum`
>   - `time heap-sum` should be under 10 µs/op.

> **j27 (lane HANGHUNT follow-up, 2026-10-28): PARTLY, two opt-in switches for the host A/B, PENDING HOST VERIFICATION.**
> - **Candidate 1:** `CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_OWN_GUARDS_ONLY=1`. `ffm_element_inline::ffm_element_plan_withdrawn` (asked from `jit/src/lib.rs` in place of `ffm_element_guards_withdrawn`) withdraws the element plan only for the expansion's own ids at the bci (`UncommonTrap`, `NullCheck`, `ReceiverTypeChanged`). The wildcard and foreign ids no longer do.
> - **Candidate 2:** `CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE=1` (`jit/src/ic_seed_claim.rs`, `vm/src/jit/helpers/fast_native.rs` `note_ic_profile_seed`, `ProfileStore::release_ic_seed_recompile`). Two changes:
>   - The seed asks for the recompile even when the interpreter filled the row first. That is the race: such a row used to answer `seeded == false`, and then nothing ever asked.
>   - A `Refused` retry hands the claim back.
> - **Verify:** run 8 interleaved runs per arm at the defaults: `=1` for each switch alone, then both together. Count the runs that hang, and compare against 6/8 for the defaults.

> **j27 (lane HANGHUNT, 2026-10-28): NOT FIXED, analysis by reading only, PENDING HOST TRACE.** No code change. The mechanism is narrowed to three candidates. Each makes a decision early and never revisits it. The recipe below tells them apart.

| | |
|---|---|
| **Status** | FIXED IN CODE, HOST-VERIFIED s8 (2026-10-28): `CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE` default ON (lane HANGHUNT fix). Was OPEN (perf, bistable; default arms since `85c7a7e7ef`). |
| **Area** | JIT tiering / FFM element planning for `R26SegLongLoops.heapSum(Ljava/lang/foreign/MemorySegment;I)I` |
| **Probe** | `tools/probes/jit/r26/R26SegLongLoops.java`, `--jdk-only`, Linux |

## Evidence

- Host, 8 interleaved runs, 60 s timeout: base 0/8 hang; flips binary with all 59 switches `=0` 0/8; flips binary at defaults 6/8. The two runs that finished were 6.9-7.1 µs/op, as fast as base.
- **The state already existed before the flips, at a lower rate.** `docs/internal/proposals/jit/addr21-1-real-pointers-option-a-20261026.md` (2026-10-27) records one base-code `--jdk-only` run that "hung in heap-sum until the 300 s timeout". Three re-runs of it passed. So the flips raise the probability of a latent race; they do not create the mechanism.
- **The single-switch kill table is mostly noise.** `CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER` is inert at the defaults. It is read only inside `if door_block` (`jit_bridge.rs`, the `JitEntryGuard::enter_door` arm), and `CRATONVM_JIT_DOOR_THREAD_BLOCK` stays default OFF (`vm/src/config.rs`). Yet killing it "usually made it fast". So any one-switch result from a few runs mostly measures run-to-run variance. `CODE_ENTRY_COLOUR` (it only moves code start) and the pure IR switches most likely act through compile time and code layout too.
- **What the slow state costs.** 2.4 ms per 4096-element op is about 590 ns per element. That matches the measured cost of "a generic inline-cache call into the JDK accessor (~0.8 us per element)" (`ffm_element_inline.rs`, doc of `ffm_seed_call_site_arms_in_loop`, `T1J6FfmSeedRows`). It is about 8x the cost of a declined-to-call spliced site (72 ns, addr21: native-put 295,708 ns/op) and far above the single-pass intrinsic route (about 45 ns, `HgZero`). So in the slow state the `getAtIndex` call at bci 16 either runs as a MIC/PIC dispatch into `AbstractMemorySegmentImpl.getAtIndex` from a compiled `heapSum`, or `heapSum` runs interpreted.

## Candidates (each decides once and never revisits)

1. **De-spec withdrawal at the accessor bci.** `ffm_element_guards_withdrawn` (`ffm_element_inline.rs`) counts ANY id other than the site's own `UnstableIf` trap as withdrawn, the wildcard included. Once one charge at `heapSum` bci 16 reaches the registry, every later compile prints `ir-ffm-element-inline WITHDRAWN` and leaves the site unplanned for the life of the VM, because the registry never forgets. Which trap fires first depends on timing: an OSR entry mid-loop, or an exit at bci 16. The flipped IR switches decide which guards exist there and how a trap is attributed (`IR_BCE_NE_EXIT_VAR_START` with an OSR-variable start, `IR_CHAIN_LOOP_PROOFS`, `IR_CHECKCAST_HIT_TO_JOIN`, `EA_ROUND_NULL_COMPARE_FOLD`, `IR_OSR_RECHECK_CLASS_PREDICATES`, `IR_DEOPT_IMAGE_CLAIM`).
2. **The seed-call road and its one-shot recompile.** After a trap, `TRAP_RECOMPILE_KEEPS_C2` (flipped) re-queues C2 straight away. That compile can read the profile before the resumed interpreter records a receiver, so the site becomes `ir-ffm-element-seed CALL`, an inline-cache call. Only one recompile can lift it: `note_ic_profile_seed` (`vm/src/jit/helpers/fast_native.rs`). That helper spends `ProfileStore::claim_ic_seed_recompile` once per method, for the life of the VM, BEFORE `request_c2_retry` answers. A `Refused` answer (`c2_bailout`, `ineligible`, `tier_fail_count exhausted`, `admit` refusing) therefore leaves the IC call for good. So does a recompile that still finds no plan, for example because of candidate 1. This is the documented "bimodal per run" shape.
3. **The OSR door and the profit exit.** The memo forget/rebuild arms (`some_first_call_cell_fired`, the static-init forget) and `queue_superseded_optimizing_osr_bodies` / `arm_superseded_optimizing_osr_exits` (`OSR_PROFIT_EXIT_SUPERSEDED_OPTIMIZING`) work together with `OSR_ROUTE_OWN_REFUSAL_KEY`. If `heapSum` has no method-entry body, every call enters at its header through the door. A forget, an armed-hit refusal or a rebuild on each entry then costs an interpreted stretch per call. A count that grows with the iteration count would show this.

## Recipe (Linux host, one binary, slow runs only; each step stands alone)

Use iteration count `2000`. The probe then exits: the slow state is about 40 s, the fast one under 1 s. Keep only runs whose `time heap-sum` exceeds 100000 ns/op, and compare them against a fast run of the same arm.

```
export JAVA_HOME=<jdk25>; P="cratonvm --jdk-only -cp out R26SegLongLoops 2000"
# 0. Does not disturb the timing: what runs interpreted (census at exit)
CRATONVM_DBG_INTERP_FRAMES=1 timeout 120 $P > s0.out 2> s0.err
grep -E 'heapSum|AbstractMemorySegmentImpl.getAtIndex|VarHandleSegmentAsInts' s0.err
# 1. One line per publish
CRATONVM_DBG=jit-compiled,tierup-decline timeout 120 $P > s1.out 2> s1.err
grep heapSum s1.err
# 2. Planner and tier decisions (prints a lot: repeat until one run is slow)
CRATONVM_DBG_JITC=1 timeout 300 $P > s2.out 2> s2.err
grep heapSum s2.err | grep -E 'ir-ffm-element-(inline PLANNED|inline WITHDRAWN|plan MISS|seed CALL|unreached TRAP)|ir-ic-profile-seed|retry (REFUSED|ENQUEUED)|refused-publication|osr optimizing memo FORGETS|superseded QUEUED|OSR exit ARMED|outside-c1-drain-c2' | sort | uniq -c
# 3. Deopts (known to tip the run fast: keep only slow runs)
CRATONVM_DBG_DEOPT=1 timeout 300 $P > s3.out 2> s3.err
grep heapSum s3.err | grep -E 'per-bci de-spec|wildcard|bci=16|MakeNotCompilable|Recompile' | sort | uniq -c
```

How to read it:
- Step 0 tells the two slow shapes apart. A large count of `heapSum` frames (about one per call) means `heapSum` runs interpreted: candidate 3, or a not-compilable method. A large count of `getAtIndex` frames with few `heapSum` frames means a compiled `heapSum` calls the accessor: candidates 1 and 2.
- Candidate 1 is confirmed by `ir-ffm-element-inline WITHDRAWN ... heapSum` in step 2 together with `per-bci de-spec: R26SegLongLoops.heapSum... bci=16` in step 3, or the wildcard there. The fast runs must show neither line. The first trap's reason names the guilty guard.
- Candidate 2 is confirmed by `ir-ffm-element-seed CALL ... heapSum` with no later `ir-ffm-element-inline PLANNED ... heapSum`. Look also for `ir-ic-profile-seed recompile requested`, followed by `ic-profile-seed retry REFUSED` or by a recompile that prints `plan MISS` again.
- Candidate 3 is confirmed if the counts of `FORGETS`, `superseded QUEUED` and `OSR exit ARMED` lines for `heapSum` grow when the iteration count goes from 2000 to 4000.

## What would fix it (once the trace names the candidate)

1. Do not withdraw the element plan on a wildcard that a non-guard exit charged: name the charge, or skip exits that no expansion guard owns.
2. Claim the seed recompile only when `request_c2_retry` answers `Enqueued` (or owed). Let a recompile that still finds no plan keep the claim open with a bound.
3. Make the forget/re-arm cycle bounded per method.
