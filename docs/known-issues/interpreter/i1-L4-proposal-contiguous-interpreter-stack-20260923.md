# Proposal: one contiguous slot stack per thread instead of per-frame buffers

**Status: open (wave 3: stage-0 dead-code removal landed, measurement plan written; wave 4: the frame-shape census joined the stage-0 instrument; wave 5: census corrected (locals capacity, declared max_locals) and the analysis procedure written; wave 29: the stage-0 gate was waived by the user, the stage-0 census was run on the host (H/W = 4.8-6.2, 99.96% of frames from the cached install paths), and stage 1 landed for locals AND operand stacks behind `CRATONVM_JIT_NO_LOCALS_SLAB`, default on, unmeasured; stages 2-4 remain, see "Progress (wave 29) — lane L7"; wave 32: the stage-1 call-path regression was the pooled resets' `Vec` round trip, fixed and measured flat against wave 28, see "Progress (wave 32)"; wave 33: a retired slot holding pooled buffers is converted to a window once, and the call rows are now 1–6% faster than wave 28, see "Progress (wave 33)"; wave 37: stage 2, argument overlap, landed behind the per-VM switch `CRATONVM_JIT_OVERLAP_ARGS` (default off), unmeasured, with the A/B to run written down, see "Progress (wave 37)"; wave 38: stage 2b, the static and virtual doors validate and lay their arguments in place with no `ArgSlots` copy, behind the same switch, unmeasured, with the four-sided A/B written down, see "Progress (wave 38)") — filed 2026-09-23 by interpreter round i1, lane L4.**

## Why

Every interpreted activation owns four heap buffers — `locals:
Vec<CompactValue>`, `local_kinds: Vec<u8>`, and the operand stack's
`slots: Vec<CompactValue>` + `kinds: Vec<u8>` (`vm/src/runtime/frame.rs`,
`vm/src/runtime/value_stack.rs`). Keeping that affordable has grown a large
apparatus: the thread pools (`locals_pool` / `stacks_pool`), a per-OS-thread
spill pool (`FRAME_SOA_TLS`), a VM-wide `VecPool` spill, retired-slot reuse in
`FrameStack` (`push_cached_compact_reusing`, `harvest_retired_slot`), in-place
harvesting (`take_pool_parts_in_place`), and the `Vec<u64> <-> Vec<CompactValue>`
transmutes at every pool boundary. The measured return path
(`docs/internal/fixed-bugs/interpreter-the-return-path-is-the-largest-phase-20260819.md`)
is still dominated by this lifecycle, and every invoke copies its arguments
from the caller's operand stack into the callee's separate locals buffer.

Other costs of the per-frame shape:

* Operand stacks are sized `max(max_stack, 16) + 8`, so a leaf method with
  `max_stack = 2` owns 24 slots plus 24 kind bytes.
* Each `Frame` is ~230 bytes of headers, moved on the by-value paths.
* Retired frames keep their buffers and metadata `Arc`s alive until something
  trims them (`FrameStack::trim_retired`), so a deep recursion's high-water
  mark stays allocated.

## Design (HotSpot's interpreter layout, adapted)

A per-thread `SlotStack { slots: Box<[CompactValue]>, top: usize }` reserved
once (grown by doubling, with an epoch like `FrameStack::reloc_epoch`, or
reserved to `max_stack_depth × typical frame` in virtual memory and committed
lazily). A frame becomes a window:

```
[ caller locals | caller operand stack ... args ] [ callee extra locals | callee operand stack ]
                                         ^ callee locals start here (args overlap)
```

* `Frame` keeps `locals_base: u32` and `stack_base: u32` and `sp: u32`
  instead of three `Vec`s; `max_locals` / `max_stack` bound the window.
* **Arguments are not copied**: the callee's locals begin at the caller's
  first argument slot, exactly as in HotSpot. Only the extra locals
  (`max_locals - arg_slots`) are initialised.
* Push/pop of a frame is `top += size` / `top = base`; no pool, no allocation,
  no `Vec` header traffic.
* Kind marks become a parallel `Box<[u8]>` of the same shape until the
  precise oop maps land (`i1-L4-proposal-precise-interpreter-oop-maps-20260923.md`),
  after which they are deleted.
* The GC scans one contiguous region per thread, using each frame's
  `(locals_base, stack_base, sp)` to know which window is which (and the
  per-frame liveness / oop map for what to root).

## Staged plan

1. Introduce `SlotStack` behind the current `Frame` API: `locals` and the
   `ValueStack` become views (`&mut [CompactValue]` + indices) into it, while
   `Frame::new*` keep their signatures. Keep the pools compiled but unused.
   A/B inside one binary (`CRATONVM_JIT_NO_FRAME_SLOT_REUSE`-style switch).
2. Argument overlap for the cached/fast invoke doors (the paths that already
   hand over `(CompactValue, kind)` pairs verbatim), then the general
   dispatchers.
3. Delete the pools, `FRAME_SOA_TLS`, retired-slot reuse and the transmutes.
4. Continuations: `to_frozen_frame` / `from_frozen_frame` copy a window out and
   back in, which they already effectively do.

## Hazards to design for

* Raw frame pointers: `FrameStack`'s address-stability contract must extend to
  the slot region (growth relocates every window; the epoch covers it).
* Deopt / OSR entry build interpreter frames from JIT state; they must write
  into the window rather than build `Vec`s.
* JNI/natives that hold `&mut Frame` across a nested call must not observe a
  window moved by growth.
* `Frame::reset_for_tail_call` (which should be deleted anyway, see
  `../../internal/fixed-bugs/interpreter-L4-self-recursive-tail-call-elimination-diverges-from-hotspot-FIXED-20260923.md`).

## Expected benefit

The per-call frame build + return recycle phases (the largest measured phases
of an interpreted invoke) collapse to a few adds; argument copying disappears;
memory per frame drops to its real size. Expect the interpreted-invoke floor to
fall by a large fraction of the frame-lifecycle share measured in the
2026-08/09 invoke-phase work; measure with `CRATONVM_DBG_INVOKE_PHASES=1`.

## Risk

High effort, touches every frame producer. Stage 1's view-based API keeps the
blast radius inside `frame.rs` / `value_stack.rs` until the A/B proves it.

## Progress (wave 2): concrete staging

Nothing landed; this section replaces the staged plan above with one sized
against the tree as of 2026-09-23.

### What the blast radius actually is

* `Frame::locals` / `Frame::local_kinds` are **private**; every reader and
  writer goes through `Frame`'s accessors (`get_local*`, `set_local*`,
  `locals_snapshot`, `scan_local_objects*`, `update_local_refs`,
  `take_pool_parts*`). Moving locals into a thread slab is a `frame.rs`-only
  change.
* `ValueStack::slots` / `kinds` are private too, but `Frame::stack` is a
  `pub` field used at ~717 call sites in 25 files through `ValueStack`'s
  methods. Its API can stay; its storage can change underneath.
* Construction is the wide part: ~50 `Frame::new*` calls in 21 files outside
  `frame.rs` (8 in `threading/virtual_threads.rs`, 8 in tests, 3 each in
  `memory/roots.rs`, `deopt_resume.rs`, `exception_dispatch.rs`,
  `vm_init.rs`), plus 44 standalone `ValueStack::new` / `from_*` (tests and
  natives that build a stack with no frame).

### Stages

0. **Measure the share first.** `CRATONVM_DBG_INVOKE_PHASES=1` on
   `probes/InterpDecodedOpcodeCostProbe`-style recursion and on one
   framework boot: frame build + `pop_and_recycle_frame_with_reason` as a
   fraction of an interpreted invoke. If it is under ~25%, stop here.
1. **Locals into a per-thread slab, stack untouched.** `JvmThread` gains a
   `LocalsSlab { words: Box<[CompactValue]>, kinds: Box<[u8]>, top: u32 }`
   (reserved once, grown by doubling with an epoch like
   `FrameStack::reloc_epoch`). `Frame` keeps `locals_base: u32,
   locals_len: u16` instead of the two `Vec`s, and every accessor indexes
   the slab through a raw base pointer refreshed on growth. The pools'
   locals halves (`locals_pool`, the `FRAME_SOA_TLS` locals pair) go unused
   and are deleted in stage 3. A/B switch in the same binary
   (`CRATONVM_NO_LOCALS_SLAB`), because this is where every frame producer
   first meets the new layout.
2. **Argument overlap for the cached invoke doors only**
   (`push_cached_compact_reusing`, `emplace_cached_compact`, the paths that
   already move `(CompactValue, kind)` pairs verbatim): the callee's
   `locals_base` is the caller's operand-stack slot of the first argument,
   which requires stage 4's shared slab — so this stage is really "move the
   operand stack into the same slab", done for frames built by those doors
   and a `ValueStack` storage enum (`Owned(Vec)` / `Window{base,cap}`) for
   everyone else (the 44 standalone stacks keep `Owned`).
3. **Delete** the pools, `FRAME_SOA_TLS`, retired-slot reuse, the
   `Vec<u64> <-> Vec<CompactValue>` transmutes and `trim_retired`, once
   stage 1-2 have run the suites with the switch at its default.
4. **General dispatchers and continuations.** `to_frozen_frame` /
   `from_frozen_frame` copy the window out and back in; deopt and OSR exit
   (`deopt_resume.rs`) write into a window rather than build `Vec`s.

### Hazards found while sizing it

* `FrameStack`'s address-stability contract (`frame_ptr`, `current_ptr`,
  `reserve_stable`) covers `Frame` headers; a slab adds a second relocatable
  region. Every raw `*mut Frame` holder that also reads locals across a call
  that can push frames must re-derive the slab base, or the slab must be
  reserved (virtual memory) rather than grown.
* The GC root scans (`scan_local_objects_inner`, `ValueStack::
  scan_object_refs`) and their `update_*` twins run on other threads at a
  safepoint; the slab's base and each frame's window must be readable from
  there, i.e. published before the frame becomes visible in `FrameStack`.
* `Frame::reset_for_tail_call` assumes it owns its locals buffer; the
  self-recursive TCE it serves is itself filed for deletion
  (`../../internal/fixed-bugs/interpreter-L4-self-recursive-tail-call-elimination-diverges-from-hotspot-FIXED-20260923.md`);
  delete it before stage 1 rather than port it.

## Progress (wave 3)

**Stage-0 cleanup landed** (the one piece of stage 0 that is a code change and
cannot collide with other lanes' work): `Frame::reset_for_tail_call` is
deleted. Its only production caller, the self-recursive tail-call
elimination, was removed in wave 2, so it was dead code that the slab design
would otherwise have had to port (the hazard list above says to delete it
first). With it went `ValueStack::ensure_max_size` (its only caller; the
misplaced doc comment it left above `reset_in_place` is gone too), the two
tests that exercised them (`frame.rs`
`a_tail_call_clears_the_method_index_rather_than_stranding_it`,
`vm_init.rs` `s16_frame_reset_for_tail_call`), and `copy_args_to_locals`
became `#[cfg(test)]` (it survives as the reference layout
`pushed_locals_match_the_resize_then_copy_layout` checks the push path
against). Side effect worth knowing: `CRATONVM_FRAME_TRACE` now has no reader
outside `env_cache`'s own tests (its last use was the `[TCO_GROW]` line); it
should be retired from the flag inventory by whoever next edits it.

**Measurement plan for stage 0 proper** (not run: this lane may not run the
VM). The instrument exists: `CRATONVM_DBG_INVOKE_PHASES=1`
(`vm/src/runtime/interpreter/invoke_phases.rs`) reports per-call cycles for
`ic_lookup`, `guards`, `args`, `frame_build`, `frame_push`, `ret_total` and
`ret_recycle`, plus a `CALIB(noop)` row to subtract.

1. Workloads: (a) a `--nojit` recursion microbenchmark — `static int f(int n)
   { return n == 0 ? 0 : 1 + f(n - 1); }` at depth 1000, 10^4 repetitions,
   one warm-up pass — which is pure invoke; (b) a leaf-call loop (`int
   g(int a, int b) { return a + b; }` called 10^7 times), where locals are
   tiny; (c) one framework boot (Spring Boot petclinic or the WildFly boot)
   with the JIT on, for the population mix.
2. The share the slab can remove is `args + frame_build + frame_push +
   ret_recycle` (CALIB-subtracted) over the whole per-call total. Interleave
   5 runs per workload and take medians (in-JVM timings swing ~3x run to run
   on this host).
3. Decision: under ~25% on (a) and (c) → stop, record the numbers here and
   retire this proposal. Over it → stage 1 (locals slab behind
   `CRATONVM_NO_LOCALS_SLAB`), and re-run the same three with the switch
   both ways in one binary.

## Progress (wave 4)

**Stage-0 instrument completed** (the run itself still needs a host that may
run the VM). `CRATONVM_DBG_INVOKE_PHASES=1` measured only the TIME the frame
lifecycle costs; the proposal's other claim — how much MEMORY a frame holds
against what a window in a contiguous slot stack would need — had no
instrument. It now has one, behind the same gate (no new flag):

* `vm/src/runtime/frame.rs` `note_retired_frame_shape`, called from
  `pop_and_recycle_frame_with_reason` for every retiring interpreter frame:
  counts frames, `locals_len`, the operand stack's allocated slots
  (`ValueStack::capacity`, new) and the declared `max_stack`. Off: one relaxed
  byte load per frame pop (the `invoke_phases::on()` latch
  `count_frame_kind` already pays per frame build).
* `invoke_phases::dump` prints one more line:
  `[invoke-phases] frame shape: retired=N locals=L stack_alloc=S max_stack=M
  slots/frame; slot bytes held=H vs max_locals+max_stack=W per frame; Frame
  header=B bytes`. `H - W` is the per-frame slack the `max(max_stack, 16) + 8`
  sizing costs; `W` (minus the argument overlap) is what a window needs.

Decision rule, added to the plan above: run the three workloads of the
measurement plan once with the gate; the phase share decides whether the slab
is worth doing for speed, and `H / W` together with `B` says whether it is
worth doing for footprint even if the phase share is under 25%.

**Correction to the wave-3 note:** `CRATONVM_FRAME_TRACE` still has eight
production readers: the `[FRAME_PUSH/...]` lines in `interpreter.rs`
(`invoke_method_shared`, `execute_prebuilt_frame`), `interpreter/invoke.rs`
(stackless), `interpreter/exception_dispatch.rs` (JIT exception route) and
`interpreter/jvmti_events.rs` (`trace_frame_push`); `[FRAME_POP]` and
`[CONTINUATION_RESUME]` in `interpreter.rs`; and the invoke fast-door
admission in `execute_frame`, which it switches off so every push is traced.
Only the `[TCO_GROW]` reader went with the tail-call code, so the flag is live
and stays in the inventory (wave 4 was asked to retire it and did not).

## Progress (wave 5)

Nothing can be measured from this lane (it may not run the VM). Two things
landed, and the analysis the orchestrator should run is written out below.

### Census corrected (`vm/src/runtime/frame.rs`)

The wave-4 census had two accounting errors, both in the direction of making
the per-frame buffers look cheaper than they are:

* "held" counted the locals by LENGTH. The locals `Vec` comes from a pool and
  keeps the capacity of the largest frame it ever served, so the memory a
  frame holds is its capacity. The census now also counts
  `locals.capacity()` and prints `locals_alloc=`, and "held" uses it.
* "needed" used `locals.len()`, which the frame builder may pad past the
  declared `max_locals`. It now uses the declared `max_locals` (printed as
  `max_locals=`).

The dump line is now
`[invoke-phases] frame shape: retired=N locals=L locals_alloc=LA
max_locals=ML stack_alloc=S max_stack=M slots/frame; slot bytes held=H vs
max_locals+max_stack=W per frame; Frame header=B bytes`.

### What the two instruments cover (read before interpreting them)

* The PHASE table (`invoke_phases`) brackets one path only:
  `dispatch_static::execute_invokestatic_cached` and the in-place frame install
  it calls (`ic_lookup`, `guards`, `args`, `frame_build`, `frame_push`), plus
  the value-return arm (`ret_total`, nested `ret_recycle`). `calls=` counts
  those invokestatics. Virtual / interface invokes and the raw fast-path doors
  are NOT in the table, so its share is a statement about static calls.
* The frame-shape CENSUS counts every interpreter frame retired through
  `pop_and_recycle_frame_with_reason` — every path, including exception
  unwinds. `retired` far above `calls` is normal and says how much of the
  frame traffic the phase table does not see.
* `[site-cache] install: reuse= emplace= byvalue=` (`CRATONVM_DBG_FIELD_SITE=1`)
  says which install path built the frames; run it once alongside.

### The analysis to run

Workloads as in the wave-3 plan: (a) `--nojit` recursion
(`f(n) = n == 0 ? 0 : 1 + f(n - 1)`, depth 1000, 10^4 reps), (b) `--nojit`
leaf-call loop (`g(a, b) = a + b`, 10^7 calls), (c) one framework boot with the
JIT on. Each with `CRATONVM_DBG_INVOKE_PHASES=1`, 5 interleaved runs, medians
of every number below (in-JVM timings swing ~3x run to run on this host).

1. **Time share.** From the phase table use the CORRECTED column (it
   subtracts one `CALIB` per flat phase and three for `ret_total`):
   `lifecycle = args + frame_build + frame_push + ret_recycle`,
   `call = ic_lookup + guards + args + frame_build + frame_push + ret_total`
   (`ret_recycle` is inside `ret_total`; do not add it twice).
   `S = lifecycle / call`. Of `args`, only the copy is removable (argument
   overlap); the `guards` and the rest of `ret_total` stay.
2. **Memory ratio.** From the census: `H / W`. Above ~2 means the
   `max(max_stack, 16) + 8` sizing and pooled capacities dominate a frame's
   footprint; `LA - ML` alone is the pooled-capacity slack on the locals side
   and `S_alloc - M` the operand-stack slack. `B` (the `Frame` header) is paid
   per frame whatever the storage layout, so compare `H` with `H + B`, not
   alone.
3. **Depth sensitivity.** Workload (a) at depth 10 and depth 1000: if `S` is
   flat, the lifecycle cost is per call (the slab wins); if it falls with
   depth, the pools are already amortising it and the slab's win is the
   memory half only.
4. **Decision.** Proceed to stage 1 (locals slab behind
   `CRATONVM_NO_LOCALS_SLAB`) if `S >= 25%` on (a) and (c), or if `H / W >= 2`
   on (c) with a deep stack population. Otherwise record the three numbers
   here and retire the proposal.

### Design refinements from reading the census path

* Argument overlap needs the argument SLOT count per frame, which neither
  instrument records (`CachedBytecodeMethod::num_params` counts parameters,
  not slots: a `long` is two). If stage 1 goes ahead, add
  `arg_slots` to the census first, from the descriptor facts the invoke doors
  already cache (`descriptor_facts_cache`); `W - arg_slots` is then exactly
  what a window needs.
* The kind arrays are 1/9 of every number above. If the precise oop maps
  reach their stage 4 first (`i1-L4-proposal-precise-interpreter-oop-maps-20260923.md`;
  its stage 1 shadow landed in wave 5), the slab needs no parallel kind
  region at all, which removes one of the two relocatable regions the
  hazard list names.

## Progress (wave 6)

No code stage landed: stage 1 is gated on the wave-5 "analysis to run"
(`CRATONVM_DBG_INVOKE_PHASES=1` time share and the census memory ratio), which
needs VM runs this lane does not make. Nothing in wave 6 changed the frame
lifecycle the census measures, so the wave-5 instruments and decision rule
stand as written.

## Progress (wave 29) — lane L7

The user waived the stage-0 gate and asked for the implementation. The
orchestrator's stage-0 run on `dev` `6d39e8dcc` (no-LTO, `--nojit`, census
exact, times rough) settled the memory half anyway: recursion depth 1000
retired 11.07 M frames with locals 1.0, stack_alloc 24.0, max_stack 3.0 slots
per frame, **held 225 bytes against 36 needed (H/W = 6.2)**; the leaf and
wide-argument loops H/W = 4.8; the `Frame` header 264 bytes; **99.96% of all
frames come from the cached install paths** (`owned=4053 cached=11066056`).
The phase table is blind to those paths (`calls=344` for 11 M calls: it
brackets `execute_invokestatic_cached` only, which the fast doors bypass).

The lane cannot build or run; nothing below is measured.

### What landed: stage 1, locals AND operand stack, for the cached installs

* **`vm/src/runtime/slot_slab.rs`** (new). `SlotVec<T>`: one of a frame's four
  slot buffers, either an owned `Vec` in pieces or a window of slab memory,
  16 bytes (a `Vec` is 24), deref to a slice either way, so every reader and
  writer of locals and operand stack (the dispatch loop, the GC scans and
  remaps, freeze/thaw, the debugger's accessors) is unchanged and pays no
  branch for the choice. `SlotSlab`: chunks that are allocated once and never
  move (first 1024 slots, doubling to 65536; a larger window gets its own),
  and a bump mark `(chunk << 32) | offset`.
* **`Frame::locals` / `local_kinds` and `ValueStack::slots` / `kinds`** are
  `SlotVec`s. `Frame` gained `slab_mark`. Header 264 → 240 bytes (four
  16-byte `SlotVec`s for four 24-byte `Vec`s, plus the 8-byte mark); the
  census line's `Frame header=` should confirm it.
* **`FrameStack` owns a `SlotSlab`.** The cached installs take ONE window per
  frame: `n` locals (`max_locals`, raised for the arguments) then the padded
  operand stack (`max(max_stack, 16) + 8`, unchanged), each slot with its
  kind byte:
  * retired-slot rebuilds: `push_cached_compact_reusing` (fast doors),
    `push_cached_value_reusing` (general dispatchers);
  * first call at a depth: new `emplace_cached_compact_args` /
    `emplace_cached_value_args`, which the two call sites now use instead of
    `take_cached_*_parts` + `emplace_cached_compact` (the CROSS-LANE edits in
    `invoke_fast.rs` and `jvmti_events.rs`); the pooled pair is what they run
    with the flag set.
  * Everything else (by-value pushes, deopt, OSR exits, thaw, reflection,
    `Frame::new*`) still builds owned buffers from the pools: they are 0.04%
    of frames.
* **Release is a mark.** Every push records the slab mark in the frame; every
  drop in depth (`retire_top`, `truncate`, `truncate_hard`, `pop`, `clear`)
  releases the slab to the lowest leaving frame's mark, with `min`, so a
  frame no stack marked (`SLAB_MARK_UNKNOWN`) or a stale mark can leak a
  window until a lower pop but never hand a live one out twice. The
  interpreter's return retires in place (`retire_top`), and the next call at
  that depth takes exactly the window the retired frame used — the retired
  slot's buffers are reused, as before, only now they are slab memory.
* **A retired window is never written.** A rebuild of a windowed retired slot
  always takes a fresh window (`retired_slot_takes_a_window`); a rebuild of a
  slot that still owns pooled buffers keeps them (no free-then-allocate); the
  pooled resets (`reset_cached_compact`, `reset_cached_value`,
  `ValueStack::reset_in_place`) take their buffers out as `Vec`s and put them
  back, so a window reaching them turns into a fresh owned buffer instead of
  being written.
* **Leaving by value detaches.** `FrameStack::pop` and the `Vec<Frame>`
  conversions copy a window into owned buffers (`Frame::detach_from_slab`);
  `take_pool_parts*` of a windowed frame returns four EMPTY `Vec`s, which
  `JvmThread::harvest_retired_slot` already skips.
* **Switch:** `CRATONVM_JIT_NO_LOCALS_SLAB` (token `CRATONVM_JIT=-locals-slab`,
  `env_cache::no_locals_slab`, inventory row and generated docs updated).
  Unset (default) = slab windows; set = the pooled buffers, as `dev`. One
  build measures both. The flag is read on the emplace path and for a husk
  (a harvested retired slot) only; a windowed retired slot and a pooled one
  each keep their own kind without reading it.

### Positive controls (`CRATONVM_DBG_INVOKE_PHASES=1`)

* `[invoke-phases] slot slab: windowed=W of retired=R (CRATONVM_JIT_NO_LOCALS_SLAB=unset: slab windows)`:
  W is the census's count of retired frames whose buffers were a window. On
  `L7W29ContiguousStackBench` W must be within a few thousand of R with the
  slab on, and 0 with `CRATONVM_JIT_NO_LOCALS_SLAB=1`.
* `[invoke-phases] frame lifecycle: installs=I install_cyc=X pops=P pop_cyc=Y per event`:
  new phases `P_SLOT_BUILD` (every cached install inside `FrameStack`, both
  arms, every door and dispatcher) and `P_SLOT_RET` (every
  `pop_and_recycle_frame_with_reason` recycle). This is the instrument for
  the A/B the orchestrator asked for: `install_cyc` and `pop_cyc` with the
  slab on vs off. Off, each costs one `now()` load per install/pop and a
  register test (`t0 != 0`). The census and frame lines now print even when
  the invokestatic table is empty.

### Unit tests (run on the host)

`runtime::slot_slab::tests`: window reuse after release, releases that only
move down, 3000 windows across chunks never moving while the stack is
popped to half and pushed again, a window bigger than any chunk, owned
`SlotVec` round trips with its capacity, a window yields no `Vec` and copies
on `make_owned`. `runtime::frame::tests`:
`a_windowed_frame_holds_what_a_pooled_frame_holds` (long / int / double /
reference arguments, both argument representations, window adjacency),
`windowed_frames_keep_their_locals_across_deeper_pushes_and_pops` (3000
frames, retire half, rebuild in the same windows, truncate, clear),
`owned_frames_between_windowed_ones_release_the_right_windows`,
`a_popped_windowed_frame_leaves_with_its_own_copy`,
`the_gc_scans_and_remaps_a_windowed_frames_locals_and_stack`. They call the
slab arms directly, so they pass with the flag either way.

### Hazards, checked against the tree as of `6d39e8dcc`

* **Relocation:** none. Chunks never move; growing the chunk list moves only
  their headers. `FrameStack::reloc_epoch` still covers the `Frame` headers;
  no second epoch exists. A `*mut Frame` held across a push reads the same
  window after it.
* **GC root scanning** reads windows through the unchanged accessors, bounded
  by `len` / `locals_len()`; a window is fully laid (locals) or kind-cleared
  (stack) before `depth` admits the frame, as the pooled buffers were.
* **Deopt / OSR entry, JVMTI / JDWP locals, obsolete-frame moves** use the
  accessors or build owned frames (`new_pooled`, `from_frozen_frame`);
  `adopt_redefined_body` swaps code only. No change needed.
* **Continuations** copy out (`to_frozen_frame`) and thaw into owned frames.
  The slab travels with its `FrameStack`, so a virtual thread's stack keeps
  its windows across carriers.
* **JNI natives holding `&mut Frame` across a nested call:** the nested
  frames' windows are above the held frame's mark; untouched.
* **`execute_frame` leaving the finished frame on the stack:** read in place
  or popped by value (`pop` detaches).
* **New with this stage:** a pointer to a POPPED frame's buffers that is used
  after the stack popped below it and pushed again now reads or writes the
  new frame's window, where in the pooled layout it touched a dead buffer
  only. The raw-pointer contract already forbids it ("the frame must not have
  been popped"); no such use was found, but a corruption that shows up only
  with the slab on points here first.
* **`CRATONVM_JIT_NO_FRAME_SLOT_REUSE` with the slab on** (a diagnostic
  combination): `recycle_top_frame_in_place`'s pooled arm routes a windowed
  frame's four empty `Vec`s into the pools, so the by-value constructors
  allocate afresh while that combination runs. Harmless, not the default.

### What remains

1. **Measure** (orchestrator): `L7W29ContiguousStackBench` and the stage-0
   programs, fat LTO, slab on vs `CRATONVM_JIT_NO_LOCALS_SLAB=1` in one
   build, interleaved and pinned, plus `dev` for the flag-off side's own cost
   (the pooled resets now take and put their `Vec`s, a few moves and two
   predictable branches per rebuild, and `retire_top` / `truncate` load a
   mark). Expected: flat to a few percent down per call. Suites and probes
   with the default (slab on), and `L7W29ContiguousStackProbe` both ways.
2. **Stage 2, argument overlap.** Needs: (a) the callee window to start
   inside the caller's window, so the slab top while a frame runs is its
   operand stack's current top, not its padded end — i.e. a frame's window
   end becomes `stack_base + len` at the call, with the callee's `slab_mark`
   the caller's first argument slot; (b) **a category-2 argument occupies ONE
   operand-stack slot and TWO local slots in this VM** (`push_long_unchecked`
   writes one slot; the locals keep the upper half as filler), so the
   arguments cannot simply be reused in place: lay them from the last to the
   first, each moved up by the number of category-2 arguments before it (the
   destination is never below the source, so the walk is safe in place), and
   only calls with no category-2 argument skip the copy entirely; (c) the
   door's `read_args_verbatim` validation stays (it reads the slots it
   checks), the `ArgSlots` copy goes; (d) a callee that does not fit the
   current chunk falls back to a fresh window and copies (rare).
3. **Drop the operand-stack padding for windowed frames.** The census shows
   the padding is most of what a frame holds (stack_alloc 24 against max_stack
   3). A window sized `max_locals + max_stack` would bring H/W near 1, but the
   `max(16) + 8` padding exists in 22 places and its reason is not written
   down; first count pushes past the declared `max_stack` under a
   `CRATONVM_DBG_*` gate on the suites, then size windows exactly.
4. **Stage 3** (only after 1 has run the suites with the switch at its
   default): delete the pooled arms, `FRAME_SOA_TLS`, the thread pools'
   frame role, `take_pool_parts*`, the transmutes and `trim_retired`'s
   buffer role; `SlotVec` then needs no owned case for frames built by the
   cached paths.

## Progress (wave 32) — orchestrator: stage 1's call-path cost, measured and fixed

**The regression.** Fat LTO, `--nojit`, interleaved on cores 5 and 3, three
rounds each: wave 29 made the call rows 5–15% slower than wave 28 (`dev29`):
`InvokeDoorCostBench` `static-call` +9.9%, `L7W28VirtualDoorSplitBench`
`staticCall` +9.9% and `syncMono` +10.8%, `L7W29ContiguousStackBench` `leaf-g`
+14.7%. A bisect over wave 29's merges put all of it on this lane's merge
(`a015166b2`); the earlier merges were within ±2.5%. Switching the slab off
did not recover it.

**The cause.** About half the retired slots keep pooled buffers (a slot that
first held a pooled frame is rebuilt in its own buffers:
`FrameStack::retired_slot_takes_a_window`), so the pooled reset runs on most
calls of the benches. Stage 1 made that reset take each of its four buffers
out as a `Vec` and put it back (`SlotVec::take_vec` / `from_vec`, in
`Frame::reset_cached_compact`, `reset_cached_value` and
`ValueStack::reset_in_place`), so that a window is never written through a
retired frame. `perf` (timer sampling) showed the pooled arm at 2–4% self time
on top of `reset_cached_tail`. The slab-off run pays the same round trip,
which is why it recovered nothing.

**The fix.** An OWNED buffer that already has room for the new frame is
rewritten in place (`SlotVec::owned_capacity`, `as_mut_ptr`, `set_len`), laid
by the same `lay_compact_args` / `lay_value_args` the window arm uses; only a
window, or a buffer that must grow, takes the round trip. The pooled arm is
also out of line (`FrameStack::reset_pooled_compact_at` /
`reset_pooled_value_at`), as are `push_frame_verbatim`'s first-call-at-a-depth
arm and `recycle_top_frame_in_place`'s pooled arm. That split alone
(`lto-w32a`) moved nothing: the rows stayed +8–14%.

**Measured** (`lto-w32e`, the fix, against `lto-dev29`, same interleaved run):
`static-call` +0.4%, `staticCall` +0.8%, `leaf-g` +1.1%, `super-call` −1.7%,
`virtual-mono` 0.0%, `rec-d10` −1.5%, `deep-d6000` −0.2%; the largest row left
is `syncMono` +4.9%, inside this host's floor for that row. Stage 1 is now
cost-neutral on these benches, not a gain: the next step is still converting
a pooled slot to a window once, so the window arm (no pool, no reset of four
buffers) is what the benches run.

## Progress (wave 33) — orchestrator: every warm slot is a window

The wave-32 "next step" is done. A retired slot that still holds pooled
buffers (`FrameStack::retired_slot_holds_pooled_buffers`: not a window, a
non-empty operand stack, the slab on) is converted once, at its next cached
install: `JvmThread::convert_retired_slot_to_window` hands its four buffers
to the thread pools (not freed, so a by-value push at that depth finds them
again) and leaves the slot a husk, which the existing husk rule rebuilds in a
window. Nothing above the slot is trimmed. Both reuse call sites convert:
`invoke_fast::push_frame_verbatim` and the general dispatchers'
`jvmti_events` install. From then on the slot is a window at every call until
a by-value push harvests it again.

**Measured** (`lto-w33a` against `lto-dev29`, the same interleaved setup;
`lto-w32e` in the same run for reference): `static-call` −3.9% (w32e −0.6%),
`super-call` −6.3% (−4.8%), `private-same` −2.6%, `virtual-mono` −2.2%,
`staticCall` −1.4% (+1.0%), `syncMono` −0.1% (+4.6%), `leaf-g` −1.3% (+4.8%),
`virtual` −1.5% (+3.1%), `mixed` −2.6%; `ctor` +3.5% is the one row up, at
its floor. `perf` on `L7W29ContiguousStackBench`: `reset_cached_tail` and
the pooled reset are gone from the profile; the window install is inside
`push_frame_verbatim` (19% self).

Stages 2 (argument overlap) and 3 (delete the pools) remain as the wave-29
section describes.

## Progress (wave 37) — lane L7: stage 2, argument overlap, behind a per-VM switch

The lane cannot build or run; nothing below is measured. Commits
`4e3e82e42` (the mechanism, lane L7's files plus the flag inventory) and
`325886744` (CROSS-LANE: the door wiring in `invoke_fast.rs`).

### The design

* **Where the callee's window starts.** A frame installed by a fast door
  (`invoke_fast::push_frame_verbatim`: the static, special and virtual doors)
  takes its window at the caller's first argument slot, which the door has
  just popped (`discard_top`). Its locals begin on the arguments, as in
  HotSpot's interpreter; its padded operand stack follows its `n` locals as in
  stage 1. `FrameStack::push_cached_compact_overlapping` builds it;
  `SlotSlab::alloc_overlapping` hands out the memory.
* **Why that memory is free.** Invariant: every live slot of every frame
  below the top frame lies below the top frame's window start (in slab
  order). A callee starts either at the caller's live stack top (overlap) or
  at the mark, which is at or past the end of every live window (a stage-1
  window), so the invariant holds by induction, and everything from the top
  frame's live stack top up to its window end, and from there to the mark, is
  dead: the top frame's free stack slots, the free stack slots of the frames
  below that it overlapped, and released windows. `alloc_overlapping`
  refuses unless the mark is in the cached chunk and
  `chunk base <= start < end <= mark address` (`start` the caller's stack
  top, `end` its window end). `start < end` puts `start` strictly inside the
  caller's window, and chunks are disjoint allocations, so the caller's
  window is in the cached chunk -- an address comparison alone would prove
  nothing, chunk addresses are not ordered.
* **Marks.** The callee's `slab_mark` is the mark BEFORE it (at or past the
  caller's window end), so `retire_top` / `truncate` / `pop` restore exactly
  the caller's state. The slab's mark moves up to the callee's window end
  when the window reaches past it and never down (the region in between may
  be a lower frame's free stack slots, which it needs back when it is the top
  again). A plain stage-1 window taken later from the caller lands at or past
  the caller's window end
  (`nested_overlaps_restore_the_slab_and_a_plain_window_lands_past_the_caller`).
* **Category-2 arguments.** A `long` / `double` is ONE operand-stack slot and
  TWO local slots here. A call with none keeps the argument values where they
  are and rewrites only their kind bytes (a category-1 local's mark is
  `LKIND_OTHER`) and the filler past them (`lay_overlapped_args`). A call with
  one lays the locals again from the door's `ArgSlots` copy (never from the
  slab, which the lay overwrites) -- counted as `relaid_cat2`.
* **The caller looks popped.** Its operand stack's `len` excludes the
  arguments before the callee exists, so the GC root scans, the remaps
  (`update_object_refs`, `update_local_refs`), freeze/thaw and every frame
  reader see each argument once, as the callee's local.
* **The one shared moment: a value return.** The return arms push the result
  into the caller BEFORE the callee leaves (wave 4: the result must be rooted
  across `MethodExit`, the synchronized monitor's release and `FramePop`,
  which can collect), and that push lands on the callee's first slot. For
  that span the slot would be a root of two frames, and a moving collection
  remaps through a `PointerMap` keyed by old addresses: a second remap is
  wrong whenever an object's new address is another relocated object's old
  one. So the fast value-return arm and the decoded `Return` arm call
  `Frame::release_caller_overlap` first on an overlapped callee (its locals'
  and operand stack's lengths go to 0). The test is one bit of the frame
  header (`SlotVec`'s new `SHARED_BIT`) on every value return. Nothing reads a
  returning frame's locals after its value is taken: JVMTI cannot read locals
  (`can_access_local_variables` is refused) and the door does not overlap
  under JDWP (`debugger_observes_locals`), which could read a returning
  frame's locals at `MethodExit`. The void return, the OSR return (it pops,
  then pushes) and the exception unwind (it pops every frame above the
  handler before clearing the handler's stack) never share a slot.
* **Refusals** (the stage-1 paths run instead, unchanged): the switch off; a
  JDWP port; `CRATONVM_JIT_NO_FRAME_SLOT_REUSE` /
  `CRATONVM_JIT_NO_FRAME_EMPLACE`; the caller not the top frame; an owned
  (not windowed) caller; the callee's window not fitting the cached chunk (a
  deep recursion declines at each chunk boundary and overlaps again inside
  the next chunk). A virtual thread never reaches a door
  (`invoke_fast_door_on`), so no overlapped frame is ever frozen. The general
  dispatchers (`jvmti_events::install_cached_frame`, arguments already decoded
  to `Value`s) do not overlap; a door callee of a frame they built does.
* **The switch** is per VM: `VmConfig::overlap_interpreter_args`, read once
  in `VmConfig::default` from `CRATONVM_JIT_OVERLAP_ARGS` (token
  `CRATONVM_JIT=+overlap-args`, default OFF; inventory row, surface fixture
  and both generated docs updated). No process global.

### What each frame reader sees (the hazard list, checked)

| reader | while the callee runs | at the callee's value return |
|---|---|---|
| GC root scan / remap (`scan_local_objects*`, `ValueStack::scan_object_refs*`, `update_*`) | each argument once, as the callee's local | the result once, as the caller's stack top (the callee released) |
| stack walks, `Throwable` traces, thread dumps, JFR | method and pc only | same |
| JVMTI locals | the capability is refused | same |
| JDWP locals | no overlap under JDWP | same |
| deopt / OSR entry (`try_osr`) | the callee's locals through the accessors | the OSR return pops before it pushes |
| exception unwinding | the frames above the handler are popped before its stack is cleared and the exception pushed | -- |
| obsolete-frame moves (`adopt_redefined_body`) | code swap only; the window is untouched | -- |
| freeze / thaw | never overlapped (a virtual thread reaches no door) | -- |
| `FrameStack::pop` / the `Vec<Frame>` conversions | copy the window (`detach_from_slab`), as in stage 1 | -- |
| frame-shape census (`note_retired_frame_shape`) | -- | a released frame counts 0 locals, so `locals=` reads low with the switch on (the capacity columns are unaffected) |

### Costs

* **Off** (the default): one load of `shared.config.overlap_interpreter_args`
  and a predicted branch per door call; one bit test of the returning frame's
  `locals` header per value return. Nothing is moved by value (`ArgSlots`
  stays in the door's frame, passed by `&`).
* **On**: an out-of-line call (`push_frame_overlapping`, so the switch-off
  door keeps its code) with three latched-flag / config loads, the overlap
  test (a handful of compares), the kind bytes of the arguments and the
  filler past them; no argument value is written unless a `long` / `double`
  is among them. The `ArgSlots` copy the door makes to VALIDATE the arguments
  (`read_args_verbatim`) is still made.

### Positive control

`CRATONVM_DBG_INVOKE_PHASES=1` prints at exit
`[invoke-phases] arg overlap: overlapped=N relaid_cat2=M declined=D released_at_return=R (installs above include them)`.

```sh
javac -d /tmp/p37 tools/probes/interp/L7/L7W37ArgOverlapProbe.java
CRATONVM_JIT_OVERLAP_ARGS=1 CRATONVM_DBG_INVOKE_PHASES=1 \
  target/release/cratonvm --nojit -cp /tmp/p37 L7W37ArgOverlapProbe 2>&1 | grep 'arg overlap'
#   expect N > 0, M > 0 (the cat2-* rows), R > 0, D > 0 but small (the deep rows' chunk boundaries)
CRATONVM_DBG_INVOKE_PHASES=1 target/release/cratonvm --nojit -cp /tmp/p37 L7W37ArgOverlapProbe 2>&1 | grep 'arg overlap'
#   expect all four 0
```

### Correctness gate before any default flip

* `L7W37ArgOverlapProbe` and `L7W29ContiguousStackProbe`, the switch on and
  off, with and without `--nojit`, and `--compatible`: HotSpot's stdout each
  time (both headers).
* The full probe set and the core suite once with `CRATONVM_JIT_OVERLAP_ARGS=1`
  in the environment, under all four collectors (the release at the return is
  what a moving collector depends on).
* Unit tests: `runtime::slot_slab::tests::an_overlapping_window_starts_where_asked_and_its_release_restores_the_mark`,
  `a_shared_window_keeps_its_size_and_an_owned_buffer_cannot_be_shared`;
  `runtime::frame::tests::an_overlapped_frame_holds_what_a_fresh_window_holds`,
  `an_overlapped_argument_is_one_root_and_the_value_return_releases_it`,
  `nested_overlaps_restore_the_slab_and_a_plain_window_lands_past_the_caller`,
  `an_overlap_without_a_windowed_caller_is_refused_and_changes_nothing`.

### The A/B the orchestrator should run

One fat-LTO binary, the switch both ways, interleaved and pinned, 5 rounds,
medians, repeated on a second core (the host floor is 8-40% between runs, so
compare only within one interleaved run):

```sh
B=bin/cvm-lto-w37   # CARGO_TARGET_DIR=target-lto cargo build --release -p cratonvm-cli --bin cratonvm
P=/tmp/p37b; mkdir -p $P
javac -d $P tools/probes/interp/L7/L7W29ContiguousStackBench.java \
  tools/probes/interp/L4/InvokeDoorCostBench.java \
  tools/probes/interp/L7/L7W28VirtualDoorSplitBench.java tools/probes/interp/L2/TypeCheckBench.java
for core in 5 3; do for r in 1 2 3 4 5; do for ov in 0 1; do
  for b in L7W29ContiguousStackBench InvokeDoorCostBench L7W28VirtualDoorSplitBench TypeCheckBench; do
    CRATONVM_JIT_OVERLAP_ARGS=$ov taskset -c $core $B --nojit -cp $P $b \
      >/dev/null 2>>ab37-$b-ov$ov-c$core.txt
  done
done; done; done
```

Rows to read: `L7W29ContiguousStackBench` `rec-d1000`, `rec-d10`, `leaf-g`,
`mixed` (the category-2 re-lay), `virtual`, `deep-d6000` (the chunk-boundary
declines); `InvokeDoorCostBench` `static-call`, `private-same`, `super-call`,
`virtual-mono`; `L7W28VirtualDoorSplitBench` `staticCall`, `syncMono`;
`TypeCheckBench` `classMono` (a control: one call per iteration). Expected:
`rec-d1000` and `deep-d6000` down (a frame's share of the slab falls by its
argument slots and the arguments are never rewritten), the leaf rows flat to
slightly down, `mixed` flat (it re-lays), no row up beyond the floor.
`CRATONVM_DBG_INVOKE_PHASES=1` on `L7W29ContiguousStackBench` both ways gives
`install_cyc` for the same A/B. If `on` is flat everywhere, stage 2b below is
what could still pay; if a row is up, read `declined` first (a refusal costs
the overlap test and then the stage-1 install).

### What remains

1. **Measure** (above), then flip the default if the rows allow it; a flip is
   the `Default` read plus the inventory row.
2. **Stage 2b: validate without copying.** The door still copies up to nine
   `(slot, tag)` pairs into `ArgSlots` to validate them. With overlap on, a
   validate-only walk over the caller's slots (lane L4's `read_args_verbatim`)
   and an in-place re-lay for category-2 calls (from the last argument to the
   first, each moved up by the number of category-2 arguments before it: the
   destination is never below its source) would remove the copy. The receiver
   read (`ArgSlots::receiver_ptr`) and the monitor handoff need the receiver
   only.
3. **The general dispatchers.** Their arguments are `Value`s decoded (and
   coerced) off the stack; overlapping there means laying the coerced values
   back into the popped slots, which pays only if those installs are a real
   share of a workload (`[site-cache] install: reuse= emplace= byvalue=`).
4. **Operand-stack padding** (wave-29 item 3) and **stage 3** (delete the
   pools) are unchanged.

## Progress (wave 38) — lane L7: stage 2b, arguments validated and laid in place

The lane cannot build or run; nothing below is measured. Behind the SAME
per-VM switch as stage 2 (`VmConfig::overlap_interpreter_args`,
`CRATONVM_JIT_OVERLAP_ARGS=1`, default off): with it on, "stage 2" now means
stage 2 plus 2b for the static and virtual doors. No new flag.

### What changed

* **The static door** (`invoke_fast::execute_invokestatic_fast_door`) and
  **the virtual door** (`dispatch_virtual::execute_invokevirtual_fast_door`)
  validate their arguments where they are (`invoke_fast::validate_args_verbatim`:
  `read_args_verbatim`'s bounds and per-slot checks, factored into
  `arg_read_in_bounds` / `arg_tag` / `arg_slot_is_verbatim`, which
  `read_args_verbatim` now calls too, `#[inline(always)]`) and never fill an
  `ArgSlots`. The virtual door reads its receiver from the validated slot
  (`receiver_ptr_on_stack`), after the re-validation an inline compile
  already forced.
* **`invoke_fast::push_frame_in_place`** (out of line) commits: the stage-2
  refusals (`CRATONVM_JIT_NO_FRAME_SLOT_REUSE`, `CRATONVM_JIT_NO_FRAME_EMPLACE`,
  a caller that is not the top frame, a debugger that reads locals), then
  `FrameStack::push_cached_compact_in_place` with the arguments' descriptor
  tags (from the callee's `DescriptorFacts`, 9 bytes on the stack).
* **`FrameStack::push_cached_compact_in_place`** asks the slab for a window
  starting `nargs` slots below the caller's stack top
  (`SlotSlab::alloc_overlapping`, unchanged), and only once it is granted does
  the caller give the arguments up (`discard_top`). The locals are laid by
  `lay_overlapped_args_in_place`: with no `long` / `double` only the kind
  bytes change, as in stage 2; with one, argument `i` moves up by the number
  of category-2 arguments before it, laid from the last to the first (each
  destination is at or above its source and above every source not yet read),
  instead of stage 2's re-lay from the door's copy. The window, the marks, the
  `SHARED_BIT` release at a value return and every frame reader are stage 2's,
  unchanged.
* **A refusal changes nothing** (the arguments are still on the caller's
  stack), and only then does `push_frame_in_place` copy them
  (`read_args_verbatim`, which cannot fail after the validation: nothing
  between them can safepoint) and run the stage-1 install
  (`push_frame_stage1`, the tail of `push_frame_verbatim` factored out
  `#[inline(always)]`, so `push_frame_verbatim` keeps its code).
* **The non-virtual door** (`invokespecial`, private `invokevirtual`,
  constructors) still copies: its prelude hands the copy to the frameless
  field-store constructor (`field_ctor_stores_answered` reads argument
  values), and splitting that is lane L4's door. It keeps stage 2 as it was.

### Costs

* **Off** (the default): per static or virtual door call, one load of
  `shared.config.overlap_interpreter_args` and a predicted branch at the read,
  and one at the push (the virtual door: one more at the receiver read). The
  doors grew by the inlined validate-only loop and a call; this is a
  code-layout change on the hottest path, so the off side must be measured
  against wave 37 too (below), not assumed flat.
* **On**: no `ArgSlots` stores (16 bytes per argument), and a category-2
  call moves its arguments within the window instead of re-laying them from
  a copy; a declined call validates twice (the second time while copying).

### Unit test

`runtime::frame::tests::an_in_place_overlap_lays_what_a_fresh_window_holds`:
four descriptors (category-2 first, middle, last, and none; no arguments),
each laid in place from the caller's stack and compared slot for slot with a
fresh stage-1 window, the callee's locals starting on the first argument, and
a refused install (an owned caller) leaving the argument on the caller's
stack.

### Probe and positive control

`tools/probes/interp/L7/L7W38InPlaceArgsProbe.java` (category-2 arguments in
every position through both doors, virtual and interface recursion, GC
across in-place frames, unwinding, chunk-boundary recursion), plus wave 37's
`L7W37ArgOverlapProbe` and `L7W29ContiguousStackProbe`: switch on and off,
default and `--nojit`, and `--compatible`, HotSpot's stdout each time.

```sh
javac -d /tmp/p38 tools/probes/interp/L7/L7W38InPlaceArgsProbe.java
CRATONVM_JIT_OVERLAP_ARGS=1 CRATONVM_DBG_INVOKE_PHASES=1 \
  target/release/cratonvm --nojit -cp /tmp/p38 L7W38InPlaceArgsProbe 2>&1 | grep 'arg overlap'
#   expect in_place=K > 0 and relaid_cat2=M > 0; K <= overlapped + relaid_cat2
#   (the non-virtual door's overlaps are stage 2's, not in place)
CRATONVM_DBG_INVOKE_PHASES=1 target/release/cratonvm --nojit -cp /tmp/p38 L7W38InPlaceArgsProbe 2>&1 | grep 'arg overlap'
#   expect all five 0
```

### The measurement plan

Four sides in one fat-LTO interleaved run, pinned, 5 rounds, medians,
repeated on a second core (the host floor is 8-40% between runs on the
layout-sensitive rows, so compare only within one run):

* `w37` switch off: the wave-37 landing binary, the baseline;
* `w37` switch on: stage 2 alone (the orchestrator's wave-37 A/B, repeated
  here so every side is in one run);
* `w38` switch off: does the off path (the extra branches and the larger
  doors) cost anything? It must be flat to `w37` off;
* `w38` switch on: stage 2 + 2b; against `w37` on it isolates 2b.

```sh
P=/tmp/p38b; mkdir -p $P
javac -d $P tools/probes/interp/L7/L7W29ContiguousStackBench.java \
  tools/probes/interp/L4/InvokeDoorCostBench.java \
  tools/probes/interp/L7/L7W28VirtualDoorSplitBench.java tools/probes/interp/L2/TypeCheckBench.java
for core in 5 3; do for r in 1 2 3 4 5; do
  for side in w37:0 w37:1 w38:0 w38:1; do B=bin/cvm-lto-${side%:*}; ov=${side#*:}
    for b in L7W29ContiguousStackBench InvokeDoorCostBench L7W28VirtualDoorSplitBench TypeCheckBench; do
      CRATONVM_JIT_OVERLAP_ARGS=$ov taskset -c $core $B --nojit -cp $P $b \
        >/dev/null 2>>ab38-$b-${side%:*}-ov$ov-c$core.txt
    done
  done
done; done
```

Rows to read, and what each answers:

| row | door | expected `w38` on against `w37` on | expected `w38` off against `w37` off |
|---|---|---|---|
| `L7W29ContiguousStackBench` `rec-d1000`, `rec-d10`, `deep-d6000` | static, int arguments | flat to slightly down (no copy) | flat |
| `leaf-g` | static, `g(int, int)` | slightly down | flat |
| `mixed` | static, `h(long, double, int)` / `k(double, long)` | down: the in-place shift replaces the re-lay from a copy | flat |
| `virtual` | virtual door | slightly down | flat |
| `InvokeDoorCostBench` `static-call`, `virtual-mono` | static, virtual | slightly down | flat |
| `InvokeDoorCostBench` `private-same`, `super-call`, `ctor` | non-virtual door (not converted) | flat: a control | flat |
| `L7W28VirtualDoorSplitBench` `staticCall`, `syncMono` | static, virtual synchronized | slightly down | flat |
| `TypeCheckBench` `classMono` | a control, one call per iteration | flat | flat |

`CRATONVM_DBG_INVOKE_PHASES=1` on `L7W29ContiguousStackBench` for each side
gives `install_cyc` for the same A/B and the `in_place=` count (non-zero only
on `w38` on). Read it this way:

* `w38` off up against `w37` off on a static or virtual row: the off path's
  layout cost; move the validate-only read out of line (a call on the `on`
  side only) before anything else.
* `w38` on flat against `w37` on everywhere: the copy was never the cost; 2b
  stays behind the switch, and stage 3 (delete the pools) is next.
* `mixed` up on `w38` on: read `declined` first (a refusal validates twice),
  then `relaid_cat2`.

Do not flip the default from this section; the orchestrator decides from the
A/B.
