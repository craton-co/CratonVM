# A large IR body pays ~110 instructions per activation before its loop, and ~40 more to publish its frame block

Status: OPEN. Area: JIT optimizing tier (prologue, frame block publication) and GC shadow-stack readers.
Severity: perf, every Matcher/split row (each calls `match` bodies per position or per op). Found by round
rx27 wave 4, lane bimodal, 2026-10-28.

## Evidence

`regex-matches` on `cvm-rx27b5` (16.2k instructions per op). `tools`: the lane's `prec.sh` (one process,
quiesced, `perf record -e instructions:u -t <main tid>` over 2 M ops: 37 k samples; the host caps the sample
rate at 5000/s, so short windows give ~1 k samples) and `CRATONVM_DBG_JIT_DISASM=Pattern`.

* 47 % of the row is a CALLED `Pattern$BmpCharPropertyGreedy.match` C2 body (19 KB, frame 0x3D0). It is
  called twice per subject (`[a-z]+`, then `\d*` through its pc 70), 16 times per op.
* Inside that body, by code region (per cent of the whole row):

  | region | what runs | % of row |
  |---|---|---|
  | `0x000-0x12a` prologue | 13 `vmovdqu` frame clears + `vzeroupper`, 5 callee-saved stores, 5 argument homes, the thread-state stores (`fs:` frame / bci / thread fetch), the entry poll test | 6.5 |
  | `0x12a-0x371` entry to the loop | parameter copies through the merge with the mode-exit resume entry (each reference parameter goes `reg -> [rbp-8] -> [rbp-170h] -> its colour`), the String class test materialised to 0/1 and re-tested, 8 `mov qword [..], 0` param-home zeroes (`CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO`) | 4.3 |
  | `0x4aa4-0x4bf0` frame-block publication | 18 indirect shadow entries: `lea rcx,[rbp-X+1]; mov [r11+8i],rcx` each, plus the overflow guard | 5.9 |
  | each call-out | ~7 dead-home clears (`[rbp-3A0h]` stored twice at the `0x713` site), the bci store, the publication `CMP/JNE/CALL` | (spread) |

  About 17 % of the row is per-activation bookkeeping of this one body. `Pattern.matcher` (24 %) and the
  called `LastNode.match` (4.5 %) pay the same shapes.
* The existing opt-ins do not reach it (icount2 3 reps, `ic2-lazy.log`): `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR=1`
  regex-matches 16 467 / base 16 454, regex-find-email 39 475 / 38 110, cached 41 116 / 40 569;
  `CRATONVM_JIT_IR_LAZY_REF_HOMES=1` 16 551 / 38 110 / 41 296; both 16 542 / 38 108 / 41 160.
* (Correction to the wave-3 note on `rx27-bimodal-matchonly-and-groups-spread-20261028.md`: the frame is 976
  bytes, but the prologue clears 39 words with 13 stores, not 976 bytes; the 30 % there was a 1 k-sample
  estimate of the whole entry region.)

## Why it cannot be cut in place

The frame block is how the collectors find this frame's references: there is no stack walk with oop maps, so
every reference home is published by address once per activation, and every published word must hold a
reference or zero from the first safepoint to the return. That is what the prologue clear, the param-home
zeroes and the per-call dead-home clears guarantee (a stale word would be read, and under a moving collector
rewritten). Dropping any of them is unsound unless the reader stops reading the word.

## What would close it

1. **One descriptor entry per activation instead of one entry per home** (the publication, 5.9 %): push
   `(rbp | 2, &static_offsets_table)` -- two stores -- and let the readers expand it. Every reader of the
   window must learn the record, or a root is lost: `gc/src/shadow_stack.rs` (`for_each_value_in`,
   `indirect_slots_in`, `remap_in`, `resolve_entry`), `vm/src/jit/xt_root_scan.rs` (the raw peer-window reads
   at ~1173, ~8223, ~8289), `vm/src/jit/conservative_roots.rs` (~7466 and the moving-young coverage verifier,
   which counts `n` entries per push, ~23041). Batteries: R24 facts both modes, `Zr8StPeerHandoff`,
   `RandomLiveAcrossGc`, `ShadowOddLongProbe`, every collector.
2. **Oop maps per safepoint for the IR tier** (prologue clears and dead-home clears, ~8 %): the HotSpot shape.
   A word only needs a zero if a map can name it before it is written; with per-safepoint maps the GC reads
   only live words and the clears go. `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` is the start of this, but it
   still composes with the block (`frame_block_composes_with_liveness`), so today it adds work.
3. **The mode-exit resume entry's merge** (entry region, ~4 %): the second entry at the loop header makes every
   parameter a phi with a frame home; a separate resume stub that writes the colours directly would leave the
   normal entry with register parameters.
4. Small: dedupe the dead-home clear list against the staging clears at a call site (`[rbp-3A0h]` twice).

## Round rx27 wave 5 (lane bimodal, 2026-10-28; built as `cvm-bimodal-6` / `-7`)

**The 13-store block in `Pattern.split`** (the profiler's `+9dd..+a61`; `+9be..+a4d` on `cvm-bimodal-7`) is a
set of loop-ENTRY dead-home clears (`CRATONVM_JIT_IR_LOOP_ENTRY_DEAD_HOME_CLEARS`, default ON):
`CRATONVM_DBG_IR_SLOTS=1` prints `loop-entry dead-home clears: {.. 17: [576, 592, 608, 632, 856 .. 920] ..}`.
It sits between the two loops of the spliced `Matcher.search` (`groups[i] = -1`, then the `localsPos`
`IntHashSet.clear()` loop) and runs once per `find`. The rule took the clears off the `IntHashSet.clear` call
site inside the second loop because nothing in that loop writes those colours. For `\s+` that loop never
iterates (`localsPos` is empty), so the move turned 0 stores per find into 13. Measured share on rx27b5
`cached` (26 k samples): 3.2 % of the row (~1.3 k instr/op), less than the 4.5-5 k the per-row page
attributes to all of `split`'s frame clears.

* `CRATONVM_JIT_IR_LOOP_ENTRY_CLEARS_BY_FREQUENCY=1` (opt-in, `move_loop_entry_dead_home_clears`): move only
  when the site's block frequency is at least the entry block's. **Does not engage here**: the block
  frequencies of a spliced loop are the static estimate (a loop is assumed to iterate), so the site reads as
  hotter than the entry and the 13 stores stay (`cvm-bimodal-7`, same 912 zero stores in `split` on and off).
  Closing it needs either the callee's real trip count in `BlockFrequencies` for spliced loops (its
  `MethodProfile::loops` exit/back-edge counts), or a placement that runs once per entry only when the body
  is entered (the clears on the header->body edge of a rotated loop).
* `CRATONVM_JIT_IR_LOOP_ENTRY_DEAD_HOME_CLEARS=0` puts them back on the in-loop site (free here) but also
  back into every other loop that needed the move; icount2 3 reps: cached 40 826 vs 38 721, split-ws 45 696
  vs 45 428 (no gain).

**Call-site clear dedup** (`CRATONVM_JIT_IR_CALL_CLEAR_DEDUP=1`, opt-in, `Lowerer::emit_call_clear_batch`):
a call site's staging clear and planned dead-home clears as one batch, each word stored once. Engages (the
`[rbp-3A0h]` repeat, and `[rbp-508h]`/`[rbp-518h]`/`[rbp-520h]` repeats in `split`), but the effect is in
the noise (icount2 5 reps vs `cvm-bimodal-6` without it: regex-matches 16 425 / 16 524, find-email 38 122 /
38 119, cached 40 983 / 41 179, matchonly spread both).

**Proposal 3 (separate resume stub)**: not built. The second entry into `BmpCharPropertyGreedy.match` is the
ordinary OSR entry stub (`emit_osr_entry_stubs`), which jumps to the loop header with the header phis'
homes seeded. `CRATONVM_JIT_IR_OSR_ENTRY=0` A/B (3 reps): regex-matches 15 911 / 16 475, find-email 37 158 /
38 113, cached 40 359 / 40 314. The 3 % does not come from the stubs alone: with the stubs off the compile
also lands with a different profile (the body's own compile sees 59 calls at pc 21 instead of 513, and
splices pc 26 one-class instead of bimorphic). The "materialised String test" is
the fused class test (`CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH`, wave-1 opt-in), present with or without
OSR stubs.

## Round rx27 wave 6 (lane bimodal, 2026-10-28; built as `cvm-bimodal-8`)

**(1) Header-to-body placement of the split clear block: not built.** The block's loop (`Matcher.search`'s
`for (i = 0; i < localsPos.length; i++)`) is top-tested: its header holds the poll, the `localsPos` field load
and the `arraylength` the exit test compares against, and the header-to-body edge runs on every iteration.
There is no edge that runs once per entry and only when the body runs; it exists only in a rotated loop
(guard at the entry, test at the bottom). The options, none small: rotate loops in the IR; re-evaluate the
header's test at the entry block (needs the header's load and `arraylength` re-emitted there, sound only
because a zero store is never wrong -- a skipped one is a retention, not a dangling root, since every
published home holds a valid reference); or a per-entry "cleared" flag tested in the body (2 instructions
per iteration, worse than today for the 16-trip loops the move was made for). Note for the GC invariant:
the clears are a reachability/retention measure, not memory safety -- the frame block keeps every home a
valid root at every GC.

**(2) One descriptor record per activation: built, opt-in** (`CRATONVM_JIT_IR_FRAME_BLOCK_DESCRIPTOR=1`).

* Record (module docs of `gc/src/shadow_stack.rs`, "Frame descriptor records"): `w0 = rbp | 2`,
  `w1 = 0xF0B10C4D << 32 | n`, then `ceil(n/4)` words of four u16 home offsets / 8. No pointer outside the
  shadow stack (no table to keep alive or validate); recognised only when `w0` has tag 2 (never an object)
  AND `w1` has the magic (never a user-space address); homes trusted only inside the owner's band, as
  indirect entries are.
* Readers: ONE walker, `shadow_stack::walk_window`, now used by every window reader: `for_each_value_in`,
  `indirect_slots_in`, `remap_in` (every frame home rewritten once), `xt_root_scan::read_peer_shadow_window_ex`,
  `conservative_roots::published_shadow_values`. Every other consumer (vm_exec, gc_and_alloc, memory/gc,
  memory/roots, helpers/alloc, `shadow_owned_slots`) goes through those methods; no other raw window read
  exists (grep of `shadow_stack.base/top`, `resolve_entry`, `INDIRECT_TAG`).
* Producer: `Lowerer::emit_frame_block_publication` stores the record (`LEA RCX,[rbp+2]`, then `MOV RCX,
  imm64; MOV [R11+8k], RCX` per word) where it stored the n entries; the overflow guard's `need` is the
  record size; exits retract `top` to the saved base as before. 18 homes: 6 stores instead of 18
  `LEA`+`MOV` pairs. Engagement: `CRATONVM_DBG_JIT_DISASM=BmpCharPropertyGreedy` shows
  `mov rcx,0F0B10C4D00000012h`.
* Tests, one per reader: `rx27_walk_window_expands_a_record_between_values`,
  `rx27_for_each_value_reads_every_descriptor_home`, `rx27_indirect_slots_name_every_descriptor_home`,
  `rx27_remap_rewrites_descriptor_homes_once`, `rx27_a_malformed_record_is_not_expanded` (gc);
  `rx27_peer_descriptor_record_homes_are_roots` (xt_root_scan); `rx27_published_shadow_values_expand_a_descriptor_record`
  (conservative_roots); `rx27_frame_block_descriptor_packs_offsets_for_the_gc_reader` (the producer's words
  decode with the GC's constants). All pass on the host, with the existing shadow_stack / peer / bandless suites.

**Measured** (`cvm-bimodal-8`, icount2 5 reps, `ic2-desc.log`; base / switch off / switch on):
regex-matches 16 551 / 16 524 (16 445-16 593) / **16 284 (16 228-16 341)**, ranges disjoint, -1.5 %;
find-email 38 122 / 38 326 / 37 904, cached 40 732 / 41 053 / 40 874, matchonly 28.6k / 29.0k / 29.4k
(spread in every arm), split-ws 45 341 / 45 033 / 45 052 -- in the noise. Below the estimate (24
instructions saved per activation): the publication runs once per activation, and only the activations that
reach a safepoint publish.

**GC battery** (`descbat.sh`: 11 probes x ZGC / G1 / Generational / Serial x `--jdk-only` / `--compatible`,
switch off and on, one binary; HotSpot with the same collector flag): output identical on and off in every
completed run. The only differences: `ConcurrencyUnderGcSweep` (its per-phase millisecond lines) and
`G1ListArrayIntegrityProbe` under G1 / Generational `--jdk-only` (240 s timeout in BOTH arms, host load
60). `R24RegexSplitRows facts`, `Zr8StPeerHandoff`, `RandomLiveAcrossGc`, `ShadowOddLongProbe` equal
HotSpot under ZGC, G1 and Serial (HotSpot has no `-XX:+UseGenerationalGC`).

**Zero-clear census of `BmpCharPropertyGreedy.match`** (cvm-rx27b5 body, 37 k samples): 151
`mov qword [rbp-x],0` in the body, 35 of them executed in the measured window: 9 in the entry region (the
parameter-home zeroes after the copies through the OSR-stub merge), 12 + 7 + 7 at the per-call-out sites
(dead-home and staging clears, repeats included). The other 116 are never sampled: OSR entry stubs (each
zeroes the ~23 homes it does not seed), deopt pads and cold arms. Executed zero stores are ~5 % of the
row; the 13 vector clears of the prologue are separate (6.5 % region).

## Round rx27 wave 7 (lane bimodal, 2026-10-29; built as `cvm-bimodal-9`): the split clear block

`CRATONVM_JIT_IR_LOOP_ENTRY_CLEARS_ON_BODY_EDGE=1` (opt-in; `body_edge_clear_target`, the emission at the
top of `Lowerer::lower_block`): a counted loop's loop-entry clear set runs at the start of the loop BODY on
its first iteration only -- `MOV R11, counter; TEST R11D, R11D; JNE past; MOV qword [rbp-x],0 ...` --
instead of at the end of the entry block. A zero-trip entry (the `localsPos` loop for `\s+`) runs none of
the 13 stores; a loop that iterates pays them once per entry plus a test per iteration.

Guarded by "the counter is 0" in the body rather than by a re-emitted `0 < localsPos.length` at the entry:
same once-per-entry, only-if-the-body-runs placement, but nothing is duplicated, so there is no second
field load, no `arraylength`, no null check and nothing that can throw or see a different memory state. The
admission: the entry block's only successor is the header; the header ends in a two-way `If` whose one
in-loop successor (the body block) has the header as its only predecessor; the header has no armed GC site
of its own; and a header phi is a counter (entry input `Const(0)`, other input the phi plus a positive
constant). The body block dominates every in-loop site the clears came off, so every armed GC point that
relied on the moved zeroes runs after them; a wrap of the counter to 0 only clears again.

Engagement: `CRATONVM_DBG_JITC=1 ... | grep loop-entry-clears-on-body-edge` -- `Pattern.split: 13 clear(s)
from block 17 to the first iteration of block 19`. One process each: `cached` 36 989 -> 34 942 instr/op.

icount2 5 reps (`ic2-be.log`; rx27b5 / bimodal-9 off / on): `cached` 41 121 / 39 904 (38 695-40 262) /
39 741 (35 556-39 947), `split-ws` 45 438 / 44 366 / 44 082 -- not separated. The block is 13 stores per
`find`, about 20 finds per op: ~260 instr/op, under the row's run-to-run spread (the single-process 2 k
drop above was a mode flip, not the switch; `cached` is bimodal again on bimodal-9, 35.5-37.1k vs ~39.8k,
a different decision not diagnosed here). The earlier sample-based 1.3 k for this block (wave 5) overstated
it. R24 facts, `Zr8StPeerHandoff`, `RandomLiveAcrossGc` equal HotSpot in both modes with the switch on.
Stays opt-in.

**FRAME_BLOCK_DESCRIPTOR on cvm-rx27b11 (lane prof, int13, 5 reps):** `compile` 4 444 (4 366-4 474) ->
4 147 (4 059-4 230), -6.7 %, ranges disjoint; every other row within noise (split-ws 42 141 -> 43 895,
overlapping). Facts and the GC battery identical on the default GC, G1 and Serial. A flip candidate for the
12-row b15 baseline.

## Close-out (lane bimodal, 2026-10-29)

**What landed.**

* Keep-loop rules: `CRATONVM_JIT_IR_KEEP_LOOP_STRING_PREFIX` and `CRATONVM_JIT_IR_KEEP_LOOP_RARE_ROW`. They fixed
  the compile-order bimodality of `Start.match`'s splice of `BmpCharPropertyGreedy.match`.
* `CRATONVM_JIT_IR_FRAME_BLOCK_DESCRIPTOR`: one shadow-stack record per activation, with every window reader
  expanding it through `shadow_stack::walk_window`. It went default ON after the measurements: regex-matches
  -1.5 % here, `compile` -6.7 % on b11 by lane prof. The GC battery was identical on every collector.
* Three switches were removed because they never paid: `LOOP_ENTRY_CLEARS_ON_BODY_EDGE`,
  `LOOP_ENTRY_CLEARS_BY_FREQUENCY` and `CALL_CLEAR_DEDUP`.
* Still opt-in from this lane: `NESTED_HOT_CAP_BY_FREQUENCY` and `NESTED_EXACT_YIELDS_TO_PROFILE`.

**The dse0 finding** (`ic2-bund.log`, `ic2-12.log` on `cvm-bimodal-10`).

* The rx27 chain bundle's `CHAIN_ENTRY_SPLICE` alone regressed two rows: cached 45.0k against 37.1k with it
  off, split-ws 49.3k against 43.0k.
* `ASCII.isType` was a call per character at nest depth 7, because the leaf tier refuses a body that calls.
  `CHAIN_DEEP_SPINE=1` splices it.
* `CHAIN_DEEP_SPINE=1` with `CHAIN_ENTRY_SPLICE=0` was the best set on all 14 rows. cached reached 35.8k
  (3.04x HotSpot), split-ws 42.9k, regex-split 90.9k; the other rows stayed within 1.5 % of def. Adopted:
  deep spine ON, entry splice opt-in.
* Under it, R24 facts, `Zr8StPeerHandoff`, `RandomLiveAcrossGc` and `J27DeoptRows` matched the default binary
  in both modes.
* Open parity gap, independent of the switches: `J27DeoptRows` prints
  `exception-after-deopt-repeat-mismatches` as 12924 on HotSpot and 0 on CratonVM.

**Today's profiles** (dse0 env plus the descriptor, `cvm-bimodal-10`).

* split-ws, `/root/rx27/bimodal/pw1` (perf) and `/root/rx27/bimodal/pw2` (DBG census).
  * `Pattern.split` 44 %, a called `Pattern$Start.match` 40 %, `Pattern.compile` 4 %, `ArrayList.grow` plus
    `copyOf` 3.4 %.
  * `Start.match` stays a call per `find`. In split's tree, `Matcher.search` pc 121 gets a nest-exact row for
    `Pattern$Node`, which never proves its receiver.
  * `NESTED_EXACT_YIELDS_TO_PROFILE=1` drops that row, but the 90-byte, non-leaf `Start.match` is then held to
    the leaf rule.
  * Forcing the splice (yields + `NESTED_GUARDED_HOT_CAP_LEAF=0` + `NESTED_BIMORPHIC_SPLICE=1`) gains nothing:
    split-ws 43.0k, cached 38.0k, matchonly 27.6k. That matches entry splice, which also made split-ws slower.
  * Inside `Start.match` the cost is spread over the per-character scan loop: frame-slot reloads, branches,
    bounds and layout checks, with no clears.
  * In `split`, 113 of 669 zero stores execute, about 9 % of samples. Most of that is the 12-store loop-entry
    block between `Matcher.search`'s two loops, which runs once per find (a few hundred instructions by
    count; the sample share is skid).
* regex-matches, `/root/rx27/bimodal/pm1`.
  * A called `BmpCharPropertyGreedy.match` 49 %, the spliced row lambda (`spin`) 46 %.
  * The call exists for the same reason, at `Matcher.match` pc 121. Forcing the splice gives 14 678 against
    14 745.
  * The body's prologue region is 6.0 % of the row and its executed zero stores 3.4 %. The descriptor already
    took the publication cost.
* Tools, in `/root/rx27/bimodal`:
  * `prec.sh`: one quiesced process, a perf stat window, then a perf record window.
  * `annot.py`, `bins.py`, `regions.py`: per-instruction, per-bin and per-region sample attribution against
    the `CRATONVM_DBG_JIT_DISASM` dump.
  * `zeros.py`: executed against cold zero stores.
  * `bm_state.py`: per-method compile diff of two DBG logs.

**Conclusion.** The remaining gap on split-ws, regex-matches and the other rows above 3x is body quality.
Splice decisions and clear decisions are not where it is. What remains is frame-slot traffic in the big
spliced bodies: each intermediate is stored to and reloaded from its home under the current accumulator
lowering. The per-character guards come on top. Splicing the remaining chain-entry calls moves the same
costs into a bigger frame and does not pay. The clears and the frame-block publication that are left are a
few per cent of any row. The next lever is the regalloc lane's frame-slot work; the guards lane's
per-character checks come after.
