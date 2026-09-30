# Shared arenas still free nothing on close: three raw paths hold no access window

Status: OPEN
Area: `native-builtins/src/lib.rs` (`native_scoped_memory_*`),
`native-builtins/src/lang_invoke.rs` (`layout_vh_get`, `layout_vh_set`),
`native-builtins/src/panama.rs` (`pe_downcall_invoke`),
`native-builtins/src/phases_late/foreign_ffm.rs` (`p67_shared_close_frees`)
Severity: MEDIUM (unbounded native leak for `Arena.ofShared()`; turning the
free on before this lands would be a use-after-free under a racing close)
Found by: round 12 wave 5 lane ffm2

## What landed in wave 5 and what is left

`r12w3-ffm-arena-close-frees-nothing-in-the-default-model` is fixed for
CONFINED arenas: `pe_arena_allocate_impl` records each block's id in one
`long[]` in the session's close-action list
(`foreign_ffm::p67_session_record_block`), and `p67_session_run_close_actions`
frees them after `justClose` flipped the state and ran the close handshake.
Switch `CRATONVM_FFM_ARENA_CLOSE_FREES` (default on).

A confined arena is safe because every checked path checks the OWNER thread
before the state (`pe_session_check_open`, `p67_session_check_valid`), so no
other thread can be past a check on one of its segments, and the owner is the
closer. A SHARED arena has no such argument: another thread can have passed the
state check and not yet touched the block when the close frees it. The close
handshake (`ffm_fast::close_handshake_begin` + `InFlight::wait`) closes that
window only for code that holds an `ffm_fast::AccessWindow` from before its
check to after its last touch. After wave 5 these do: the JIT element fast
path, `pe_segment_get_impl` / `pe_segment_set_impl`, `fill`, both segment
`copy`s, `copyFrom`, `mismatch`, `getString`, `setString`, `toArray`, and
`foreign_ffm`'s `p67_segment_get_width` / `p67_segment_set_width` /
`p67_segment_copy_to_array`. These do NOT:

1. `native-builtins/src/lib.rs` ~34188-34250: the nine
   `native_scoped_memory_*` natives (`ScopedMemoryAccess.get/put{Byte,Short,
   Int,Long}` and `copyMemory`) check the session argument
   (`p67_check_scoped_session_arg`) and then go to `Unsafe` with no window.
   They carry every `asByteBuffer()` view access and the Vector API.
2. `native-builtins/src/lang_invoke.rs` ~1409 `layout_vh_get` and ~1458
   `layout_vh_set`: `layout_vh_access` checks the scope
   (`p67_segment_check_scope`) and hands back an address the caller then
   reads or writes, with no window.
3. `native-builtins/src/panama.rs` `pe_downcall_invoke`: wave 5 added the
   scope check HotSpot's session acquire implies (a closed arena's segment is
   `IllegalStateException`, another thread's confined one
   `WrongThreadException`), but a shared close racing the call can still free
   a block the C code is using. HotSpot ACQUIRES each by-reference argument's
   session for the call (a close meanwhile is `IllegalStateException: Session
   is acquired by N clients`).

So `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` stays default OFF.

## Proposed fix (exact)

### 1. `native-builtins/src/lib.rs` (nobody owns it in wave 5)

In each of `native_scoped_memory_get_byte`, `_put_byte`, `_get_short`,
`_put_short`, `_get_int`, `_put_int`, `_get_long`, `_put_long` and
`native_scoped_memory_copy_memory`, insert as the FIRST statement:

```rust
    // Round 12 wave 5 (lane ffm2 patch): held from before the session check to
    // after the Unsafe access, so a shared close waits for it (see
    // `ffm_fast::AccessWindow`).
    let _window = crate::ffm_fast::AccessWindow::open();
```

### 2. `native-builtins/src/lang_invoke.rs` (hunter3's file in wave 5)

`layout_vh_get`: replace

```rust
    let Some((addr, _)) = layout_vh_access(ctx, shape, args)? else {
        return Ok(Some(Value::Object(None)));
    };
    let value = match addr {
```

with

```rust
    // Round 12 wave 5 (lane ffm2 patch): an access window from before the
    // scope check to the read; dropped before the boxing allocation below.
    let window = crate::ffm_fast::AccessWindow::open();
    let Some((addr, _)) = layout_vh_access(ctx, shape, args)? else {
        return Ok(Some(Value::Object(None)));
    };
    let value = match addr {
```

and insert `drop(window);` on the line after that `match`'s closing `};`
(before `if shape.carrier == FFM_ADDRESS_CARRIER {`).

`layout_vh_set`: insert as its first statement

```rust
    // Round 12 wave 5 (lane ffm2 patch): see `layout_vh_get`.
    let _window = crate::ffm_fast::AccessWindow::open();
```

### 3. Downcall session acquire (`panama.rs`, lane ffm's file)

In `pe_downcall_invoke`, after the wave-5 scope-check loop: for each segment
argument whose session `pe_segment_session` resolves, call a new
`foreign_ffm::p67_session_acquire_for_call(ctx, &mut session)` (a
`pub(crate)` wrapper over the private `p67_session_acquire`), pin the
sessions across `ffi_call` with the other pins, and release them
(`p67_session_release`) after the call on every exit path. The acquire count
is a plain field today; make its update a CAS (or take the session's monitor)
before shared arenas rely on it, or two racing acquires lose one.

### 4. Then flip the switch

`foreign_ffm::p67_shared_close_frees`: `runtime_flag_on` ->
`runtime_flag_default_on`.

## How to confirm

- `rg -n "AccessWindow::open" native-builtins/src/lib.rs native-builtins/src/lang_invoke.rs`
  finds the ten sites after 1 and 2.
- `R12Ffm2ArenaFree` (confined path) prints its HotSpot lines in every arm;
  with `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES=1`, `R12FfmSharedCloseKinds`,
  `R12RtSharedArenaClose` and `R12Ffm2ArenaFree` (its shared phase) must still
  print their HotSpot lines in 10 runs per arm, with no crash.

## Round 12 wave 6 (lane ffm3)

### Landed

1. **`native-builtins/src/lib.rs`**: all nine `native_scoped_memory_*` natives
   open an `ffm_fast::AccessWindow` as their first statement (one window over
   both session checks and the copy in `native_scoped_memory_copy_memory`).
2. **`native-builtins/src/lang_invoke.rs`**: `layout_vh_get` holds a window from
   before `layout_vh_access`'s scope check to the read and drops it before the
   boxing allocation; `layout_vh_set` holds one to the write.
3. **Downcall session acquire** (`panama.rs` `pe_downcall_invoke`, switch
   `CRATONVM_FFM_DOWNCALL_ACQUIRE`, default on): every distinct modelled session
   among the segment arguments and the capture-state segment is ACQUIRED as the
   last fallible step before `ffi_call` (`pe_downcall_acquire_sessions`), pinned
   below the call pins, re-read after the call and released
   (`pe_downcall_release_sessions`). A refused acquire releases what it took,
   unpins, and frees the scratch buffers. A close meanwhile -- a shared close on
   another thread or an upcall closing the arena on this thread -- is now
   `IllegalStateException: Session is acquired by N clients`, as on HotSpot.
4. **The acquire count is atomic and races a shared close soundly**
   (`foreign_ffm.rs`, switch `CRATONVM_FFM_ACQUIRE_HANDSHAKE`, default on):
   `p67_session_acquire` holds a window from its state check to a CAS increment
   (`p67_session_add_acquires`); `p67_session_release` CAS-decrements, clamped at
   0; `p67_session_just_close` re-reads the count AFTER the close handshake and,
   if a client got in, reopens the session (`state = 1`) and throws `Session is
   acquired by N clients` (the JDK's `justClose` does the same when its
   handshake fails). The window argument: the acquirer's window either opened
   before the closer's probe (the closer waits for it, so the re-read sees the
   increment) or after it (the acquirer reads the state as closed). This also
   covers `acquire0`/`whileAlive`, which used the same plain read-modify-write.
5. **Arena allocation paths** (`panama.rs`): `pe_arena_allocate_impl` holds a
   window from its liveness check to the block's record, so a racing shared
   close either fails the check or waits until the block is on the close list
   (before, the close could detach the list first and the block was recorded on
   a dead session and never freed). `pe_allocate_from_array` and
   `pe_arena_allocate_from_string` hold one window over the allocation AND the
   element/string writes. `pe_allocate_from_array` also held the source ARRAY
   unpinned across the allocation (stale read after a moving collection), and the
   two scalar `allocateFrom(Arena, ValueLayout, I|J)` closures held the LAYOUT
   unpinned across it: all three are pinned now.
6. Unit tests: `foreign_ffm::r12w6_ffm3_acquire_tests`,
   `panama::r12w6_ffm3_downcall_acquire_tests`.

### Decision: `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` stays default OFF

Read, not measured. Every path THIS page listed now holds a window, but the
list was not complete. These paths still read or write a shared arena's block
after their liveness check with no window, so a free on shared close would be a
use-after-free under a racing close:

1. **The 16 retired `ScopedMemoryAccess` rows run real bytecode in the default
   mode.** `native-api/src/retired_shadow.rs` (`RETIRED_SHADOW_L5_TRIPLES`,
   ~6768-6783) re-tags `copyMemory`, `putInt`, and the `get/put{Short,Int,Long}Unaligned`
   rows plus their `*Internal` twins as `SyntheticStub`, so under `--jdk-only`
   (the launcher default since 2026-09-20) they are dropped and the JDK's
   `if (session != null) session.checkValidStateRaw(); return UNSAFE.x(..)`
   runs: the check is our native (`foreign_ffm` `checkValidStateRaw`), the
   access is `Unsafe`, and nothing holds a window between them. The JIT's
   `jit_scoped_memory_*_direct` helpers (`vm/src/jit/helpers.rs` ~30794) decline
   for a non-null session and dispatch to that same bytecode. These rows carry
   every multi-byte `asByteBuffer()` access (`DirectByteBuffer.getInt(i)` ->
   `getIntUnaligned`) and the buffer bulk `get/put(byte[])` (`copyMemory`).
   Window-holding natives cover them only under `--compatible`.
2. **`ScopedMemoryAccess` methods that were never natives** run bytecode in
   every mode: `get/put{Char,Float,Double}*`, the `Volatile`/`Acquire`/
   `Release`/`Opaque` and CAS/`getAndAdd*`/`getAndSet*` families, `setMemory`,
   `copySwapMemory` (bulk `asCharBuffer`/`asIntBuffer` copies in non-native
   order), `vectorizedMismatch` (`ByteBuffer.mismatch/equals/compareTo`).
3. **`lang_invoke.rs` `segment_vh_get` / `segment_vh_set`** (a real
   `java/lang/invoke/SegmentVarHandle` receiver): no scope check at all and no
   window; see `r12w6-ffm3-segment-varhandle-has-no-scope-check-20260927.md`.

### What is left (exactly)

- Give the bytecode `ScopedMemoryAccess` accessors the handshake HotSpot gives
  them. Two designs, either sufficient:
  (a) **HotSpot's**: `closeScope0` / `p67_session_just_close` (shared) walks
  every other Java thread's top frames and waits (or retries) while one is
  inside a `@Scoped` method (`jdk/internal/misc/ScopedMemoryAccess`'s
  `*Internal` methods carry `@ScopedMemoryAccess.Scoped`) whose session local
  is the closing session. Needs a VM-side `NativeContext` hook
  ("threads with a frame of class X, method-set S, holding reference R"),
  i.e. `vm/src/runtime/` + `native-api` work nobody owned this wave.
  (b) **Entry/exit windows**: the interpreter's invoke path (and the JIT's
  frame setup, or a refusal to inline) opens the thread's `AccessWindow` on
  entry to a method flagged `@Scoped` at link time and closes it on every
  exit (normal, exceptional, deopt). Cheaper to reason about; touches
  `interpreter.rs` and the x64 prologue/epilogue.
- Then 3 (the `SegmentVarHandle` page), then flip
  `foreign_ffm::p67_shared_close_frees` to `runtime_flag_default_on`.
- Confirm with `R12Ffm3SharedClose` (this wave's probe; it drives the
  `asByteBuffer` int path, the layout VarHandle path, and allocate/close races)
  under `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES=1`, 10 runs per arm, no crash
  and HotSpot's lines.

Status stays OPEN for the flip; everything else on this page landed.

## Round 12 wave 7 (lane ffm4)

### Landed

- Item 3 of wave 6's "still no window" list: a real `SegmentVarHandle` now checks the scope
  under an access window (`r12w6-ffm3-segment-varhandle-has-no-scope-check-20260927.md`,
  "Round 12 wave 7").
- `reinterpret(.., Arena, Consumer)` cleanups run from the same close walk, before the block
  record frees, so a cleanup that reads its memory stays safe when the shared free is turned on.

### Decision: `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` still stays default OFF

Re-read against the wave-6 list. What is left is exactly its items 1 and 2: the JDK BYTECODE
`ScopedMemoryAccess` accessors -- the 16 retired rows under `--jdk-only`
(`retired_shadow.rs` `RETIRED_SHADOW_L5_TRIPLES`) and the families that were never natives
(`get/put{Char,Float,Double}*`, the `Volatile`/`Acquire`/`Release`/`Opaque`, CAS and
`getAndAdd`/`getAndSet` families, `setMemory`, `copySwapMemory`, `vectorizedMismatch`). Each
runs `if (session != null) session.checkValidStateRaw(); return UNSAFE.x(..)` inside a
`@Scoped` method with a `finally { Reference.reachabilityFence(session); }`. The check is ours
(`foreign_ffm` `checkValidStateRaw`), the access is `Unsafe`'s, and between them nothing this
lane owns runs, so no window can be held from inside `native-builtins`:

- Opening a window in `checkValidStateRaw` and closing it in `Reference.reachabilityFence` was
  considered and rejected: `checkValidStateRaw` / `checkValidState` are also called with no
  fence after them (`AbstractMemorySegmentImpl.checkValidState`, the bulk copy paths), so a
  window would leak and the next shared close would wait forever; and the JIT treats
  `reachabilityFence` as an empty method it may inline, so the close half would not run.
- What would work is still wave 6's (a) or (b): the closer scanning other threads for a frame of a
  `@Scoped` `ScopedMemoryAccess` method holding the closing session (a `NativeContext` hook over
  the frame walker; `vm/src/runtime` + `native-api`), or entry/exit windows the interpreter and
  the x64 prologue/epilogue open for `@Scoped` methods (`interpreter.rs`, `x64/`). Neither file
  set is this lane's; proposal FFM4-2 in `jit-r12-ffm-proposals.md` sketches (a).

Status stays OPEN for the flip.

## Round 13 wave 1 (lane mhffm)

Re-read; nothing on this page's remaining path is in the lane's files, so nothing landed toward
the flip. Confirmed against the JDK 25 sources (`lib/src.zip`): every `ScopedMemoryAccess` accessor
the page lists is `@Scoped` bytecode of the shape `session.checkValidStateRaw(); UNSAFE.x(..)`
with a `reachabilityFence(session)` in `finally`, and `SharedSession.justClose` relies on
`ScopedMemoryAccess.closeScope(this, ..)`, whose native `closeScope0` handshakes every other thread
and looks for a `@Scoped` frame holding the session. Designs (a) and (b) of wave 6 are still the
only sufficient ones, and both are VM work (`vm/src/runtime` frame walker + a `NativeContext` hook,
or interpreter/x64 entry-exit windows). Proposal MF13-1 in `jit-r13-mhffm-proposals-RETIRED-20260929.md` sketches
(a) against the code as it stands.

Related change this wave: element accesses now check bounds and alignment BEFORE the session
(`CRATONVM_FFM_BOUNDS_BEFORE_SCOPE`, see `r12w7-ffm4-segment-varhandle-residuals-FIXED-20260928.md`).
Every path that changed still holds its access window from before the session check to after the
last touch of the block (the window opens before the bounds check now), so the close-handshake
argument above is unchanged.

Status stays OPEN for the flip.

## Round 13 wave 3 (lane ffm2)

Worked towards proposal MF13-1 (the scoped-frame handshake); the flip is NOT done, because the
machinery it would stand on cannot yet prove the absence of a thread between its check and its
access. What exists, and why it is not enough (all by reading):

1. **The pause exists.** `vm/src/runtime/interpreter/gc_and_alloc.rs` `stw_publish_frame_traces`
   (~341) takes a `NonMovingPause::request_handshake(.., NonCollectionPause::FrameTrace,
   FRAME_TRACE_GRACE)`; every mutator that reaches a poll publishes its stack into
   `JvmThread::frame_trace`, which `ThreadRegistry::frame_trace_of` reads. A closer could take it
   and look for a `jdk/internal/misc/ScopedMemoryAccess.*Internal` frame on every other thread.
2. **Compiled frames are invisible to it.** The published trace is
   `stackwalker::capture_frames_no_lines(&thread.frames)` (`vm/src/runtime/stackwalker.rs`
   ~2670): interpreter `Frame`s only. A `@Scoped` body the JIT compiled, or inlined into a
   compiled caller (`DirectByteBuffer.get` -> `ScopedMemoryAccess.getByte` -> `getByteInternal`),
   has no entry, so a thread stopped at a compiled poll between `checkValidStateRaw` and the
   `Unsafe` access publishes a trace that looks quiescent. A free after that pause is a
   use-after-free.
3. **Frozen peers publish nothing.** After the 2 ms grace the take-over freezes a peer still in
   compiled code; its `frame_trace` is its LAST deposit (gc_and_alloc.rs ~355-365), and the pause
   does not tell its caller which peers were frozen.
4. **Which check runs is dispatch-dependent.** Whether the `@Scoped` path runs our
   `MemorySessionImpl.checkValidStateRaw` native (`foreign_ffm.rs` ~4975, a `Bridge`) or the real
   bytecode depends on `resolve_dispatch` (real class bytes beat a `Bridge`); a cheaper design
   keyed on the native seeing every check (e.g. a per-session "a scoped accessor checked this"
   bit that lets a never-scoped shared arena free) is only as sound as that, so it was not
   landed either.

### What would make the flip sound (exact, by owner)

* **JIT** (`jit/src/lib.rs` compile admission + IR inline planner): while
  `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` is on, never compile and never inline a method of
  `jdk/internal/misc/ScopedMemoryAccess` whose name ends in `Internal` (every `@Scoped` method of
  JDK 25 is such a private `*Internal`, lane mhffm checked `src.zip`). Those bodies then always run
  as interpreter frames and appear in the published trace. Cost: the null-session unaligned
  accessors are already served by the direct helpers and a non-null session already dispatches
  to bytecode (`jit_scoped_memory_*_direct`); what slows is the byte/char/float/double and
  volatile families reached from compiled callers. Measure `R12Ffm3SharedClose` and a
  `ByteBuffer.allocateDirect` get/put loop with and without.
* **GC round** (`gc_and_alloc.rs`): a `stw_publish_frame_traces` variant that also answers
  whether any peer was frozen by the take-over (the `TakenOver` set is in hand in
  `NonMovingPause`).
* **VM + native-api** (`vm/src/vm/vm_exec.rs`, `native-api/src/registry.rs`):
  `NativeContext::scoped_access_quiescent(&mut self) -> Option<bool>` (default `None`): take that
  pause; `None` when no pause was taken or a peer was frozen; `Some(false)` when another thread's
  trace holds a `ScopedMemoryAccess.*Internal` frame (conservatively, whatever its session: the
  trace carries no locals); `Some(true)` otherwise.
* **This lane's files**: `foreign_ffm::p67_session_run_close_actions`, for a SHARED session, frees
  a block record only on `Some(true)`; otherwise it parks the record on a per-VM deferred list
  that the next shared close re-examines (never a leak worse than today's, never a free without
  proof). Then `p67_shared_close_frees` -> `runtime_flag_default_on`.

HotSpot pays a handshake per shared close too; here it is a stop-the-world pause per shared close
(rare: an arena close, not an access). Recorded as proposal FFM2-1 in `jit-r13-ffm2-proposals-RETIRED-20260929.md`.

Status stays OPEN for the flip.

## Round 13 wave 5 (lane ffm3)

### Found and fixed: shared arenas DID free some blocks on close, unsoundly

The page's premise ("with the switch off a shared arena frees nothing") was false for every
allocation that reaches `ArenaImpl.allocateNoInit`: the `SegmentAllocator.allocateFrom(layout,
value)` defaults (`OfByte`..`OfDouble`, `AddressLayout`), `allocateFrom(String, Charset)` and
`allocateFrom(ValueLayout, MemorySegment, ValueLayout, long, long)` all go through the private
`SegmentAllocator.allocateNoInit`, which for an `ArenaImpl` receiver (every arena this model mints)
calls `ArenaImpl.allocateNoInit(JJ)`, not `allocate(JJ)`. Only `allocate` was routed to this VM's
carrier, so these ran the JDK's `SegmentFactories.allocateNativeSegment`, got a real
`NativeMemorySegmentImpl`, and registered a `ResourceCleanup` (`UNSAFE.freeMemory`) through our
`addOrCleanupIfFail`; `foreign_ffm::p67_session_run_close_actions` runs every `cleanup()` whatever
`CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` says. A real segment's accessors are JDK bytecode
(`ScopedMemoryAccess`, no access window), so a shared close racing one was exactly the
use-after-free this switch exists to rule out.

Landed (`native-builtins/src/panama.rs` `register_arena_impl_allocate_no_init`, switch
`CRATONVM_FFM_ARENA_NOINIT_CARRIER`, default on): `ArenaImpl.allocateNoInit(JJ)` and the covariant
`ArenaImpl.allocate(JJ)Ljdk/internal/foreign/NativeMemorySegmentImpl;` are `Bridge`s that answer the
carrier through `pe_arena_allocate` for a modelled arena (anything else runs the JDK body through
`invoke_virtual_bytecode_only`). Those blocks are now recorded and freed for a confined arena and
left alone for a shared one, like `allocate`'s. Unit tests `panama::r13w5_ffm3_arena_noinit_tests`;
probe `C:\craton\jitr13-probes\src\R13Ffm3AllocateFromShapes.java`.

Still freed by a shared close with the switch off, by design of the close walk and outside this
lane's files: a JDK `ResourceCleanup` from any OTHER path that registers one on a modelled session
-- `FileChannel.map(mode, offset, size, arena)` (the unmapper), and whatever real bytecode builds a
`NativeMemorySegmentImpl` through `SegmentFactories` with a session it read from an `ArenaImpl`'s
`session` field. On HotSpot these run after the close handshake. Here a racing bytecode access
reads unmapped memory (a crash, not silent corruption, for a mapping). Fix, if wanted
(`foreign_ffm.rs`, not this lane's): in `p67_session_run_close_actions`, for a SHARED session with
`p67_shared_close_frees()` off, defer a `ResourceCleanup` whose class is not a VM-registered action
to the same deferred list FFM2-1 proposes, instead of running it. That trades a leak for the
race, the same trade this switch already makes.

### Decision on the flip: no sound design fits in this lane's files

Considered and rejected, with the reason:

1. **A per-session "reached Java" taint** (free a shared arena's blocks only if its session was
   never handed to Java code, set at the doors that return it). Not sound from `panama_*.rs`: the
   session escapes through doors this lane does not own -- `ArenaImpl.scope()`,
   `MemorySessionImpl.toMemorySession`, `Buffer.session()` (`foreign_ffm.rs`, `lib.rs`) -- and
   through the REAL `ArenaImpl.session` field, which JDK bytecode reads directly (`allocateNoInit`
   above was one such reader). And the free decision is made in `foreign_ffm`'s close walk, not
   here. Even with all doors covered, the taint write and the closer's read race (store/load
   reordering) unless the taint is written under an `AccessWindow`; that part is easy, the
   coverage is not.
2. **Decommitting instead of freeing** (keep the address range, drop the pages). Memory-safe for
   page-sized blocks only, needs `mmap`/`VirtualAlloc` per block (Linux caps mappings at
   `vm.max_map_count`, 65 530 by default), and a racing read would see zeros where HotSpot throws:
   a silent wrong answer instead of a leak. Not worth it.

What would make the flip sound is unchanged: FFM2-1 (`jit-r13-ffm2-proposals-RETIRED-20260929.md`), i.e. the JIT
never compiling or inlining `ScopedMemoryAccess.*Internal` while the switch is on, a frame-trace
pause that reports frozen peers, a `NativeContext::scoped_access_quiescent()` over it, and a
deferred free list in `foreign_ffm`. None of those files is this lane's.

Status stays OPEN for the flip.

### Hand-back after w4c (same wave)

`R13Ffm3AllocateFromShapes` then failed with `NoSuchMethodError: ArenaImpl.checkValidStateRaw()`
from `AbstractMemorySegmentImpl.copy` -> `ScopedMemoryAccess.copyMemoryInternal`.
`AbstractMemorySegmentImpl.sessionImpl()` is `final` (`return scope;`), so JDK bytecode that casts a
carrier to `AbstractMemorySegmentImpl` runs that body, not the carrier's registered `sessionImpl`
native, and reads `scope`'s slot -- slot 2, where the carrier kept the ARENA. Fixed in
`panama.rs` `pe_carrier_scope` (switch `CRATONVM_FFM_CARRIER_SCOPE_SESSION`, default on): arena
allocations, `ofAddress`'s global-scope segments and upcall-stub segments now keep the arena's
SESSION in slot 2, the shape slices have had since G19-1 and every slot-2 reader already accepts.
Not this lane's: `foreign_ffm.rs` `p67_arena_segment` (~1803) still writes the arena there; change
its `set_field(segment, P67_SEGMENT_ARENA, arena)` to `crate::panama::pe_carrier_scope(ctx, arena)`
if that registration is still reachable. Not located by reading: the NATIVE crash the orchestrator
saw with `CRATONVM_FFM_ARENA_NOINIT_CARRIER=0` and on w3a, i.e. with the real
`NativeMemorySegmentImpl` these shapes used to produce; the default arm no longer creates one on
these paths. Needs the fatal-error dump's native frame to name the reader.

## Round 13 wave 8 (lane ffm4)

Nothing landed toward the flip itself: what would make it sound is still FFM2-1 (the JIT never
compiling or inlining `ScopedMemoryAccess.*Internal` while the switch is on, a frame-trace pause
that reports frozen peers, `NativeContext::scoped_access_quiescent`, a deferred free list), and
none of the first three is in this lane's files. Landed on the close path this page is about
(`native-builtins/src/phases_late/foreign_ffm.rs`; details in
`r12w7-ffm4-reinterpret-cleanup-residuals-CLOSED-20260929.md`, "Round 13 wave 8"):

* **Add vs. close, add vs. add, on a shared session** (`CRATONVM_FFM_SESSION_ADD_ATOMIC`,
  default on): the close-action list is appended by compare-and-swap, and every add holds an
  `ffm_fast::AccessWindow` from its liveness check to the swap. Before, two threads adding to one
  shared arena lost an action, and an add racing a shared close could land on the detached list;
  a lost action is a cleanup or a JDK `ResourceCleanup` (e.g. an unmapper) that never runs.
* **Blocks free after every Java action** (`CRATONVM_FFM_CLOSE_WALK_HOTSPOT`, default on), in the
  JDK's per-kind order. Relevant to the flip: once shared arenas free, a cleanup that reads its
  segment must still find the block, and the JDK's confined `cache` slot would otherwise put the
  first allocation's free first.
* **`addOrCleanupIfFail` cleans up a refused resource** (`CRATONVM_FFM_ADD_OR_CLEANUP_RUNS`,
  default on), as the JDK does: an allocation or mapping that raced a close is freed / unmapped
  instead of leaked.
* `p67_arena_segment` no longer returns with its pin taken when the carrier allocation fails
  (that `Arena.allocate` registration is overwritten later in the same registrar by
  `panama::pe_arena_allocate`, so it is dead in the shipping binary; left in place because the
  registrar gates count it).

### Decision: the wave-5 "defer foreign ResourceCleanups on a shared close" idea is not taken

Wave 5 suggested that, with the switch off, a shared close should park (not run) a JDK
`ResourceCleanup` that is not one of this VM's records, since a racing bytecode
`ScopedMemoryAccess` accessor could touch an unmapped mapping. Rejected: with no deferred list to
revisit (FFM2-1), "defer" means "never run", and the one reachable producer is
`FileChannel.map(.., sharedArena)`'s unmapper. A mapping that is never unmapped keeps the file
open (on Windows it cannot be deleted or truncated) for every program that closes a shared arena
correctly, to protect programs that race their own close, which HotSpot answers with an
`IllegalStateException` in the racing accessor. Revisit together with FFM2-1's deferred list.

### Found, left for the flip (not a defect while the switch is off)

`p67_session_record_block` updates the block record in place (`[0]` count, then the slot) with
plain stores, and replaces a grown record with a plain `set_array_element` on the current list.
For a CONFINED arena only the owner allocates. For a SHARED arena two allocating threads would
lose an id (a leak) or both write one slot. It runs only when `p67_arena_frees_on_close` says the
arena frees, i.e. never for a shared arena today. Before flipping the switch, make the record
update a CAS on the count (`compare_and_swap_field` has no array form; take the session's monitor
around the record update, or keep one record per allocating thread).

Status stays OPEN for the flip.

## Round 13 wave 9 (lane ffm5)

The flip is still not done: what makes it sound is still FFM2-1 (the JIT never compiling or
inlining `ScopedMemoryAccess.*Internal` while the switch is on, a frame-trace pause that reports
frozen peers, `NativeContext::scoped_access_quiescent`, a deferred free list), and none of the
first three is in this lane's files. Landed on the close path (all in
`native-builtins/src/phases_late/foreign_ffm.rs`):

* **Two threads closing one shared arena** (`CRATONVM_FFM_CLOSE_STATE_CAS`, default on).
  `p67_session_just_close` stored the closed state with a plain write after a plain liveness
  check, so two concurrent `close()` calls could both pass, both run the handshake and both walk
  the close-action list (its detach is a plain read then a plain store): every
  `reinterpret(.., arena, cleanup)` cleanup and every JDK `ResourceCleanup` could run twice, and
  once shared arenas free, every block record would be walked twice. The flip is now a
  compare-and-swap open -> closed (`p67_session_flip_closed`); the loser throws `Already closed`,
  as the JDK's `SharedSession.justClose` (`compareAndExchange`) does. Not a flip blocker, but
  the flip would have turned it into a double `free_native_memory` per block (harmless only
  because `NativeMemoryTable::free` ignores an unknown id).
* **The wave-8 "record update" blocker is removed** (`CRATONVM_FFM_SHARED_RECORD_PER_THREAD`,
  default on, read only while `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` is on). A shared
  session's blocks go into per-thread records (`p67_session_record_block_shared`): a `long[]`
  with the allocating thread's tag (`thread_id + 1`) in its last slot, written in place only by
  that thread, and a full record is followed by a new, twice as large one pushed with
  `p67_session_push_action_cas`. No record is ever replaced in place, so two allocating threads
  can neither share a slot nor lose a grown record. The free reads `[1..=count]` only and
  `count` never reaches the tag. Confined sessions, and every session while the shared switch is
  off (the default), keep the single record exactly as before. Unit test
  `foreign_ffm::r13w9_ffm5_lifecycle_tests::shared_blocks_go_into_per_thread_records`.
* `panama::pe_arena_allocate_impl`: a failed segment-carrier allocation (an
  `OutOfMemoryError` after the block was allocated) freed nothing; the block is freed before the
  error returns now (it was on no close list yet).

What is left for the flip: FFM2-1 only (`jit-r13-ffm2-proposals-RETIRED-20260929.md`), unchanged.
Probe: `C:\craton\jitr13-probes\src\R13Ffm5ConcurrentClose.java` (race phase; run it with
`CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES=1` too).

Status stays OPEN for the flip.

## Round 13 wave 11 (lane ffm6)

Nothing landed toward the flip: what makes it sound is still FFM2-1 (`jit-r13-ffm2-proposals-RETIRED-20260929.md`:
the JIT never compiling or inlining `ScopedMemoryAccess.*Internal` while the switch is on, a
frame-trace pause that reports frozen peers, `NativeContext::scoped_access_quiescent`, and a
deferred free list here), and the first three are JIT, GC-round and `vm/` + `native-api`
work. The fourth alone would be dead code (nothing could ever prove quiescence), so it was not
written. Re-read this wave:

* The CAS state flip (wave 9), the per-thread shared records (wave 9), the atomic list append
  (wave 8) and the acquire handshake (wave 6) are all still on the close path, and nothing
  landed since weakens the window argument: `copy_group_return` (new, `panama_upcall.rs`)
  reads a returned segment's block under an `AccessWindow` held from its scope check to the
  copy, so a shared close racing an upcall's struct return waits for it.
* The by-value upcall arguments (`r12w7-upcall-residuals`, "Round 13 wave 11") live in a
  CONFINED arena the VM makes per upcall (`foreign_ffm::p67_new_vm_confined_arena`) and frees
  on its close like any confined arena; they add no shared block and do not depend on the
  flip.
* Found while reading the add path: the close-action list copies itself on every add
  (`r13w11-ffm6-close-action-list-append-is-quadratic-FIXED-20260929.md`). A linked list would also
  make a future shared free's per-thread records cheaper to find (today a record search is a
  scan of the whole list per allocation once shared arenas record blocks).

Status stays OPEN for the flip.

## Round 13 wave 13 (lane ffm7)

Re-read against the code: no piece of the flip fits in the lifecycle files alone. The one
missing ingredient is still FFM2-1's quiescence proof (the JIT never inlining the bytecode
`ScopedMemoryAccess.*Internal` accessors while shared frees are on, a frame-trace pause that
reports frozen peers, `NativeContext::scoped_access_quiescent`), which is JIT, GC-round and
`vm/` work; a deferred free list here without it would be dead code. This wave's
lifecycle changes do not touch shared arenas: `Arena.ofAuto()` close actions now go to the JDK's
`Cleaner` (`r12w7-ffm4-reinterpret-cleanup-residuals`, "Round 13 wave 13"), and an implicit
session records no blocks, as before.

Status stays OPEN for the flip.

## Round 14 wave 1 (lane ffm)

Nothing landed toward the flip; the missing piece is unchanged (FFM2-1: the JIT never compiling or
inlining `ScopedMemoryAccess.*Internal` while `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` is on, a
frame-trace pause that reports frozen peers, `NativeContext::scoped_access_quiescent`, then a
deferred free list in `foreign_ffm`), and its first three parts are JIT, GC-round and `vm/` work.
Re-read against this wave's FFM changes: the new `layout_vh_get` alignment refusal runs after the
read and the window drop, on the pointer VALUE only; the `layout_vh_set` heap-segment refusal runs
inside the window it already held to the write, before the write; and the downcall's
address-return change runs after the call and the session release, touching no arena block. The
window argument above is unchanged.

Status stays OPEN for the flip.

## Round 14 wave 3 (lane ffm)

Nothing landed toward the flip; the missing piece is still FFM2-1's quiescence proof (JIT, GC-round
and `vm/` work, see above). Re-read against this wave's FFM changes: FFM7-1 (`panama.rs`
`auto_arena_record_block` / `reclaim_auto_arena_blocks`, `CRATONVM_FFM_AUTO_ARENA_BLOCK_FREE`)
frees blocks of IMPLICIT (`Arena.ofAuto()`) sessions only, once a collection cleared the weak
handle on the session -- no shared or confined arena's block, and no block reachable through any
segment (every segment holds its session). It runs inside `pe_arena_allocate_impl`'s access window
and takes no close handshake, so the window argument above is unchanged. The address-scope change
(`pe_zero_length_segment` now in the global scope) touches no arena block.

Status stays OPEN for the flip.

## Round 14 wave 4 (lane ffm4)

Nothing landed toward the flip; the missing piece is still FFM2-1's quiescence proof (the JIT never
compiling or inlining `ScopedMemoryAccess.*Internal` while `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES`
is on, a frame-trace pause that reports frozen peers, `NativeContext::scoped_access_quiescent`),
i.e. JIT, GC-round and `vm/` work, and a deferred free list here without it would be dead code.
Re-read against this wave's FFM changes: FFM3W-1 (`panama.rs`, `CratonVM$FfmAutoSessionAction`)
frees blocks of IMPLICIT sessions only, from the `Common-Cleaner` thread after the session's Java
actions ran and its weak handle cleared (no segment of it is reachable, as for FFM7-1), under the
one-sweeper claim; it touches no shared or confined arena. `p67_group_members`' list unwrap and the
field-shaped row deletions touch no block. The window argument above is unchanged.

Status stays OPEN for the flip.

## Round 14 wave 6 (lane ffm6)

Nothing landed toward the flip; the missing piece is still FFM2-1's quiescence proof (JIT,
GC-round and `vm/` work, see above), and a deferred free list here without it would be dead
code. Re-read against this wave's FFM changes: the new layout-handle index refusal
(`lang_invoke.rs` `layout_vh_index_refusal`, for `arrayElementVarHandle` handles and
sequence-element bounds) is pure arithmetic on the coordinates and runs BEFORE
`layout_vh_get` / `layout_vh_set` / `layout_vh_rmw` open their `AccessWindow`, touching no
block; the handles `arrayElementVarHandle` now returns are ordinary layout handles, so every
access through them holds the same window from its scope check to its last touch as
`varHandle()`'s do. The window argument above is unchanged.

Status stays OPEN for the flip.
