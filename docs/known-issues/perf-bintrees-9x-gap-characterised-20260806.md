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

## If you pick this up

The two levers are structural and both are large:

1. **Make moving-young stop falling back** on multi-method JIT stacks. Unlocks
   the 11% already being paid, and turns the 8g collection from a ~410 ms
   non-moving sweep into a copy of a nursery that is almost entirely garbage.
2. **Shrink the object header.** 32 → 16 would take `Node` from 48 to 32 bytes
   and cut the memory traffic and both zeroing costs by a third.

Neither is a small change, and the per-call shadow-push cost is not removable
without giving up precise roots. A 9.66x row does not have a cheap fix; that is
the finding.
