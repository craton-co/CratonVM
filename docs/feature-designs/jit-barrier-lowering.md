# JIT GC Barrier Lowering — Card Marking, SATB, and Load Barriers

**Status:** Shipped (default on) for write barriers and safepoints; Partial/Designed for
concurrent load barriers.

## What it does today

CratonVM's JIT compiler emits explicit garbage collection barriers during code generation
to coordinate mutator execution with the active GC subsystem (`gc/src/card_table.rs`,
`gc/src/concurrent_mark.rs`, `jit/src/x64/codegen.rs`). The barrier lowering strategy adapts
based on the active garbage collector (Generational, G1, or ZGC).

### 1. Post-Write Card Marking (Generational and G1 Collectors)

For reference writes into heap objects and arrays (`aastore`, `putfield`, `putstatic`), the JIT
emits a post-write card mark barrier sequence:

1. **Null Check Filtering:** If the written value is known to be null (or provably points within
   the young generation for generational collectors), the card mark can be omitted.
2. **Card Calculation:** The target heap address is shifted right by `CARD_SHIFT` (9 bits,
   representing 512-byte card granules).
3. **Card Table Store:** The calculated card index is added to the base address of the GC card
   table, and a dirty byte sentinel (`DIRTY_CARD = 0x01`) is stored into the card entry:
   ```asm
   mov  [card_table_base + (dest_addr >> 9)], 0x01
   ```
4. **Card Consolidation:** In loops updating consecutive array elements, card marks are hoisted
   or batched when indices remain within the same card boundary.

### 2. Pre-Write SATB Barriers (G1 and Concurrent Generational Marking)

When concurrent marking is enabled, Snapshot-At-The-Beginning (SATB) invariants are maintained
to ensure all objects live at the start of marking are preserved:

1. **Active Marking Gate:** A thread-local or global flag indicates whether concurrent marking
   is active. If inactive, the barrier executes a single fast branch around the SATB sequence.
2. **Old Value Peeking:** The prior reference residing in the field/array slot is loaded before
   the store overwrites it.
3. **Buffer Insertion:** If the old value is non-null and not already marked, it is appended to
   the executing thread's local SATB buffer (`satb_mark_queue`).
4. **Slow Path Overflow:** If the thread-local buffer is full, the mutator calls a runtime stub
   to enqueue the buffer for the concurrent mark worker threads and allocate a fresh buffer.

### 3. Safepoint and Handshake Polling

Mutator threads must periodically reach safe points for GC pauses, deoptimization, and JVMTI
event coordination:

1. **Poll Insertion Sites:** Safepoint polls are placed at:
   - Method function returns (preventing unbounded execution in leaf recursion).
   - Loop back-edges (ensuring loops cannot delay safepoints indefinitely).
2. **Polling Mechanism:** Fast memory read from a designated thread-local polling address or
   safepoint page. When a safepoint is requested, the VM removes read permissions or updates
   the thread state, causing the poll instruction to trap or jump to the safepoint handler.

### 4. ZGC Load Barrier Lowering (Design & Staged Implementation)

Unlike write-barrier collectors, ZGC relies on read/load barriers to enable concurrent relocation
and colored pointer healing:

1. **Colored Pointer Test:** When loading a reference from a heap object field or array element,
   the pointer's metadata bits are tested against the current `ZAddressBadMask`:
   ```asm
   test  reg_ref, [rel zaddress_bad_mask]
   jnz   .L_zgc_slow_path
   ```
2. **Fast Path:** If none of the bad bits are set (the pointer is properly marked and remapped),
   execution proceeds immediately with zero overhead.
3. **Slow Path Stub:** If bad bits are set, execution jumps to an out-of-line runtime stub:
   - Preserves active scratch and argument registers.
   - Invokes the ZGC runtime load barrier helper to locate the forwarding entry, evacuate
     the object if needed, and compute the remapped address.
   - Atomically updates the heap reference slot with the healed pointer (self-healing).
   - Restores registers and resumes compiled code with the healed address.

## Configuration and Flags

| Flag | Description |
|---|---|
| `CRATONVM_NO_CARD_MARK` | Disables JIT card mark emission (testing/debugging only) |
| `CRATONVM_SAFEPOINT_POLL` | Configures polling mechanism (thread-local word vs page trap) |
| `CRATONVM_ZGC_JIT_BARRIER` | Opt-in experimental switch for JIT-emitted ZGC load barriers |
