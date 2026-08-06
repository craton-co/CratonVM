# Binary Trees is 9.66x HotSpot — where the time actually goes

**Characterised 2026-08-06. Not fixed.** This page exists so the next attempt
starts from measurements instead of from the same four hypotheses.

`CratonBench bintrees` (depth 18, ~68M `Node` allocations) is the worst row in
the README table: 1,700 ms against HotSpot's 176 ms.

## It is not the collector

| `-Xmx` | time | collections |
|---|---:|---|
| 8g | 1,686 ms | 1 minor |
| 16g | 1,275 ms | **0** |
| 24g | 1,272 ms | **0** |

With zero collections it is still **7.2x** HotSpot. GC is a 24% tax at the
README's 8g, not the gap.

## Where the time is (perf, `-F 997`, `-Xmx16g`, mapped to instruction offsets)

Sample IPs were mapped back to method offsets by capturing
`CRATONVM_DBG_JIT_DISASM` in the same run and subtracting each method's entry.

| | share |
|---|---:|
| JIT-generated code | **78.0%** |
| kernel (`clear_page_erms`) | 10.6% |
| the VM binary (helpers) | 6.9% |
| libc (`memset`) | 3.7% |

`bottomUpTree` is 47.9% of all samples, `itemCheck` 27.4%.

## The hottest instructions are moving-young publication

The top offsets in `bottomUpTree` are the shadow-stack push before each
recursive call:

```
b32: mov r11,[r10+278h]    <- shadow top
b39: lea r11,[r11+18h]     <- reserve 3 slots        2.7%  (hottest in the run)
b40: cmp r11,[r10+280h]    <- bounds check
b54: mov [rbp-30h],r11
b5c: mov [r11],rax         <- push oop
b63: lea r11,[r11+8]                                 2.2%
```

Two independent measurements agree on the size of this: `CRATONVM_NO_MOVING_YOUNG=1`
is **11%** faster (1,124 vs 1,267 ms, 3 reps each), and the codegen shrinks from
**692 instructions to 498** — the ~194 difference being 23 shadow-top stores, 24
bounds checks, 10 pushes and 15 thread reloads.

**And the workload does not get what it pays for.** The one collection at 8g
reports:

```
[moving-young] fallback #1: reason=innermost-rbp-belongs-to-unguarded-callee
  — this young collection runs the NON-MOVING sweep (no compaction, free-list allocation)
```

So the 11% buys precise relocatable roots, and then the collector declines to
relocate. `chain_entry_rbp_is_foreign` already special-cases direct self-calls
(`returned_from_direct_self_call` matches `E8 rel32` targeting the entry), so
`bottomUpTree`→`bottomUpTree` is fine; what defeats it is `binaryTrees` calling
**two different** JIT methods, leaving the chain entry's `compiled_method`
unable to describe the innermost frame. The real fix is to walk the JIT frame
chain and verify each frame against its own method — which is the most
safety-critical code in the VM, and is why this page stops here.

## The other structural half: objects are 2x

`Node{Node left, Node right}` allocates **48 bytes** — the JIT's inline TLAB
bump is `lea rax,[r11+30h]`, fields at 0x20/0x28. HotSpot's is 24.
`HEADER_SIZE = 32` (`types/src/heap_types.rs`) against HotSpot's 12–16.

That is 2x the memory traffic for the same program, and it is the direct cause
of the 10.6% `clear_page_erms` + 3.7% `memset`: ~3.3 GB of Node bytes get zeroed
by the kernel on fault and again by the TLAB refill. `CompactHeader` exists
(8-byte header) but pairs with 16-byte field slots, so `Node` would be 40 rather
than 24 — it is not the answer here.

## The JIT half: redundant stack traffic in inlined bodies

The single-pass body emits pairs like this around the inlined `Node.<init>`:

```
ca7: mov [rbp-68h],rax
cab: mov rax,[rbp-68h]     <- reload of the value already in rax
caf: mov [rbp-80h],rax
cb3: mov rax,[rbp-68h]     <- and the identical pair again
cb7: mov [rbp-80h],rax
```

There is already a mechanism that removes exactly this — the `slot_mirror`
reload elision in `x64/operand_stack.rs`, default ON. It did not fire because
`try_emit_inline_site` **blanket-suppressed it for the whole duration of an
inlined callee**, on the grounds that the callee's internal joins are invisible
to the position rule (the main loop only invalidates at OUTER-method branch
targets).

The callee's joins are not actually invisible: `try_emit_inline_body` already
computes `callee_branch_targets` for its own merge-point check. Invalidating
the mirror there — the same rule the main loop applies at an outer branch
target — lets the mechanism stay live across inlined bodies.
`perf/inline-slot-mirror-branchless-20260806` does exactly that, and **it is
not worth merging.** Measured, both arms built from the same base:

| | `bottomUpTree` |
|---|---|
| suppression on (dev) | 692 instructions, `len=3874` |
| suppression off | **690 instructions**, `len=3866` |

**Two instructions.** All seven phase checksums identical, so it is correct — it
just does almost nothing, while changing codegen in the path whose earlier
mirror defect made H2 open every database with a null `ACCESS_MODE_DATA` (see
the incident note on the main loop's second invalidation site). Two instructions
does not justify re-entering that. The branch is left unmerged on purpose.

It recovers so little because of the mirror's own rule: it requires the
**immediately preceding emitted instruction** to have touched the same slot
(exact buffer-position equality).

```
ca7: mov [rbp-68h],rax
cab: mov rax,[rbp-68h]     <- elided: -68h is the live mirror
caf: mov [rbp-80h],rax     <- mirror now describes -80h instead
cb3: mov rax,[rbp-68h]     <- NOT elided, and this is the common shape
cb7: mov [rbp-80h],rax
```

Removing the rest needs a real value tracker — "which slot does this register
currently hold", invalidated on writes to the slot, writes to the register,
calls and joins — not a one-entry position-equality mirror. That is a separate
piece of work in which **every emit site that writes a GPR has to be audited**,
which is precisely how the H2 bug happened.

## Where the 690 instructions actually go

```
register spill stores [rbp-2xx]   99
operand shuffling                 84
shadow push / reload              69
calls                             21
```

That is why a peephole cannot close this gap: the body is dominated by
machinery, not by shuffling.

**The largest inline item is the register spill.** The leaf-allocation path —
taken for every leaf `Node`, half of the 68M — runs a full GPR spill into the
reserved spill region *before* the inline TLAB bump:

```
15b: jg 0x3e8                  <- depth > 0 leaves for the recursive path
161: mov [rbp-8],r12           <- register-homed LOCAL flush
165: mov [rbp-260h],rax        <- 14 blind GPR stores, inline, on the fast path
...
1c0: mov [rbp-2C8h],r15
1c7: mov rax,4                 <- safepoint id (bci 4 = the leaf `new`)
1e8: jne 0x275                 <- TLAB-full guard; its slow path is the helper
1ee..270:                      <- inline TLAB bump
```

### Correction: the emitter

This page first attributed that run to `x64/deopt_stubs.rs:1345` and read its
comment ("the guard `JB` **reaches here** with every GPR still holding its
trapping-instant value") as evidence that the emitted layout contradicted its
own design. **That was wrong, and the arithmetic said so.** The deopt stub
spills 16 GPRs *and* 16 XMMs; this run is 14 stores with no `movq`, and
`-0x260 + 13*8 = -0x2C8` lands exactly on the last of fourteen. The emitter is
`x64/safepoint.rs`'s `safepoint_reg_spill_all` loop over the 14-entry
`ALL_SPILL_GPRS` — the SB-CRASH-04 / Keycloak-Gap-9 blind spill, default-on
since 2026-08-03 via `precise_reg_spill_disabled()`. The deopt stub is not on
this path at all. Count the stores before naming the emitter.

### What it costs: 1.178x

`CRATONVM_NO_PRECISE_REG_SPILL=1` removes the blind spill at *every* safepoint —
98 of `bottomUpTree`'s 692 instructions (692 → 594, `len=3874` → `3188`), which
is 7 safepoints x 14. Measured on `-Xmx16g`, 10 interleaved pairs with the order
flipped on alternate pairs, per-**process** user CPU because the host was at
load 25:

| | user CPU per run | median |
|---|---|---:|
| default | 2.20 2.18 2.18 2.18 2.23 2.08 2.12 2.20 2.13 2.12 | 2.18s |
| `NO_PRECISE_REG_SPILL=1` | 1.79 1.82 1.84 1.74 1.87 1.87 1.86 1.83 1.88 1.86 | **1.85s** |

**1.178x, ranges disjoint.** That is the ceiling for any work on this spill.

### What can be claimed from it, and what cannot

Not all of it. The spill exists so the conservative `[scanner_sp, entry_sp)`
walk can see an oop that lives only in a register when the collector stops the
world, and a collector only stops a thread at a safepoint. So the question at
each safepoint is: *can a collection actually be reached from here?*

* **Self-call safepoints — yes.** `bottomUpTree` and `itemCheck` are directly
  self-recursive, and a call collects. `can_elide_self_call_register_spill`
  exists for exactly this shape and correctly refuses both: `itemCheck(Node n)`
  has a reference local in a register, and `bottomUpTree`'s recursive site has
  the half-built `Node` live on the operand stack, so
  `collect_live_oop_homes()` is non-empty and moving-young needs the precise
  publication. Roughly two thirds of the executed spills are these, and they
  stay. (This also retires the "`CRATONVM_JIT_MY_SELFCALL_PROOF=0` is inert"
  observation below: the proof was never firing, and it should not.)
* **`new` safepoints — no.** Under `skip_post_init_helper` the inline-TLAB arm
  emits **no call at all** between the safepoint and the merge point: layout
  guard, cached-thread load, cursor bump, header stores, cursor commit, jump.
  Nothing there can collect, so all 14 stores are dead on the path that
  actually allocates. They are live only on the three slow-path edges, which
  converge on `new_object`.

`perf/alloc-spill-sink-20260806` sinks them: the three registers the fast path
clobbers (RAX, R10, R11) stay at the safepoint, the other eleven move to the
slow-path label, where their value is still their safepoint value precisely
because the fast path does not touch them. Same fourteen slots, same layout.
A partition test pins `ALLOC_FAST_PATH_CLOBBERS` and its complement against
`ALL_SPILL_GPRS`, because a gap there is not a slow benchmark but a live oop
the root scan never sees.

It also drops the inline `get_current_thread` fallback — a CALL clobbers the
whole caller-saved file, which would invalidate the eleven registers spilled
later, and `emit_prologue` writes that slot on every entry anyway, so a null
read means a genuinely non-Java thread and the fallback would have diverted
too.

## Things that were tried and are NOT the answer

- **Tiering.** `bottomUpTree` never reaches the optimizing tier at the default
  threshold. Giving it one (`CRATONVM_TIER_C2_THRESHOLD=1000`) changes the time
  by 1 ms (1,266 vs 1,267). `CRATONVM_JIT_FORCE_C2=1` is **25x slower**
  (31,630 ms). More C2 is not a direction here.
- **`CRATONVM_JIT_MY_SELFCALL_PROOF=0`** — inert. Same 43 spill stores, same
  `len=3874`.
- **`CRATONVM_JIT_MY_SHADOW_EMISSION=0`** — 1%, and it was separately proven
  inert (byte-identical codegen) during the PERF-02 work. Do not read that 1%
  as a measurement of anything.
- **The duplicated TLAB memset in `g1.rs`** (fixed in `2386e8bd8`): real
  duplication, but **G1 is not the default backend** — `CRATONVM_GC_STATS` says
  `backend=generational`. The memset share did not move (1.80% → 2.21%).

## A third JIT item, not yet taken

The prologue fetches the thread pointer **twice** on an external entry and
still calls the helper once on a self-entry:

```
40: call rax    <- get_current_thread, for the SHADOW thread slot
...
84: mov r10,[rbp]              <- self-entry proven: inherit from caller frame
8b: mov rax,[r10-28h]          <- jit_thread_slot
92: mov [rbp-28h],rax
96: mov rax,[r10-18h]          <- stack_floor_slot
```

`emit_prologue`'s self-cache-inherit block copies `jit_thread_slot_off` and
`stack_floor_slot_off` out of the caller's same-layout frame when a direct
self-call is proven — but the shadow-stack fetch above it is unconditional and
wants the *same* `*mut JvmThread`. On a self-recursive method that publishes
(so the fetch is not NOP'd out), that CALL runs on every invocation —
68M times in `bottomUpTree`.

Not done here because the fetch's byte range is what
`maybe_nop_out_shadow_fetch` erases, so splitting it into inherited and fetched
paths means teaching the erase about both. Worth doing; it is prologue surgery,
not a peephole.

## If you pick this up

The two structural levers are large:

1. **Make moving-young stop falling back** on multi-method JIT stacks. Unlocks
   the 11% already being paid, and turns the 8g collection from a ~410 ms
   non-moving sweep into a copy of a nursery that is almost entirely garbage.
2. **Shrink the object header.** 32 → 16 would take `Node` from 48 to 32 bytes
   and cut the memory traffic and both zeroing costs by a third.

Neither is a small change, and the per-call shadow-push cost is not removable
without giving up precise roots. A 9.66x row does not have a cheap fix; that is
the finding.
