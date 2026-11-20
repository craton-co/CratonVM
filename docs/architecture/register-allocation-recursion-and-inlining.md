# Register allocation, recursion, and selective inlining

The x64 single-pass backend now enables graph-coloured callee-saved GPR homes
by default whenever precise JIT maps are active. Liveness and interference are
computed over the complete bytecode CFG, including loop backedges and invoke
sites. A local therefore keeps one stable home across loops and calls.

The call boundary is also the publication boundary for register locals:

1. Before a GC-capable call, each register local is copied to its canonical
   frame slot and the current precise-map PC is published.
2. The oop map names the canonical slots while the VM or collector runs.
3. After a moving collection, oop-valued register locals are reloaded.
4. OSR entry uses per-block live-in data and a dead-local mask so coalesced
   registers are initialized only from their live owner.

`CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=0` remains a diagnostic opt-out.
The allocator never enables without precise maps.

Recursive calls retain two independent guards. Mutual-recursion compile cycles
always route through checked dispatch. A direct non-tail self-call is admitted
only after exact method-identity proof and emits a native-stack-floor check
before entering the same artifact. Tail self-calls reuse the current frame.

Inlining is selective at two levels. The VM resolver admits only small,
exception-free, synchronized-free leaf bodies with supported bytecodes and
valid class-initialization state. The JIT now revalidates the 35-byte hard
limit, estimates backend expansion (including field and context costs), rejects
sites above the per-site expansion ceiling, and charges that estimate against
the per-compilation budget. This keeps inline choice bounded even if a future
resolver is less conservative.

Validation includes allocator/JIT unit tests plus a differential probe whose
integer locals remain live around loop backedges and virtual calls, direct
self-recursion, mutual recursion, and a small inline leaf. Its checksum must
match HotSpot in CratonVM JIT and `--nojit` modes.
