# Proposal: every raw root write names its owner, and the writer proves it is that owner before it stores

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 26
> of 54).** Not built (no `write_raw_root`; `newest_claimants_by_os_tid` still
> routes captures). The netty suites are clean on d7 (`netty-d7-off`), so this
> is hardening, not a pending crash. **Gate:** audit-only mode first: netty /
> Tomcat / VthreadProbe with zero would-refuse lines, then the check on.
> **Size:** M.

*Filed 2026-09-26 by gen round 5 wave 1, lane `crash5`. A direction, not a
defect.*

## The problem this generalises

Five places in the VM store a relocated address into a word named only by its
address (the table in the "gen r5w1/crash5 analysis" section of
`../../internal/gc/generational-bytebuf-suite-sigsegv-hashbrown-rehash-FIXED-20260928.md`): the
blocked-peer native-stack write-back, three JIT-frame walks, and the shadow
stack's indirect homes. Each was written with its own local argument for why
the word still belongs to the frame it was captured from ("this runs on the
owning thread", "the chain entry is fresh", "the band is the owner's"), and
the arguments were checked in exactly one place: the shadow stack, after it
had already produced a wild store (gc-common w5/w6). This wave found the
native-stack write-back's argument wrong (captures routed by a shared OS tid),
and the JIT walks' argument is unverified (a stale `exact_rbp` is not
detected). The audit landed this wave (`CRATONVM_DBG_ROOT_WRITE_AUDIT`) shows
where each store lands; it does not stop one.

## The proposal

One primitive in `cratonvm_gc` that every raw write-back goes through:

```rust
/// A raw root slot as captured: where, by whom, and what it held.
pub struct RawRootSlot { pub addr: usize, pub owner: StackOwner, pub orig: usize }
/// The stack a slot was captured from: the OS thread and that thread's
/// stack band AT CAPTURE, plus a per-thread generation bumped on every
/// virtual-thread mount/unmount and at thread start.
pub struct StackOwner { pub os_tid: u32, pub band: SlotBand, pub generation: u64 }

/// Store `new` into `slot` iff the CALLING thread is `slot.owner` (same OS
/// tid, same generation), the word lies in the caller's live band
/// `[sp, top)`, and it still reads `slot.orig`. Audited under
/// `CRATONVM_DBG_ROOT_WRITE_AUDIT`; refusals counted by kind.
pub fn write_raw_root(kind: RootWriteKind, slot: &RawRootSlot, new: usize) -> bool;
```

- The capture side (the helper-window classifier, the JIT walks) records a
  `StackOwner` instead of a bare tid; the fold routes by owner, not by OS tid,
  which makes the newest-claimant map this wave added unnecessary.
- The generation closes the cases no address check can: a continuation that
  unmounted and remounted on the SAME carrier (same tid, same band, different
  frames), and an OS thread whose cached stack glibc handed to a new thread.
- The JIT walks pass the frame's `(rbp, frame_size)` and get the
  `rbp - frame_size >= scanner_sp` check for free.

## Cost

One thread-local read (the generation) and two compares per stored word; the
stores are per relocated root, not per object, and happen once per wake or
per pause. The capture record grows from `(tid, addr, value)` to about 40
bytes.

## What it would retire

The newest-claimant routing (`newest_claimants_by_os_tid`), the bare
own-stack bound in `apply_native_slot_fixups`, the three per-site bounds
requested in
`../../internal/gc/gengc-r5w1-crash5-jit-frame-remap-writes-are-not-bounded-by-the-own-stack-FIXED-20260927.md`,
and the class of "whose word is this" reasoning each future write-back would
otherwise re-derive.

## How to evaluate

Land it behind the audit first: route every raw write-back through
`write_raw_root` in AUDIT-ONLY mode (store exactly as today, log what the
owner check would have refused). A netty / Tomcat / VthreadProbe run with zero
would-refuse lines is the evidence to switch the check on.
