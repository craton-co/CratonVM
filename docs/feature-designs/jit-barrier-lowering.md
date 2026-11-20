# JIT garbage-collection barriers

Compiled reference accesses must use the active heap's barrier and publication
contract. The implementation spans `jit/src/x64/`, `jit/src/ir_lower.rs`,
VM JIT helpers, and collector code under `gc/src/`.

## Writes and safepoints

Generational and G1 reference writes maintain remembered sets/card tables.
Concurrent marking requires SATB treatment of overwritten references. Barrier
elision is valid only while the compiler's null, allocation, alias, and memory
proofs hold; a primitive store is not interchangeable with a reference store.

Safepoint polls publish the stack-map and frame state required by the collector.
Loop transformations, OSR, helpers, and uncommon exits must retain bounded poll
progress and complete root reconstruction. See [safepoint polls](../architecture/jit-safepoint-polls.md)
and [precise maps](precise-jit-maps-default.md).

## ZGC reads

The production concurrent-marking profile is enabled by default and uses SATB.
Concurrent relocation and the colored-reference barrier profile are separately
opt-in. Barrier-aware helper routing is required whenever inline reference
access cannot honor the active representation. See [ZGC load barriers](zgc-jit-load-barrier.md)
and [reference slots](zgc-reference-slot-representation.md).
