# Proposal: copy JNI primitive arrays in bulk instead of one boxed element at a time

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 21
> of 54).** Not built: `vm/src/native/jni.rs` `get_array_region!` still calls
> `get_array_element` per element. gcd d5/f's leaf windows made the TRANSITION
> of `Get/Set<T>ArrayRegion` cheap on the flag arm (d7 jnicost_E `int-region`
> 34-36 ns), not the copy. **Gate:** lz4-java 1 MiB round trip (or a test
> native) per backend, flag off vs on, interleaved; ZGC run included (the load
> barrier the raw read bypasses). **Size:** S.

Status: PROPOSAL (gc-common w9-g, 2026-09-24)
Area: `vm/src/native/jni.rs`: `get_array_elements!`, `release_array_elements!`,
`get_array_region!`, `set_array_region!`, `jni_get_primitive_array_critical`,
`jni_release_primitive_array_critical`

## What happens today

Every JNI primitive-array copy walks the array one element at a time through
`VmHeap::get_array_element` / `set_array_element`. Each element costs a
receiver check, a backend dispatch, a bounds and type check, a boxed `Value`
and an enum match. `GetPrimitiveArrayCritical` also re-encodes each element
into bytes (`critical_encode_element`). A 1 MiB `byte[]` handed to a
compression native (lz4-java, zstd-jni, snappy: the `in = Get(src); out =
Get(dst)` idiom) costs about a million accessor calls on Get and a million
on Release, per call. That work is paid on the mutator inside a native call,
where a pause cannot start (the caller is a counted mutator).

The per-element walk was chosen when a G1 humongous array had no flat
payload. That stopped being true: `VmHeap::array_data_ptr` now documents a
contiguous `len * stride` payload for every array on every backend. The VM's
own bulk paths already use it (`write_byte_array_from` /
`read_byte_array_into` in `vm_exec.rs`, `gpu_marshal.rs`, `arraycopy`).

## Proposal

For a primitive array, copy `len * stride` bytes between `array_data_ptr(o)`
and the buffer with `copy_nonoverlapping`. Keep the current walk for:

* a `long[]` write-back, element by element, so the
  `mint_if_smuggled_long_value` chokepoint still sees every `jlong`. Or scan
  the copied buffer for minted candidates first, which is cheaper still;
* any backend whose `array_data_ptr` answers `None`, as the other bulk paths
  already do.

The copy is sound for as long as the caller is a counted mutator: no
collection can start until it arrives, so the payload cannot move under the
copy. That is the same argument `read_byte_array_into` relies on. A
`jboolean` array needs no normalisation on Get. On Release, the JNI spec
lets any non-zero byte stand, and the element walk stores what the native
wrote anyway.

## Why it is a proposal and not a change

It swaps the accessor every backend implements for raw memory access in the
JNI layer. That is the kind of change this round puts behind a
`CRATONVM_*` flag, default OFF, until a run shows it is equivalent: the
three backends on the JNI-library run (`common-w2c-jni-local-refs-are-raw-addresses`)
plus a ZGC run, where the concurrent relocator's load barrier is the one
thing the raw read would bypass. The `vm_exec.rs` byte paths already
bypass it, so a ZGC run of those would answer the same question.

## Measure

A JMH-style loop in a test native, or lz4-java's `LZ4Factory.nativeInstance()`
round-trip on 1 MiB blocks, before and after, per backend. The
`GetPrimitiveArrayCritical` + `ReleasePrimitiveArrayCritical` pair should
drop from O(n) accessor calls to two `memcpy`s.
