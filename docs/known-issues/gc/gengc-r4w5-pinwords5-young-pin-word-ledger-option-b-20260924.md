# The per-pause young pin-word ledger, and option B of the conservative-JIT-root divert

> **STATUS (2026-09-29, gce e1/x): KEEP -- no e1 row runs `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`.** **Remaining:** the triage decision (option B's netty loop arm).

> **STATUS (2026-09-29, gce e1/y, review only): unchanged -- working as designed, opt-in, OPEN only for the triage decision.** Re-read the consumer side on `adb9178bc` (`term4_population` / `term4_ledger_cleared` in `collect_garbage_inner_with_pins`, `gc_quiescence::young_pin_ledger_clears_term4`): no change and no defect. Option B's netty arm (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1`, 30/30) is still the gate on `gengc-r5w4-pin8-...`; nothing this round changed alters it.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`): unchanged -- working as designed, opt-in (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`, default OFF), OPEN only for the triage decision.** Option B's arm of the netty loop (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1`, 30/30, gate on `gengc-r5w4-pin8-...`) was not run on d7; the battery's `jitwarm_divert_term4` is =HS.

## STATUS (2026-09-27, gcd d2/h): re-verified on `a1fa77603`; working as designed, opt-in (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`, default OFF); no defect found; nothing changed this wave

- The predicate is unchanged: `gc_quiescence::young_pin_ledger_verdict` =
  `enabled && read.complete && read.words == 0`, evaluated through
  `young_pin_ledger_clears_term4()` as `term4_ledger_cleared` in
  `gen_heap::collect_garbage_inner_with_pins` (gcd d1/e split term 4 into
  `term4_population` and `term4_ledger_cleared`). It still runs only on
  cycles term 4 would divert, and records the census flag on or off.
- Since gcd d1/e (pin8, fixed), a cycle option B lets through also pins the
  blocked peers' interior native-stack words (`plan_takes_blocked_peer_words`),
  so option B is now safe to use as an A/B arm.
- What it cannot do, and why the orchestrator's measurement still reads
  `cjdiv_ledger_cleared` near 0 on a JIT-warm loop: a compiled hot loop holds
  young references in registers across every allocation, so a complete
  ledger is never empty; and option B, like the pinned copy, is a term-4
  hatch only. A cycle diverted by term 3 (a helper window or frozen peer,
  `takeover_forbids_unpinnable_move`) is never evaluated here. See the gcd
  d2/h STATUS of
  `gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926.md`
  and the proposal `../../internal/gc/gcd-d2h-proposal-pinned-copy-takes-helper-window-cycles-DONE-20260928.md`.
- Probe unchanged (the "Probe" section below; step 2 as corrected by the
  r5w1 block): `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1 CRATONVM_DBG=gc-stats
  cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4JitWarmDivertProbe`
  prints `PASS jitwarm threads=4 calls=1500000 checksum=-4668312146048759300`,
  and `[GC] young_conservative_divert:` shows `cjdiv_ledger_evals` equal to
  the flag-off arm's `cjdiv_diverts` within load noise. Opt-in page: the
  orchestrator's triage, not a defect.

## Earlier status (2026-09-26, gen r5w1/young5; superseded by the block above): working as designed; the orchestrator's measurement explained; probe step 2's reading corrected

**The measurement** (orchestrator, dev tip `9e252c8b2`, Linux release,
`CRATONVM_DBG=gc-stats -Xmx256m GenR4W4JitWarmDivertProbe`):
default `moving=0 non_moving=118` = 90 `nonmoving-coverage-incomplete` + 28
`nonmoving-unrewritable-conservative-jit-roots`; with
`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1`, `moving=0 non_moving=118`, all 118
term 4, `cjdiv_ledger_incomplete=0`.

**Why option B converted nothing: C = 0 is the prediction of this page's own
probe step 2**, not a defect. Option B stands term 4 down only on a pause
whose ledger is complete AND EMPTY (`young_pin_ledger_verdict`:
`enabled && read.complete && read.words == 0`). This probe's compiled hot loop
holds young references in its locals and registers across every allocation,
so every complete ledger holds words. `cjdiv_ledger_incomplete=0` says the
ledger was complete on all 118; the words are why it did not clear. The
mechanism that CAN move these cycles is the pinned copy
(`CRATONVM_GEN_PINNED_YOUNG_COPY`), which pins exactly those words' pages
instead of needing none: the orchestrator's
`moving-pinned-pages=88 of 88` on `GenR4W5PinnedYoungCopyProbe` is that.

**Why 90 coverage-incomplete cycles "moved" into term 4: they did not move
because of the flag.** The flag is read in exactly one place,
`gc_quiescence::young_pin_ledger_clears_term4`, the LAST conjunct of
`unrewritable_conservative_jit_roots`, which is evaluated only on cycles whose
coverage proof already passed (`moving_young` true). It cannot change a
coverage verdict. The coverage-incomplete population is load-sensitive: a
peer caught inside a JIT helper window is frozen by the take-over and the
cycle is `nonmoving-coverage-incomplete` with the
`xt-helper-window-conservative-scan` sub-reason (the pinstale6 page's
"Load sensitivity" note measured 150 of 150 such cycles on a run overlapping a
`cargo test -j 5`, and 0 on idle runs). The two runs differ in load, not in
the flag. To confirm, interleave the two arms ABBA on an idle host: the
coverage-incomplete counts must agree between arms within noise, and the
default arm's sub-reasons (the `unproven` column of the decision histogram)
must be take-over / helper-window.

**What would make option B convert cycles:** a workload whose compiled frames
are idle, blocked or walking old data at the pause (page step 2). And, for the
words it does see, the `cjdiv_ledger_native_sum` split: if the ledger's words
are mostly the layout-free native band (stale Rust helper-frame slots, "Known
limits" below), the precise lower bound for helper frames (the SP at the
JIT->Rust transition) is the lever; if they are compiled-frame words, only the
pinned copy helps. Read `cjdiv_ledger_words_sum`, `_max` and `_native_sum`
from the same run's `[GC] young_conservative_divert:` line.

Probe step 2's reading, corrected: C (`cjdiv_ledger_cleared`) near 0 on
`GenR4W4JitWarmDivertProbe` PASSES the step; the checksum line
`PASS jitwarm threads=4 calls=1500000 checksum=-4668312146048759300` is the
correctness half.

---

*Filed 2026-09-24 by generational GC round 4, wave 5, lane `pinwords5`.*

- **Earlier status (superseded by the block above):** FIX LANDED, awaiting probe — see "Probe" below for the command
  and the census line expected to change.
- **Severity:** perf (term 4 diverts essentially every JIT-warm young cycle);
  correctness of the new path rests on the completeness argument below.
- **Flag:** `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4` (token
  `gen-young-pin-ledger-term4`, group GC). Opt-in, default OFF. With it off,
  nothing decides differently; the ledger is still built and counted.

## What landed

`gc/src/gc_quiescence.rs`, one self-contained block at the end of the file:

```rust
/// Every conservative word published for the CURRENT pause that lies inside a published
/// young region and that no precise channel rewrites (interior and derived words included,
/// NOT resolved to object bases). Appends to `out`. Returns false when the ledger is
/// incomplete for this pause (a thread's scan did not deposit); the caller must then divert.
pub fn pause_young_pin_words(out: &mut Vec<usize>) -> bool
```

plus `young_pin_ledger_clears_term4()` (option B's predicate, the last conjunct
of `unrewritable_conservative_jit_roots` in
`gen_heap::collect_garbage_inner`) and `young_pin_ledger_census()`.

### The pause stamp (the reset-ordering prerequisite)

The ledger is never cleared. `open_young_pin_pause()` advances a stamp, called
from `reset_peer_proven_jit_depth()`. That function runs only inside the
barrier's request core (`request_stw_counted_locked`). The core runs it under
the barrier lock, after the "already requested" check and before
`stw_requested` is stored, so the stamp moves strictly before the first
deposit of a pause and strictly after the last deposit of the previous one.
Each deposit records the stamp current when it began. A reader keeps only
deposits whose stamp equals the current one:

| slot state | meaning | reader |
|---|---|---|
| stamp == current | published for THIS pause | words + depth counted |
| 0 < stamp < current | stale, from an earlier pause | ignored (no words, no depth) |
| stamp == 0 | never published, or a deposit in progress | ignored |

`begin_moving_young_coverage_cycle` does NOT touch the stamp, so neither it
nor any of the initiator's `collect_roots` clears can erase a peer's deposit.
A deposit writes its words into preallocated atomic cells behind a seqlock on
the stamp, so it never allocates, and a torn read is detected and treated as
incomplete.

### Who deposits (`vm/src/jit/conservative_roots.rs`, `deposit_pause_young_pin_words`)

- **Each cooperatively parking peer** deposits in
  `publish_peer_jit_coverage_for_stw`, right after its own coverage proof
  deposited its depth. Its next step is `arrive_and_wait`.
- **The initiator** deposits in `refresh_moving_young_coverage_for_collection`.
  `memory::roots::collect_roots` calls that after every counted peer has
  parked.

A deposit holds every word in a published young region that meets one of
these conditions. A word is in a young region if it lies in
`[base - 256, end]`: the low slack catches base-minus-offset derived pointers,
and the end is inclusive.

1. **Registers.** It is in one of the thread's callee-saved GPRs at the
   deposit. On x86-64 these are rbx, rbp, rsi, rdi and r12–r15; on AArch64,
   x19–x29. Other targets mark the deposit incomplete.
2. **Layout-free bands.** It lies in the JIT band
   `[deposit SP, outermost entry SP)` but outside every compiled frame whose
   layout the walk resolved. This covers:
   - the Rust helper frames that compiled code called into (their prologues
     saved the compiled frame's callee-saved registers);
   - frames with no usable layout;
   - foreign innermost frames;
   - conservative (non-precise) chain entries.
3. **Compiled frames.** It lies in a resolved compiled frame's band and is not
   excused by `young_pin_slot_excuses`. Only three cases are excused:
   - an object base in a slot the active oop map names, because
     `remap_one_jit_frame` rewrites that slot;
   - an object base in a dataflow-modelled slot that the resolved map proves
     dead;
   - an operand-spill slot above the safepoint's live cursor.

   The following are always counted:
   - register images, the blind spill and the outgoing-argument reserve;
   - unmodelled slots the map does not name;
   - every word when no map resolved;
   - every non-base (interior or derived) word.

   This rule is stricter than `band_slot_is_verifiable_with_map`'s split, on
   purpose. "Verifiable" means the coverage proof inspects the slot for shadow
   publication. It does not mean a channel rewrites the frame word, and the
   ledger needs the second.

Words are stored raw and de-duplicated per thread. The cap is 256 per thread
per pause; overflow makes the ledger incomplete.

### The completeness argument (the safety property)

`pause_young_pin_words` returns `true` only if every one of these holds:

1. **The deposits account for every JIT entry.** Add up the chain depths that
   this pause's deposits reported. The sum must be at least
   `gc_quiescence::depth()`, the process-wide count that `JitEntryGuard` keeps
   through `enter()`/`leave()`. That count equals the sum of every thread's
   `JIT_ENTRY_CHAIN` length.
   - Each thread owns one slot and a new deposit replaces its old one, so no
     thread is counted twice.
   - A thread in compiled code that did not deposit leaves a shortfall. This
     covers a BLOCKED peer (`vm_exec::deposit_root_snapshot`), a peer whose
     park path skips `publish_peer_jit_coverage_for_stw`, and an unproven peer.
   - The depth read can only fall between a deposit and the read (a prune
     lowers it), which is the safe direction.
2. **No current-stamp deposit overflowed,** hit a truncated band, failed to
   capture registers, failed to borrow its chain, or was torn.
3. **The young regions are published.** Otherwise the range screen was
   vacuous. This is always true on the generational heap.
4. **The take-over verdict is `NONE`.** A frozen, helper-window or unread peer
   was read by the initiator into pin sets, never into this ledger.
5. **No thread reported uncovered frames.** Specifically:
   - the initiator reports no unregistered (guardless) compiled frame;
   - no thread's coverage proof marked the cycle incomplete (this is how an
     unregistered frame on a peer shows up).

What the argument rests on, stated so it can be checked:

- **Every young cycle's pause goes through `request_stw_counted_locked`.**
  Since gc-common w2-a every GC door goes through `run_collection_pause` and
  `request_stw_opening_cycle`. A collection that ran without a request would
  see the previous pause's deposits as fresh. `PEER_PROVEN_JIT_DEPTH` makes
  the same assumption.
- **The two deposit sites read the thread's stack exactly as it will resume.**
  A peer deposits at its park, and only `arrive_and_wait` runs after the
  deposit. The initiator deposits during `collect_roots`, and its callers'
  frames are stable after that.

## Known limits

- **Stale stack data is counted.** The layout-free sweep counts what earlier
  calls left in uninitialised slots of live Rust frames. On the initiator it
  also counts the collector's own call chain down to `collect_roots`. Both
  over-count, which is the safe direction, but they can keep the ledger
  non-empty on cycles where no compiled frame holds anything young. The census
  splits this out as `cjdiv_ledger_native_sum`. If that dominates, the next
  step is a precise lower bound for the helper frames: the SP at the
  JIT→Rust transition, which the helper-entry path would have to publish.
- **Callee-saved XMM registers are not captured.** On Windows these are
  xmm6–15. This relies on the argument that `deopt::try_resolve_value` never
  reads an object from an XMM register.
- **The opt-in above-chain band is not included.** This is
  `CRATONVM_JIT_ABOVE_CHAIN_SCAN`: the interpreter/Rust frames above the
  outermost entry. Those frames call INTO compiled code, so they hold none of
  its registers.
- **Both young semispaces count.** A word into the empty to-space is stale
  and over-counts. `pinned5` filters to from-space itself.
- **The census is process-wide,** like the ledger. With two generational VMs
  in one process, the census mixes their cycles.
- **`evals` assumes one term-4 evaluation per young cycle.** It counts
  evaluations of the term-4 predicate, which equals would-be term-4 cycles
  only if `collect_garbage_inner` evaluates the term once per cycle.

## Probe

1. Price it with the flag OFF, on the wave-4 final binary's probe:

   ```
   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4JitWarmDivertProbe
   ```

   The new keys on the `[GC] young_conservative_divert:` line:
   - `cjdiv_ledger_evals` equals `cjdiv_diverts` (151 on the wave-4 run);
   - `cjdiv_ledger_cleared=0`;
   - `cjdiv_ledger_empty`: how many of those cycles option B alone would have
     let copy;
   - `cjdiv_ledger_incomplete`, `cjdiv_ledger_words_sum`,
     `cjdiv_ledger_words_max` and `cjdiv_ledger_native_sum`: why the rest
     would not.

2. Engage it with the flag ON:

   ```
   CRATONVM_DBG=gc-stats CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4JitWarmDivertProbe
   ```

   Expected: `PASS jitwarm threads=4 calls=1500000 checksum=-4668312146048759300`.
   On the census line, `cjdiv_ledger_cleared=C` and
   `cjdiv_diverts = cjdiv_ledger_evals - C`. In the `[GC] decision histogram:`,
   `nonmoving-unrewritable-conservative-jit-roots` falls by C and the moving
   reason rises by C.

   **Expected value of C on this probe: near 0.** The probe's hot method keeps
   young references in locals across allocations. That is exactly the
   population the design page says option B cannot unlock (it "does NOT help
   the allocation loop whose registers hold young pointers"). A workload whose
   compiled frames are idle, blocked or walking old data is where C should be
   large.

3. `--nojit` control: `cjdiv_ledger_evals=0`.

4. Correctness with the flag ON, on the list that gated term 4 in the first
   place:
   - the QDox 4-thread repro (the original 3/3 SIGSEGV);
   - `MTChurn`;
   - `HashMapOnly` with 4 threads;
   - `BinT 18` under `CRATONVM_DBG_GC_STRESS=250000`;
   - the `[peer-reg-stale]` census, which must read `stale=0`.

   On any failure, the flag stays off and this page records the failing word.

Unit tests:
- `cargo test -p cratonvm-gc --lib gc_quiescence::young_pin_ledger_tests`
- `cargo test -p cratonvm-vm --lib jit::conservative_roots::tests::young_pin_frame_screen_excludes_rewritten_slots_and_keeps_interior_words_raw`

---

## 2026-09-24 round 4 wave 6 (lane `pinstale6`): the ledger gains peer register words

Status unchanged: **FIX LANDED, awaiting probe**. The ledger's population
grew in three ways, all in `gc/src/gc_quiescence.rs`.

- **Peer register words.** No thread deposits these words for itself. They
  sit in the register file of a peer that the cross-thread scan read from
  outside, and nothing rewrites a register file. `record_peer_reg` now runs
  on every call, not only under `CRATONVM_DBG_PEER_REG_PAIRING`. It sends
  every REGISTER word (index < `PEER_REG_STACK_TAKEOVER`) that lies in a
  published young region to `YoungPinLedger::note_peer_reg_word`.
  - The words are stamped with the pause, deduplicated and capped at
    `PEER_REG_PIN_WORDS_CAP` (1024). One word past the cap makes the ledger
    incomplete.
  - They credit no JIT depth.
  - `YoungPinRead::peer_regs` counts them, and they are also counted in
    `native`.
- **Two new incompleteness conditions** in `pause_young_pin_read`:
  - the blocked-peer stack remap is off
    (`CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1`);
  - the peer stack-slot capture buffer is saturated.

  In either case the blocked-peer stack words are neither rewritten nor
  pinnable.

**What this changes for option B.** A pause whose only young words are a
blocked peer's register values is no longer "complete and empty". Term 4
therefore keeps diverting that cycle, or, with
`CRATONVM_GEN_PINNED_YOUNG_COPY`, the pinned copy pins those pages. This is
the safe direction. Before this change, option B cleared term 4 over such a
word and ran a full Cheney copy that relocated the object under the register.
Expect `cjdiv_ledger_empty` and `cjdiv_ledger_cleared` to fall slightly on
workloads with blocked threads (I/O, `wait`, `park`).

**The correctness gate in step 4 is restated.** The `[peer-reg-stale]` line
now splits its words by source and fate. Read `stale_live=` instead of
`stale=`: `stale=` still counts helper-window stack bases, and the
blocked-wake fold rewrites those at wake. On an option-B cycle
(`cycle=cheney`), nothing is pinned. There, `stale_live=` above 0 names a
register or an interior word the Cheney copy relocated under a blocked peer.
The flag-off default moving path has the same exposure. See
`docs/internal/gaps/gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md`.

New unit tests, in `gc_quiescence::young_pin_ledger_tests`:
- `peer_register_words_join_the_pause_and_go_stale_with_it`;
- `a_register_word_overflow_is_incompleteness`;
- `option_b_keeps_the_divert_over_a_peer_register_word`;
- `only_register_indices_are_registers`.
