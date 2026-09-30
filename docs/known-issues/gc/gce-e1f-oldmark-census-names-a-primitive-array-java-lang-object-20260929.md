# The old-mark census names a primitive array `java/lang/Object`

> **STATUS (2026-09-29, gce e2/f): FIXED IN CODE, awaiting the run.**
> `gc/src/gen_heap_oldmark_census.rs`: `object_name` names a primitive array
> by its element tag (`primitive_array_descriptor`: `[Z [C [F [D [B [S [I
> [J`) and keeps the class-id name for instances and reference arrays; used
> by the `top#`, `unrooted young`, referrer and holder-path lines. The
> Java-visible class was never wrong (`"xyz".toCharArray().getClass()` is
> `[C` on the base); only the header's class id is not an array class.
> Test: `cargo test -j 5 -p cratonvm-gc --lib gce_e2f_a_primitive_array_is_named_by_its_element_tag`.
> Run: `p gen_ngr_census_name 300 "CRATONVM_DBG=oldmark-root-census" "-XX:+UseGenerationalGC -Xmx128m" NativeGrowthReclaimProbe`
> -- any 30 MiB array root prints `[C`, none prints `java/lang/Object` with
> `old_bytes` above 1 MiB.

> **STATUS (2026-09-29, gce e1/x): KEEP -- no fix landed, no run applies.** **Remaining:** the census class-name fix and a census line that prints `[C` for the 30 MiB array.

> **STATUS (2026-09-29, gce e1/f): OPEN -- diagnostics only, found while
> reading a census.** Owner: whoever next touches
> `gc/src/gen_heap_oldmark_census.rs`. Size: XS.

*Filed 2026-09-29 by gce wave e1, lane f.*

## Evidence

`CRATONVM_DBG=oldmark-root-census` on `NativeGrowthReclaimProbe` (base
`adb9178bc`, Windows, Generational) printed

```
top#1 root[3813]=0x1b4808191b8 cat=14: ...jit-precise-movable -> old 0x1b4808191b8 java/lang/Object old_bytes=30199008 ...
```

for what is, by size and by the program, the `char[]` of
`String.toCharArray()` (`15 099 494` chars = `30 198 988` bytes plus the
array header). A `byte[]` elsewhere in the same log prints `[B`, so arrays
can be named; this one is not.

`base_class_name` / the `top#` line read the header's first `u32` as a class
id and pass it to `collector::class_name_for_diagnostics`. For a primitive
array whose header carries no array class id there (the element type lives
in the kind/element tag, `element_type_tag_at`), the id resolves to a plain
class -- here `java/lang/Object`.

## Impact

A reader of the census chases a 30 MiB `Object` that cannot exist; it cost a
detour here. No collector behaviour depends on the name.

## Fix

In `gen_heap_oldmark_census.rs`, name an object whose kind tag says "array"
from its element tag (`array_element_type_from_tag`) as the JVM descriptor
(`[C`, `[B`, `[J`, ... and `[L<elem>;` when a reference array carries its
element class), falling back to the class id only for instances. The census
already imports `object_kind_from_tag` and `array_element_type_from_tag`.

## How to verify

The same census run prints `[C old_bytes=30199008` for that root.
