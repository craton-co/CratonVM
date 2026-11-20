# ZGC reference-slot representation

Reference representation is a heap contract. Code must use the owning heap's
accessors rather than assuming a reference word is always a raw pointer.

The optional concurrent relocation stack uses colored heap offsets, forwarding,
and remapping. Decoding must preserve null: masking a zero word and adding the
heap base would manufacture a non-null reference. Slot updates, including
atomics and self-healing reads, must preserve the current color and avoid
publishing a stale pre-relocation address.

Compact instance reference fields use the configured reference-word width. Tagged values,
reference arrays, statics, JNI handles, and compiled roots have distinct storage
and publication paths; a change to one representation does not authorize raw
casts through the others. A compare-only operation must compare object identity
in the representation appropriate to the active heap.

## Storage inventory

| Storage | Representation | Authority |
|---|---|---|
| Compact instance reference field | An 8-byte reference word, or a 4-byte encoded reference when narrow oops are enabled. | `types/src/field_layout.rs`, `types/src/narrow_oop.rs::ref_field_size` |
| Reference array element | The same selected 4/8-byte reference width; no `Value` tag per element. | `types/src/heap_types.rs::element_byte_size`, `narrow_oop::ref_element_size` |
| Legacy instance field | A 16-byte tagged `Value` cell; the reference is its payload, not the whole cell. | `types/src/value.rs`, `types/src/heap_types.rs` |
| Static field | A `Value` in class metadata, outside the heap instance layout. | `classloading/src/class.rs` |
| Interpreter locals and stack | Typed `Value` entries, including the `Object` discriminant. | `types/src/value.rs` |
| Compiled roots and JNI handles | Locations described by stack maps or the owning handle/root protocol. | [Precise maps](precise-jit-maps-default.md), [native handles](native-handle-discipline.md) |

`Value` is not the 8-byte NaN-boxed `CompactValue`; those types cannot be
substituted for one another. Use registered class offsets and the configured
reference width instead of assuming a uniform field stride. Narrow-oop
base/shift configuration is fixed before allocations and layout registration.
Colored reference words also require the owning heap's decode/remap path.

Self-healing concurrent reads must update only the reference word, using the
exact word observed as the compare-exchange comparand. A concurrent write must
not be overwritten by healing an older observation. For a tagged legacy cell,
that operation must preserve the tag and padding as well as the new reference.

The implementation lives in `gc/src/zgc/`, `gc/src/heap.rs`, and the VM's
barrier-aware field and array helpers. See [JIT load barriers](zgc-jit-load-barrier.md),
[compact reference fields](compact-ref-field-layout.md), and
[reference publication](../architecture/inline-allocation-and-reference-publication.md).
