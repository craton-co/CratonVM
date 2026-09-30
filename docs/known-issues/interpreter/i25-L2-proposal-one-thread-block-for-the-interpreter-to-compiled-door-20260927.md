# Proposal: one per-thread block for the interpreter→compiled door's bookkeeping

**Status: open (proposal) — filed 2026-09-27 by interpreter round i1 wave 25,
lane L2. Not measured (no build in the lane); the first stage is the
measurement.**

## Problem, with evidence

Every call from an interpreted frame into a compiled body goes through
`jit_bridge::run_jit_body_raw` (the three stack/decoded/one-shot doors and the
first-call door in `interpreter.rs`). Read from the code, one door call does
this thread-local work, each a separate TLS lookup (on Windows a `gs:`-relative
load through the TLS index; on Linux a `fs:` load, plus the lazy-destructor
state check for a `thread_local!` whose type needs `Drop`):

| Step | Thread-local | File |
|---|---|---|
| retire a stale deopt flag | `JIT_SIGNALS.deopt` (take) | `vm/src/jit/helpers.rs` `take_jit_deopt_pending` |
| exceptional-stash floor (wave 25; only for a body with a bakeable point) | `LAST_EXCEPTIONAL` (borrow, len) | `jit/src/deopt.rs` `exceptional_stash_depth` |
| install the JIT thread | `JIT_THREAD` (get + set), `LINUX_JIT_THREAD_MIRROR` / `gs:[disp]` | `helpers.rs` `set_jit_thread`, `jit/src/x64/licm.rs` `publish_jit_thread_mirror` |
| push the frame-chain entry | `JIT_ENTRY_CHAIN` (borrow_mut, push), the scan-cache boundary note, the top-RBP cache | `vm/src/jit/conservative_roots.rs` `push_entry_full` |
| mark the class active | the per-thread activation state and a slot scan | `types/src/jit_activation.rs` `enter` |
| suspend native unwind | `UNWIND_OK` (get, maybe set) | `vm/src/runtime/native_oom.rs` `suspend_for_jit` |
| (exit) pop the chain, deactivate, restore | the same three again | `JitEntryGuard::drop` |
| restore the JIT thread | `JIT_THREAD` (get? + set), the mirror | `restore_jit_thread` |
| drain every signal | `JIT_SIGNALS` (7 cells, one `RefCell`) | `take_all_jit_signals` |

That is 12–15 TLS round trips and three `RefCell` borrow-flag checks per call,
around a compiled body that is often a handful of instructions (a leaf
getter, `InvokeDoorCostBench`'s `static-call`). HotSpot's i2c adapter
touches the thread once (the `JavaThread*` is in a register) and records the
last Java frame with two stores.

## Design

One `#[repr(C)]` per-thread block owned by `JvmThread` (not a new process
global and not a new `static` in `jit/src`, whose ratchet counts
thread-locals), reachable from the `&mut JvmThread` the door already holds:

* the JIT signal cells (today `JIT_SIGNALS`), the deopt flag included;
* the frame-chain vector and its top-RBP cache (today `JIT_ENTRY_CHAIN`);
* the unwind-permission depth (today `UNWIND_OK`);
* a cached "exceptional stash depth" word the jit crate's stash updates on
  push/pop through a pointer the VM installs at `set_jit_thread` (the stash
  stays in `jit/src/deopt.rs`; only its length is mirrored).

`JIT_THREAD` stays the one TLS the compiled code and the extern-C helpers
use to FIND the block (they have no `&mut JvmThread`); every Rust-side door
step then reads fields of `thread` instead of doing its own lookup. The
helpers that set signals (`set_jit_pending_npe` & co.) go through
`JIT_THREAD` once, as they already do for the pending exception
(`thread.jit_pending_exception`).

## Expected win and how to measure it

Stage 0 (measure, no code): a `perf record -g` of
`tools/probes/interp/L2/L2W25DoorSentinelBench.java` `leaf` and
`tools/probes/interp/L4/InvokeDoorCostBench.java` `static-call` under
`CRATONVM_JIT_OSR=0` on the Linux host (an FP build, as the throwable-cost
work used): the share of `run_jit_body_raw` + `JitEntryGuard` + the three TLS
helpers in the per-call cycles. Proceed only if it is ≥ 15% of the door.
Expected, if it is: 10–20 ns per door call off a ~50–90 ns interpreted→
compiled call, visible on every workload whose interpreted code calls small
compiled methods (the classic tier-1 caller / tier-4 leaf mix).

## Cost and risk

Medium-high: four crates' thread-locals move (`vm/src/jit/helpers.rs` and
`conservative_roots.rs` are lane L6's and the GC's; `types/src/jit_activation.rs`
is shared), and the GC root walk reads the chain from other threads at a
pause (`xt_root_scan`), which today finds it through the thread-local's
address — the block's address must be published the same way. A re-entrant
door (compiled → helper → interpreter → door) must save and restore exactly
what the TLS scopes save today (`JitThreadScope`, the chain depth). The
wave-3 lesson applies: fold one thread-local per stage and A/B each.

## Staged plan

1. Stage 0 above; stop if the share is small.
2. Move `JIT_SIGNALS` into `JvmThread` (the drain and the door's entry retire
   already hold `thread`; the setters reach it through `JIT_THREAD`). A/B.
3. Mirror the exceptional-stash length (the wave-25 floor read) into the
   block. A/B.
4. The frame chain and `UNWIND_OK`, with the cross-thread publication for
   the root walk. A/B on the GC stress suite as well as the benches.
