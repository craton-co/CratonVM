# `NativeGrowthReclaimProbe`: the OSR-compiled `main` keeps a dropped `ArrayList` past its row (holder narrowed by reading, not yet named)

> **STATUS (2026-09-29, gce ve2): OPEN -- frame half: Generational 3/3, ZGC 2/2 in HotSpot's NewRatio=2 geometry; G1 0/2 is gcd-d10v. The fragmentation half is gce-e2f-old-humongous-compaction-refused-while-compiled-frames-are-live.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/f): NARROWED -- the Generational residual is
> NOT a frame holder; it is an old-generation compaction the JIT's live
> frames refuse. The empty control output is explained (an OOME escaping
> line 95, the pre-e1 failure). No code change for it this wave.**
>
> - **Named, on a FAILING census run** (base `adb9178bc`, Windows,
>   `CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_GC_STATS=1
>   RUST_LOG=cratonvm::gc=info`, `NativeGrowthReclaimProbe alAdd`, which
>   printed `alAdd=1/4`): at every missed round the only large live old
>   object is the round's OWN list array (26 MiB; roots: the IR
>   `ArrayList.add` frame's `this`, `in_map=true`, and the grow native's
>   `native-pin`) -- legitimately live. The grow's 39 MiB request fails with
>   `old_free_bytes=40322688 old_largest_free_block=27709728` (or
>   `36307096`): FRAGMENTATION, not a dead holder. The allocation ladder
>   asks for the humongous compaction that would fix it, and the
>   non-moving pause declines it: `INFO cratonvm::gc: old-gen humongous
>   compaction not taken by default; reclaiming in place why="conservative
>   roots are live (CRATONVM_GC_OLD_PINNED_COMPACT is off)"
>   published_words=4..6 pinned_root_range=3890..4006
>   humongous_request=39258696`, every time (`gen_heap.rs`, the
>   `humongous_refusal` arm of the non-moving sweep). `pinned_root_range`
>   ~ the whole root slice: `OldPinnedCompact::plan_from_sources` pins the
>   whole slice when the interpreter probe ran (compiled frames live) and the
>   door published no `OldPinRootLayout` (`OLD_PIN_LAYOUT_FALLBACKS`).
>   With `--nojit` the same row is `4/4` x3 (no conservative source, so the
>   default humongous compaction runs).
> - **A/B, base, Windows, `alAdd` (4 rounds per run):** default 1/4 (runs
>   also pass 4/4: the list array lands young or old by timing);
>   `CRATONVM_GC_OLD_PINNED_COMPACT=1` 3/4, 3/4, 2/4, 3/4, 2/4, 3/4 (the
>   compaction runs but PINS the list array, named by a root in the pinned
>   range, so the hole stays split); `-XX:NewRatio=2` (HotSpot Serial's old
>   share, 85 MiB) default 0-1/4; `-XX:NewRatio=2
>   CRATONVM_GC_OLD_PINNED_COMPACT=1` 4/4 x3. The geometry matters too:
>   Generational's default old gen is 64 MiB of `-Xmx128m` (young is a
>   32 MiB semi-space PAIR), and 26 MiB live + a 39 MiB request = 65 MiB, so
>   in the default geometry the row can pass only while the list array is
>   young.
> - **What would fix it (not this lane's files, not small):**
>   `gengc-r5w6-old10-proposal-pinned-compaction-pins-only-unrewritable-words-20260927.md`
>   (pin only unrewritable words, rewrite map-named compiled-frame words
>   after an old move) plus a door that publishes `OldPinRootLayout` on the
>   allocation-ladder path, so the pinned range is the peers' part only.
>   New page: `gce-e2f-old-humongous-compaction-refused-while-compiled-frames-are-live-20260929.md`.
> - **The empty control output (`*_ngr_ctl`):** the stderr of such a run
>   ends `OutOfMemoryError: Java heap space at
>   NativeGrowthReclaimProbe.main(NativeGrowthReclaimProbe.java:95)`: the
>   `sbCapacity` rounds' `fillAndDrop` is OUTSIDE its `try`, so the retention
>   the two switches restore surfaces as an OOME escaping `main` before the
>   println. That is the probe's pre-e1 failure form (d8/x recorded the same
>   line-95 OOME on `307f0c6a2`; the base here prints it in some runs and the
>   `PROBE-FAIL` line in others). Not a crash. Decisive reading of an
>   empty-stdout row: its stderr carries that `:95` trace.
> - **Retire this page (the frame holders):** ZGC 5/5 = HS is the frame
>   claims' evidence; the Generational residual moves to the new page. Row
>   for the frames question on Generational, which the compaction refusal
>   does not confound: `p gen_ngr_nr2_$r 300
>   "CRATONVM_GC_OLD_PINNED_COMPACT=1" "-XX:+UseGenerationalGC -Xmx128m
>   -XX:NewRatio=2" NativeGrowthReclaimProbe` x3 = HotSpot (`java
>   -XX:+UseSerialGC -Xmx128m -XX:NewRatio=2` prints the 4/4 line), and the
>   same with `CRATONVM_GC_SP_LOCAL_MAP_ROOTS=0
>   CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0` as the control (expected to
>   fail `toCharArray` / `sbCapacity`, stdout line or `:95` on stderr).
>   Measured on the base in that geometry: without the claims (base) 0/2
>   (`toCharArray=3/4`, and one `:95` OOME); with the two regions the claims
>   drop skipped by the measurement lever
>   (`CRATONVM_JIT_BAND_SKIP=java-local,deopt-saved-gpr-image`, the base's
>   stand-in for e1's claims) `PROBE-OK`, HotSpot's line.

> **STATUS (2026-09-29, gce e1/x): KEEP -- ZGC now passes; Generational improved but still fails; the kill-switch control prints nothing.**
> - Evidence (`verify-e1/ve1`, Linux release, `-Xmx128m`): ZGC `zgc_ngr_1..2` and `zgc_ngr_census_1..3` (`alAdd,sbCapacity`) = HotSpot 5/5 (base `zgc_ngr` 0/2). Generational `gen_ngr_1..2` 1/2 (`gen_ngr_1`: `toCharArray=3/4`), `gen_ngr_census_1..3` 2/3, and the census row with `CRATONVM_DBG=oldmark-root-census,root-source` 0/1; the two `alAdd,sbCapacity` failures print `alAdd=1/4`. Base: `gen_ngr` 0/2, `gen_ngr_census` 0/3. OSR off (`*_ngr_noosr_*`) = HotSpot on both binaries.
> - The control arm (`CRATONVM_GC_SP_LOCAL_MAP_ROOTS=0 CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0`, `gen_ngr_ctl` / `zgc_ngr_ctl`) exits rc 1 with an EMPTY stdout on e1, where the base's same row printed the probe line. So the two `=0` switches do not restore d10's behaviour; read that row's stderr. G1 prints nothing (`gcd-d10v-...`).
> - **Remaining:** name the Generational `alAdd` holder (census on a FAILING run; the same row fails on Windows, `gce-e1f-windows-native-growth-alAdd-fails-with-the-jit-SUPERSEDED-20260929.md`), explain the empty control output, then 3/3 on Generational.

> **STATUS (2026-09-29, gce e1/f): FIXED IN CODE, awaiting the probe -- the
> holders are NAMED (a census on the base binary), and they are two band
> words the conservative frame scan read although nothing will read them
> again. Both are dropped by two new band claims, default on.**
>
> - **Measured (this lane, Windows, `cratonvm-jitr14-base.exe` = a release
>   build of the base `adb9178bc`, run from a copy; `-Xmx128m -cp
>   tools/probes`, `CRATONVM_DBG=oldmark-root-census,root-source`).** The
>   no-argument run prints `alCapacity=4/4 alAdd=0/4 toCharArray=2/4
>   sbCapacity=4/4` (1/4 and 2/4 for `toCharArray` across reps); `--nojit`
>   prints HotSpot's line. At the `toCharArray` row's failing majors the
>   top root is `jit-precise-movable -> old [30 MiB char[]]
>   prov="method=NativeGrowthReclaimProbe.main off=96 region=java-local
>   tier=sp sp=258 live_hi=160 in_map=false"`: **local 11** of the OSR'd
>   `main` (home `8*(11+1)`), the PREVIOUS round's `cs`, at the next round's
>   `fillAndDrop` (bci 258). At 258 local 11 merges an `int` (the `alAdd`
>   loop's `i`) with the `char[]` of the back edge, so the verifier types it
>   `top`; the map does not name it (`in_map=false`); only the conservative
>   band read kept it. With `CRATONVM_JIT_BAND_SKIP=java-local` the same
>   array is next read from `method=...fillAndDrop off=672/704
>   region=deopt-saved-gpr-image`: the frame-deopt register block of a frame
>   that never deoptimized, i.e. whatever an earlier frame at that stack
>   depth (the `toCharArray` call's) left there. With
>   `CRATONVM_JIT_BAND_SKIP=java-local,deopt-saved-gpr-image` the row prints
>   `toCharArray=4/4`. The `sbCapacity` rounds on Linux (`3/4`) are the same
>   local 11 one row later (it merges `int`, `char[]` and `StringBuilder`).
> - **Corrections to the page below.** Candidate 4 (the IC args buffer) is
>   not the holder; candidate 3 was the right family but the wrong slot. The
>   body IS single-pass, but not because of the `ldiv` preference:
>   `osr_single_pass_preferred` answers `false` for `main` (every loop with
>   an `ldiv` also calls), the optimizing route is attempted, and
>   `IrBuilder::build refused at ir.rs:14544 (bytecode pc 0) ...
>   Runtime.getRuntime` leaves it no IR body (`osr optimizing (bg) ...
>   entries=[] accept=accepted`), so the door falls back to the single-pass
>   OSR artifact.
> - **Fix (`vm/src/jit/conservative_roots.rs`, `scan_one_frame_filtered`):**
>   the seventh band claim, `SpLocalClaim` (`sp_local_claim`): in a
>   single-pass frame, a java-local home no map of the active safepoint names
>   -- neither through the must-be-an-oop mask nor as an operand-stack alias
>   -- is not a root, when every map of that id carries the locals oracle
>   (`OopMapEntry::local_oop_mask`, i.e. the dataflow reached the pc and the
>   method has at most 64 locals). The band verifier already spends the same
>   statement for relocation (`band_slot_is_verifiable_with_map`). Switch
>   `CRATONVM_GC_SP_LOCAL_MAP_ROOTS=0`. The eighth,
>   `frame_deopt_image_is_residue`: a single-pass frame's frame-deopt GPR
>   block whose RBP image (`gpr[5]`) is not the frame's own `rbp` was never
>   written by this activation's stub (the only writer, which spills RBP) and
>   is not a root. Switch `CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0`. Both
>   drop only object starts, count into `LIVESET_CENSUS`, and go through the
>   `CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1` oracle (`why = "sp-local-map"` /
>   `"deopt-image-residue"`, and a `[sp-local-map] words_excluded=..
>   deopt_image_residue=..` line). Shared code: every collector's frame scan
>   goes through it.
> - **Residual (by reading, recorded not fixed):** a deopt snapshot at a pc
>   bytecode liveness does NOT cover can still describe a whole-method-`Ref`
>   local from its home (`x64.rs`, `typed_local_frame_value`'s
>   `LocalKind::Ref` arm), and would then resume a dead interpreter local
>   with the stale word. The same hazard already exists for a moved young
>   object (the band verifier's exemption); it is the reason for the switch.
> - **Tests:** `cargo test -j 5 -p cratonvm-vm --lib gce_e1f` (in
>   `conservative_roots.rs`: `gce_e1f_sp_local_claim_drops_only_unnamed_described_homes`,
>   `..._needs_every_map_of_the_id_to_describe_its_locals`,
>   `..._is_single_pass_only_and_needs_a_safepoint`,
>   `gce_e1f_sp_java_local_index_inverts_the_home_offset`,
>   `gce_e1f_deopt_image_residue_reads_the_rbp_image`,
>   `gce_e1f_deopt_image_residue_reads_the_live_frame`).
> - **Verify (Linux, each of `-XX:+UseGenerationalGC`, `-XX:+UseZGC`; G1 is
>   gated by `gcd-d10v-g1-heap-filling-probes-exit-before-any-output-...`):**
>   `P="--java-home $JDK <gc> -Xmx128m -cp tools/probes"`;
>   `cratonvm $P NativeGrowthReclaimProbe` x3 and
>   `cratonvm $P NativeGrowthReclaimProbe alAdd,sbCapacity` x3 must print
>   HotSpot's lines (`alCapacity=4/4 alAdd=4/4 toCharArray=4/4
>   sbCapacity=4/4 PROBE-OK (sink 0)`, and the `skipped` form), rc 0. The
>   control `CRATONVM_GC_SP_LOCAL_MAP_ROOTS=0
>   CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0` should fail as d10 did. A
>   census run must name no `main ... region=java-local` or
>   `region=deopt-saved-gpr-image` holder. Windows is NOT a valid host for
>   this probe on this base: its `alAdd` row fails with the JIT on even with
>   `CRATONVM_JIT_OSR=0` and with both claims levered off
>   (`gce-e1f-windows-native-growth-alAdd-fails-with-the-jit-SUPERSEDED-20260929.md`).

> **STATUS (2026-09-28, gcd d10/f third commit, lane frames10): OPEN,
> narrowed by reading; no code change.** Filed from the orchestrator's
> item 4. The measured facts are lane o's (`d10-o-report.md` section 2) and
> the orchestrator's. Without a run this lane could not name the holder, so
> no fix is made blind. The four candidates are below, three of them ruled
> out by reading, with the command that names the fourth or refutes it.
> Owner: the JIT frame lane (`jit/src/x64/**`), once the census has run.
> Collectors: Generational and ZGC fail. G1 fails earlier, for a different
> reason (the humongous refusal page).

*Filed 2026-09-28 by gcd wave d10, lane f.*

## Measured (lane o, pre-round Windows binary, `-Xmx128m`; orchestrator on d9)

- Rows `alAdd,sbCapacity`: OOME at `:95` (sbCapacity's
  `fillAndDrop(max*70/100)`, outside its `try`) / `:131` (the fill's
  `new byte[pieceBytes]`), on Generational and ZGC.
- The same run with `CRATONVM_JIT_OSR=0`: `alAdd=3/4 sbCapacity=4/4` on
  Generational and `4/4 4/4 PROBE-OK` on ZGC.
- Rows `alCapacity,sbCapacity`: pass. `sbCapacity` alone: 4/4.
- d9 build: still fails 3/3, with 0 true-root fallbacks, so the Generational
  seed is not the holder.

So something the OSR body of `main` keeps holds one of `alAdd`'s lists: about
26 % of the heap, plus a grown copy. That list is `new ArrayList<>(k)` with
`k = refs(26 %)`, whose `Object[k]` is allocated eagerly. It is held through
the sbCapacity row.

## The method (`javac` 25, `-g`; the bcis that matter)

- **Slot 10.** It holds `alAdd`'s `l` from bci 136 (`astore 10`) to its last
  read at 188 (`l.size()`). The loop that OSR enters is at bcis 141-159:
  `l.add(filler)`, an `invokevirtual` through the inline cache. Slot 10 is
  next written at bci 246 (`astore 10`, toCharArray's `s`, a row skipped
  here) and at bci 295 (`istore 10`, sbCapacity's loop counter, int 0).
- **bci 318.** This is sbCapacity's `fillAndDrop` call, where the failing
  collections run. The operand stack there is `sink` (`lload_3`) plus the
  call's argument.
- **Tier.** `main` contains `ldiv`, so the optimizing OSR route declines it:
  `osr_single_pass_preferred` (`jit_bridge.rs`, "single-pass preferred
  (ldiv/lrem/lcmp)"). The OSR body is a SINGLE-PASS body
  (`compile_osr_artifact` -> `x64::compile_with_param_slots`).
  `CRATONVM_DBG_JITC=1` prints `osr optimizing ... not attempted --
  single-pass preferred` to confirm it.

## Candidates

1. **Ruled out: the interpreter frame's entry locals.** They were parked
   before the body ran (`park_osr_entry_reference_locals`,
   `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS`, default on since gen r5w5, written
   for exactly this probe).
2. **Ruled out: the OSR seed vector `jit_locals`** (`try_osr`), which still
   holds round 1's `l` as a raw word. It is a Rust heap `Vec`, which no
   collector scans, and `osr_trampoline` copies from it into the body's
   homes; the native stack holds only its pointer.
3. **Ruled out: slot 10's single-pass home.** A single-pass body gives a
   local index ONE home: a register colour (`osr_local_assignments`) and
   its canonical frame word, which the pre-safepoint spill refreshes. The
   `istore 10` at bci 295 writes int 0 there before the first failing
   collection at bci 318, and the local oop masks call slot 10 an int there.
   The only way this is wrong is a register-homed slot 10 whose canonical
   word the spill does not refresh; the census below would show it as
   `region=local`.
4. **Leading candidate: a dead operand-spill word of the OSR body.** The
   inline-cache sites `l.add(filler)` (bci 148 and 177) and `l.size()`
   (bci 186) stage their arguments into the args buffer at the pre-pop
   spill cursor. That is depth 0 for 148/177, with `l` as argument 0, and
   depth 1 for 186. The word stays in the frame after the call. At bci 318
   the stack entries at those depths are `lload_3` (local 3, `sink`) and the
   call's argument. The load of a register-mapped local is the zero-cost
   `StackSlot::CalleeSaved` push, which writes no frame word, so the word at
   depth 0 can still hold round 4's `l` when `fillAndDrop` collects.
   - **ZGC:** this backend scans the single-pass frame conservatively, which
     reads that word.
   - **Generational:** the dead-spill claim (`CRATONVM_GC_DEAD_SPILL_ROOTS`,
     default on) drops words above the recorded cursor. It helps only if
     this safepoint recorded one: `emit_safepoint_metadata_only` records
     `live_frame_hi` only on its moving-young arm, and a word BELOW the
     cursor is still read.

   The `OsrDeadSlot` probe never exercised this: its dead value was a call
   result in an IR frame (fixed by d6/u).

## How to name it (orchestrator, on the d10 build)

Generational, where the census names roots:
```
P="--java-home $JDK -XX:+UseGenerationalGC -Xmx128m -cp tools/probes"
CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_DBG_JITC=1 timeout 600 \
  cratonvm $P NativeGrowthReclaimProbe alAdd,sbCapacity 2>ngr.log
grep -n 'holder#' ngr.log | grep -v 'Static fields' | head
grep -n 'NativeGrowthReclaimProbe.main' ngr.log | grep -E 'osr|single-pass preferred' | head
```

Reading the output:
- A `holder#` line naming `method=NativeGrowthReclaimProbe.main ...
  region=operand-spill tier=sp off=<o>` confirms candidate 4.
- `region=local` points to candidate 3.
- A `native-pin` or `Rust stack` provenance means neither.

Also run the two controls on Generational and ZGC:
- `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS=0`: the same failure is expected.
- `CRATONVM_JIT_OSR=0`: expected to pass, which re-establishes lane o's
  row.

If the census run passes (it can change timing), the plain run plus
`CRATONVM_DBG_JITC=1` still confirms the tier, and the census arm of
`GenR4W6JitOomRootProbe` is the fallback.

## Proposed fix, if candidate 4 is confirmed (default on: retention)

On the NORMAL return edge of a single-pass inline-cache or dispatch site,
after the post-call callee-deopt check and the exception check, zero the
args-buffer words that held a reference argument. On that edge nothing
reads the buffer again: the result is pushed at `post_pop_spill`, and the
only post-call readers (the service check, the `Reinterpret` resume
snapshot) run before this point or on the exceptional edge. The site would
record the reference words; the IC hit, IC miss and dispatch arms of
`walk_invoke_instance` all stage through `args_base_offset`. A narrower
alternative is to write the frame word for a `CalleeSaved` push at a
safepoint. That word does not move the spill cursor, the constraint the
`StackSlot::Scratch` note records for OSR homes. Either needs the census
first: without a named holder, it is a guess about which word to clear.

## How to verify a fix

Run on each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`, `-XX:+UseZGC`,
`-Xmx128m`:
- `NativeGrowthReclaimProbe alAdd,sbCapacity` should print
  `alCapacity=skipped alAdd=4/4 toCharArray=skipped sbCapacity=4/4
  PROBE-OK`, 3/3 on Generational and ZGC. HotSpot prints the same line.
- The no-argument run should print HotSpot's `alCapacity=4/4 alAdd=4/4
  toCharArray=4/4 sbCapacity=4/4 PROBE-OK`.
- G1 is gated by the humongous refusal page first.
