# Proposal: guard a pinned private-interface call instead of refusing the method

**Status: proposal — filed 2026-10-04 by interpreter round i1 wave 40, lane
L2, from the closure of
`docs/internal/fixed-bugs/interpreter-L2-compiled-invokeinterface-selection-edges-FIXED-20261004.md`.
Performance only; not built.**

## Where it stands

An `invokeinterface` whose resolved method is a PRIVATE interface method
selects that method (JVMS §6.5 selection step 1), but only for a receiver
that implements the interface; any other receiver is an
`IncompatibleClassChangeError`. javac emits the shape for a default method
that calls its interface's private helper (`java.base` has it in
`SegmentAllocator` and `MatchResult`), with `this` as the receiver; only
hand-assembled bytecode passes a receiver that does not implement it.

Since wave 37 (lane L2), under `--jdk-only`, no compile door pins such a
site, because no compiled route checks the receiver: the method holding it
is not compiled at all. `CpResolvers::invoke` declines and bail-lists it,
`compile_osr_body` denies its loops (`jit_bridge.rs`, reason
`invokeinterface-private-receiver-unchecked`), the eager door seals it, and
`resolve_inline_site_from` refuses the splice. The interpreter raises the
ICCE, and its own cache entry for such a site is never filled
(`private_interface_pin_uncached`, so every call takes the slow path).

So a default method that calls a private helper of its interface stays
interpreted, and its call site is uncached, for the life of the VM. No hot
one is known, but none has been looked for.

## The proposal

Keep the pin, with the receiver check HotSpot's `invokeinterface` makes in
front of it:

1. **Prove it statically where possible.** The receiver is local 0 of an
   instance (default or private) method of the SAME interface and the local
   is never stored to: the verifier then guarantees it implements the
   interface (it is `this`), and the pin needs no guard. This covers every
   javac-emitted instance. A scan of `astore_0` / `astore 0` over the caller's
   bytecode decides it at compile time.
2. **Otherwise, a subtype guard.** A class-id compare against a per-site
   memo of receiver classes known to implement the interface (filled by
   `selection::receiver_does_not_implement` on the miss path), falling to a
   helper that raises the ICCE, in front of the direct bind and on the
   kind-1 dispatch-helper route.
3. **The interpreter's cache entry** gets the same shape: a guarded
   `Bytecode` entry whose hit compares the receiver's class id with the
   filling one.

## What to measure first

`CRATONVM_DBG_JITC=1` over the suite, the jdk-only corpus and a Spring Boot
fat jar, counting `invokespecial-selection REFUSED ...
invokeinterface-private-receiver-unchecked` and `OSR ... refused` lines by
method. If no method with a measurable share of samples appears, record that
and retire this proposal.
