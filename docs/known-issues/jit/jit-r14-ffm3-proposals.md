# JIT round 14 wave 3, lane ffm: proposals (ranked)

Status: OPEN (proposal book; ideas, not work items, until the owner queues one)
Area: FFM (`native-builtins/src/panama*.rs`, `phases_late/foreign_ffm.rs`)
Found by: round 14 wave 3 lane ffm

## FFM3W-1: free a "kept" implicit session's blocks through its own `Cleaner` action

- What: FFM7-1 (round 14 wave 3) never frees the blocks of an `Arena.ofAuto()` session that had a
  Java close action registered (`panama::auto_arena_blocks_keep`), because that action runs on the
  `Common-Cleaner` thread at no fixed point relative to the sweep. HotSpot frees them right after
  the actions, in the same cleaner run. The JDK `Cleaner` gives no order between separate
  cleanables, so the free cannot simply be one more registration: register, at the first Java
  action of such a session, ONE `ResourceList`-shaped runnable that runs the program's actions
  (appended to it from then on) and then a VM native that frees the row's ids. That closes the
  last leak of the model.
- Benefit: medium (jextract-style code registers `reinterpret(.., auto, cleanup)` often).
- Cost: medium: a VM-owned `Runnable` class with a native `run()` (explicit `NativeKind::Bridge`,
  a removal record under `docs/jdk-only/`), and routing every later action of that session into
  the same list instead of separate `Cleaner.register` calls (FFM7-2 is the same restructuring).
- Risk: medium (a native `run` that frees must never run twice: key it by the row handle).
- First step: land FFM7-2 (one `Cleaner` registration per implicit session), then add the free.

## Round 14 wave 4 (lane ffm4): FFM3W-1 landed

Landed without FFM7-2, by counting instead of merging: every Java action of an implicit session is
still its own `Cleaner` registration, but wrapped in a VM-owned `CratonVM$FfmAutoSessionAction`
(`VmInternal` class in the reserved `CratonVM$` namespace, native `run()V` registered explicitly
`Bridge`, `panama::register_auto_arena_action`). The wrapper holds the action, the session row's
weak handle and the session's identity hash -- never the session. Registration counts the action
on the row (`AutoArenaBlocks::pending`, filing a block-less row when the session has no block
yet); `run()` runs the action, then counts it off, and the last one frees the row's blocks under
the one-sweeper claim once the weak handle has cleared (else the next sweep does). A sweep never
takes a row with an action pending, so its handle is never released and reused under a live
wrapper; the handle slot is zeroed before the action runs, so a second `run()` cannot count
twice. Any failure (no weak handle, wrapper allocation, `Cleaner.register`) leaves the count on
the row, i.e. the old "kept". Switch `CRATONVM_FFM_AUTO_ARENA_ACTION_FREE` (default on; `0` =
`auto_arena_blocks_keep` as before; also off when `CRATONVM_FFM_AUTO_ARENA_BLOCK_FREE=0`).
Files: `native-builtins/src/panama.rs` (FFM3W-1 section below `auto_arena_blocks_keep`, tests
`r14w4_ffm4_auto_arena_action_tests`), `native-builtins/src/phases_late/foreign_ffm.rs`
(`p67_cleaner_register`). Order: HotSpot's `ResourceList` runs actions and frees LIFO in one pass;
here all actions (in the `Cleaner`'s order) run before any block of the row is freed, which is
never earlier than HotSpot for a block an action can read. Probe:
`C:\craton\jitr14-probes\src\R14Ffm4AutoActionFree.java`.

## FFM3W-2: pace the auto-arena sweep by collections, and ask for one under native pressure

- What: the FFM7-1 sweep runs at an auto-arena allocation after max(64, survivors) new rows; a
  program that allocates few, large auto blocks (64 MiB each) sweeps late, and nothing asks for a
  collection when the auto rows hold a lot of native memory. HotSpot's `Bits.reserveMemory`
  does exactly that for direct buffers. Proposal: sweep when the GC count moved since the last
  sweep (a `NativeContext` accessor over the collector's cycle counter), and track the bytes the
  live rows hold; past a budget (`-XX:MaxDirectMemorySize`-like), request a collection once.
- Benefit: medium (bounded RSS for large-block auto arenas).
- Cost: small-medium (one accessor; byte accounting in the rows).
- Risk: low-medium (a GC request from an allocation path: must not run under the Scratch lock).
- First step: `R14FfmArenaAutoFree` with 64 MiB blocks and no `System.gc()`: record RSS on HotSpot
  and here.

## FFM3W-3: the global scope without two locks per address read

- What: `pe_zero_length_segment` now stamps the global arena's session, which costs
  `global_arena_handle` (a Scratch lock) + `resolve_global_root` (the JNI global-ref lock) +
  `pe_carrier_scope` per `get(ADDRESS)` / downcall pointer return. Keep a second per-VM handle on
  the global SESSION itself (next to the arena's, `claim_global_arena`) and resolve only that.
- Benefit: low-medium (pointer-chasing loops; `R14FfmAddressTarget vh` measures it).
- Cost: small.
- Risk: low.
- First step: time `R14FfmAddressScope`'s `seg.scope` loop with `CRATONVM_FFM_ADDRESS_GLOBAL_SCOPE`
  on and off (interleaved, medians).

## FFM3W-4: retire the dead `SymbolLookup.loaderLookup` / `libraryLookup` rows in the real-JDK modes

- What: both are concrete static interface methods in JDK 25, so their bytecode wins over
  `register_pe_symbol_lookup`'s `Bridge` rows in `--jdk-only` and `--compatible`; the rows only
  serve `--synthetic-jdk`, where they are also wrong (the `libraryLookup` receiver keeps no arena,
  `loaderLookup` searches every library). Either register them only on the synthetic path (the
  essential registrar registers them today) or make them faithful (store the arena session in a
  third slot; per-loader library lists).
- Benefit: low (clarity; fewer rows for `resolve_dispatch` to lose to).
- Cost: small (move two registrations), medium to make them faithful.
- Risk: low (confirm with `--dump-native-registry` that no real-JDK dispatch reaches them first).
- First step: a probe calling `SymbolLookup.loaderLookup().find("strlen")` under each mode with
  `CRATONVM_DBG_...` dispatch tracing, to prove which body runs.
