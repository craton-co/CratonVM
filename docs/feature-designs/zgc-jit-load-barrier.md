# ZGC load barriers in compiled code

The JIT supports the active heap's read barrier. The capability check
`zgc_codegen_honours_read_barrier()` and the inline-field restriction
`zgc_read_barrier_blocks_inline_fields()` are in `jit/src/x64/licm.rs`.
`vm/src/vm/vm_init.rs` uses the capability when deciding whether relocation is safe.

When the barrier is armed, reference access paths must decode/remap the loaded
word using the owning heap. Compact-field reads and writes can route through
`jit_getfield` and `jit_putfield_object`; bypassing this route requires an
inline sequence with equivalent null, color, forwarding, and root semantics.
A colored heap offset is not a machine address and must not be rejected as an
implausible raw pointer or silently converted to null.

Concurrent relocation and its colored-barrier profile are opt-in through
`CRATONVM_ZGC_CONCURRENT_RELOCATE=1`. Default concurrent marking uses SATB and
must not be confused with arming that relocation profile.

Correctness covers fields, statics, arrays, captures, atomic operations, JNI
handles, and deoptimization/OSR state. Safepoints must preserve references in
the representation the collector expects. Unsupported compiled paths must
refuse relocation or use a barrier-aware helper.

See [reference slots](zgc-reference-slot-representation.md),
[barrier lowering](jit-barrier-lowering.md), and [ZGC architecture](zgc-architecture.md).
