# Fix: vm-core-perf — bulk byte[] decode in `read_java_string`

**Report finding:** vm-core.md §Performance P1 — `read_java_string` decodes
`byte[]` strings element-by-element.

## Finding

`decode_java_string_value_array` (`vm/src/vm/vm_object.rs`), the decode helper
reached by every `read_java_string` call, used a bulk read only for the legacy
`char[]` path (`heap.read_char_array_bulk`). The JDK 9+ compact `byte[]` paths
(LATIN1 and UTF16) looped `heap.get_array_element(value_array, i)` once per
byte — a virtual heap-backend dispatch + `Value` box + `Result`/enum match for
every single byte of every string. `read_java_string` is among the hottest VM
functions (map-key hashing, `toString`, equality probes), so this is O(n)
fixed overhead per string read across the whole interpreter.

## Root cause

No bulk byte accessor was used on the `ArrayElementType::Byte` arm. The heap
already exposes the primitive needed: `VmHeap::array_data_ptr(obj) ->
Option<*mut u8>` returns the contiguous 1-byte-per-element payload pointer for
ordinary arrays (and `None` for a G1 *humongous* array, whose payload is split
across non-contiguous regions). `vm_exec.rs` (`write_byte_array_from`,
`read_byte_array_into`) and `gpu_marshal.rs` already use this exact
`Some(base)` memcpy / `None` per-element-fallback pattern for `byte[]`.

## Exact change

`vm/src/vm/vm_object.rs` only:

1. Added a file-local helper `read_byte_array_bulk(heap: &VmHeap, obj:
   ObjectRef) -> Vec<u8>`, mirroring the existing `VmHeap::read_char_array_bulk`
   structure: `array_data_ptr` `Some(base)` → single
   `ptr::copy_nonoverlapping` of `len` bytes; `None` (G1 humongous) →
   region-safe per-element fallback via `get_array_element`. Early-returns the
   empty `Vec` for `len == 0`.
2. Rewrote the `ArrayElementType::Byte` arm of
   `decode_java_string_value_array` to read the payload once via the helper,
   then decode from the contiguous `&[u8]`:
   - LATIN1: `for &b in &bytes { s.push(b as char) }` (bytes are already
     8-bit, identical to the old `(v & 0xFF) as u8 as char`).
   - UTF16: `bytes.chunks_exact(2).map(|c| u16::from(c[0]) | (u16::from(c[1])
     << 8))` — little-endian (low byte at even index), identical to the old
     `(hi << 8) | lo`. `chunks_exact(2)` drops a trailing odd byte exactly as
     the old `num_units = len / 2` loop did.

Output bytes/chars, endianness, empty/odd-length handling, and the
`From::from_utf16_lossy` final step are byte-for-byte preserved. No public API
changed; no file outside the owned one touched (the helper is local, built on
the already-public `array_data_ptr`/`array_length` on `VmHeap`).

## Files touched

- `vm/src/vm/vm_object.rs`

## Tests added

- `decode_value_array_byte_paths_bulk` (in the existing `#[cfg(test)] mod
  tests`) — drives `decode_java_string_value_array` with directly-built
  `byte[]` payloads: LATIN1 incl. U+00FF, UTF16 little-endian (U+0100), empty
  array, and an **odd-length** UTF16 buffer to pin the trailing-byte-drop edge
  (which the existing round-trip tests can't produce since they always
  allocate even-length buffers). Helper `make_byte_array` added alongside.
- The pre-existing `compact_string_{latin1,utf16,empty,latin1_boundary,
  beyond_latin1}_roundtrip` tests already exercise the new bulk path end-to-end
  through `read_java_string`; they continue to pin behavior.

## Follow-up & risk

- **Risk: low.** Pure perf rewrite of a hot inner loop with identical output.
  The unsafe `copy_nonoverlapping` is guarded by the same `array_data_ptr`
  `Some`/`None` contract already used in `vm_exec.rs`/`gpu_marshal.rs`; the
  G1-humongous case correctly falls back to the region-safe per-element read,
  so there is no OOB hazard for large strings. The read is synchronous with no
  intervening allocation (no GC move under the copy), matching the convention
  `read_char_array_bulk` already relies on.
- **Follow-up (out of scope, owner = gc crate):** the report's Feature
  Suggestion #3 proposes promoting this to a first-class
  `VmHeap::read_byte_array_bulk` in `gc/src/vm_heap.rs` (next to
  `read_char_array_bulk`) so other call sites can reuse it. Left as a local
  helper here because `gc/src/vm_heap.rs` is outside this agent's owned files.
