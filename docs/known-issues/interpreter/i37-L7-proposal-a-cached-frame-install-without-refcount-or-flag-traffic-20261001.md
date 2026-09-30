# Proposal: a cached frame install with no refcount traffic and one diagnostics gate

**Status: open — filed 2026-10-01 by interpreter round i1 wave 37, lane L7,
from a review of the frame push / pop / recycle paths. Not implemented
(proposals are not, this round). Performance only.**

## Why

After stage 1 of the contiguous stack and wave 33's one-time window
conversion, a warm interpreted call through a fast door rebuilds a retired
slot in a slab window (`FrameStack::push_cached_compact_reusing` →
`Frame::reset_cached_in_window` → `Frame::reset_cached_scalars`) and retires
it on return (`pop_and_recycle_frame` → `FrameStack::retire_top`). What is
left on that path, read from the tree at `54182e717` plus this wave:

| work | where | per | cost |
|---|---|---|---|
| `Arc::clone(cached)` of the inline-cache entry | the doors (`invoke_fast.rs`, `dispatch_virtual.rs`: "own the entry now, which ends the borrow of `thread.invoke_cache`") | call | one locked increment |
| the retired slot's old `inner` dropped by `self.inner = FrameInner::Cached(cached)` | `reset_cached_scalars` | call | one locked decrement |
| `cached.code.clone()` + the old code `Arc`'s drop | `reset_cached_scalars` | call | two locked RMWs -- **removed this wave for a slot rebuilt for the same method** (`c9e785452`) |
| `next_frame_seq()`: a `thread_local!` read and write | every install (the root-snapshot cache is default-on) | call | a TLS access |
| `current_redefine_stamp()`: an `Acquire` load of a global counter | every install | call | a load (a plain `mov` on x86-64) |
| separate latched diagnostic gates: `interp_frames_enabled`, `invoke_phases::on` (inside `now`, `count_frame_kind`), `rootsnap_cache` | every install | call | three to four loads and branches |
| `ValueStack::from_window`'s memset of `max(max_stack, 16) + 8` kind bytes | every windowed install | call | 24+ bytes |
| the caller's `exec_epoch` bump | every return | return | a store (read only by the root-snapshot cache) |

Each is small and none has been timed on its own; none of them is work the
call needs, and every interpreted call through a door pays all of them. The
locked read-modify-writes are the largest: each is a full barrier on x86-64.

## Design

1. **No refcount per call.** When the retired slot at the call's depth
   already holds `FrameInner::Cached(x)` with `Arc::ptr_eq(x, entry)` -- the
   same condition `c9e785452` uses for the code -- the frame needs no new
   reference at all. The doors would hand the install a borrowed
   `&Arc<CachedBytecodeMethod>` (or the entry's raw pointer, valid because
   nothing between the cache read and the push can safepoint, the same
   argument the verbatim argument transfer already rests on) and clone only
   when the slot holds another method. Lane L4's doors must restructure the
   borrow that the clone ends today (`thread.invoke_cache` is borrowed while
   `thread` is passed whole); a split borrow of the two fields, or reading
   `thread.frames` through its own `&mut`, does it.
2. **`seq` from the frame stack.** Replace the thread-local counter with a
   `FrameStack` field bumped at every install: the root-snapshot cache
   compares seqs of ONE stack only (`Frame::seq`'s doc), so per-stack
   uniqueness is all it needs, and the field is in the line the install
   already writes.
3. **One diagnostics word.** Fold `CRATONVM_DBG_INTERP_FRAMES`,
   `CRATONVM_DBG_INVOKE_PHASES` and `CRATONVM_FRAME_TRACE` into one latched
   bit set read once per install and once per return, with the cold work
   behind one branch.
4. **Kind bytes only where a reader can see them.** Every push writes its
   kind byte (`ValueStack::push*`), and every reader stops at `len`; the
   memset exists because a stale `KIND_LONG` above `len` is the unsafe
   direction for any path that "unpops". Audit the unpop paths (`dup*`
   forms, `swap`, the fused arms) and, if none reads a kind above `len`
   without writing it, drop the memset for windows.

## Measure

`InvokeDoorCostBench`, `L7W29ContiguousStackBench`, `L7W28VirtualDoorSplitBench`
(`--nojit`, fat LTO, interleaved, pinned, 5 rounds, two cores), one commit per
item above. Items 1 and 2 are expected to show on every call row; 3 and 4 are
small and should be kept only if they do not move a row up.

## Risks

Item 1 touches the doors' ownership story (a frame must never outlive the
method it names; a slot keeps its `inner` exactly because it still names that
method). Item 4 is the one with a correctness edge: it must be preceded by
the audit, and `CRATONVM_DBG_LONGROOT` / the BouncyCastle collision probes
must run with it.
