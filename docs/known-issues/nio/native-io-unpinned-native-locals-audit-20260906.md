# `native-io` unpinned-native-local audit — the family is pervasive, 5 fixed, ~104 tracked

| | |
|---|---|
| **Status** | OPEN as a population. The tool, the classification and a first tranche of fixes are landed; the remainder is baselined so it cannot grow silently. |
| **Scope** | `native-io/src` — the crate the `unpinned-native-locals-audit-48-fixed-20260825` pass never covered. |
| **Tool** | `scripts/unpinned-native-local-audit.py` |
| **Started from** | `internal/fixed-bugs/filechannelimpl-construction-window-was-never-rooted-FIXED-20260906.md`, one instance found by chasing a reclaimed `FileChannelImpl` |

## What the audit found

**114 candidates in `native-io/src`** — 48 of the local shape, 66 of the
parameter shape. Every local candidate was read. Roughly **35 to 40 of the 48
are real**, and they are all one idiom:

```rust
let obj = <allocate a Java object>;      // nothing in Java refers to it yet
let x   = <allocate something else>;     // ← a collection here reclaims `obj`
ctx.set_field(obj, SLOT, Value::Object(Some(x)));   // stores through a dead ref
```

`alloc_byte_buffer`, `alloc_typed_buffer`, `alloc_mapped_byte_buffer`,
`alloc_path`, `native_path_to_file`, `alloc_process_handle`, `alloc_zip_entry`,
`alloc_afc_channel`, `make_path_stream`, `pipe_open`, `native_ws_new`,
`dc_inet_socket_address` (four bindings), `ssc_accept_impl`, `sc_socket`,
`build_zip_entry_list`, `native_jarfile_get_manifest`, `native_file_list`,
`baos_ensure_capacity`, `read_process_environment` … the crate builds Java
objects this way everywhere, and essentially none of it roots.

Under a moving collector each is a stale local. Under the Generational
collector's NON-MOVING young sweep the object is simply unreachable and is
ZEROED in place — which is how a live `sun/nio/ch/FileChannelImpl` came back
with `fd` 0.

## Why the existing gate did not see any of it

`scripts/stale-receiver-audit.py` looks for a different shape (a receiver taken
BY VALUE by a funnel that returns `()`), and its level-0 allocator set has two
problems this audit had to correct:

* three tokens match **nothing** in the tree — `ctx.new_string`, `ctx.intern`,
  `ctx.box_`, zero occurrences each;
* it omits the most common allocator in these crates, `ctx.create_string`
  (2483 uses), plus `new_object` (202), `new_object_initialized` (407),
  `new_ref_array` (282) and `get_class_mirror` (214).

It also omits `begin_blocking_region`/`end_blocking_region`. For an I/O crate
that is the important one: a PEER thread's collection runs while this thread is
blocked, and the existence of `end_blocking_region_refs` — which *does* rewrite
native locals — is the proof that the plain form does not.

`set_field_by_name` is deliberately NOT GC-capable: it takes `&self`, resolves a
field index out of the class store and stores. (The first version of the
FileChannelImpl write-up claimed otherwise; that is corrected there.)

## How the rule was calibrated

A rule of this shape is worth exactly what its controls are worth, and the
2026-08-25 write-up is explicit that a heuristic producing confident dismissals
is worse than one producing noise. Two controls, both required to hold:

* **positive** — `file_channel.rs` at the commit *before* the FileChannelImpl
  fix must report all seven bindings that fix converted;
* **negative** — the same file *after* it must report zero.

Five rule defects were found by those controls, each of which had made the
audit silently wrong:

| defect | what it did |
|---|---|
| counted `{` as a statement continuation | the `fn …(…) {` line left the depth permanently positive, no statement ever closed, and the audit reported **zero everywhere** |
| `fn` anchored at column 0 | every function inside `mod tests` or an `impl` block was unindexed and its body glued onto the previous top-level fn |
| broke the scan at ANY rooting call | a `pin_native_root(channel)` two thirds down hid `position_lock`, `dispatcher` and `threads` — all genuinely unrooted. A false NEGATIVE, the dangerous kind |
| line-granular scanning | a reference passed as an ARGUMENT of the allocating call, on a later line, read as a use after it |
| no type filter | `let month = invoke_i32(ctx, obj, "getMonthValue")` — GC-capable RHS, used later, and an `i32` |

## What is fixed here

Five allocator helpers in `native-io/src/lib.rs`, chosen because every heap
`ByteBuffer` and every `Path` in the VM goes through them: `alloc_byte_buffer`,
`alloc_typed_buffer`, `alloc_mapped_byte_buffer`, `alloc_path`,
`native_path_to_file`. Each pins the fresh object across the allocation that
follows it and reads the live address back.

The audit reports 114 before and 109 after.

## What is NOT fixed

The other ~104. They are real work, not noise, and each needs the same
read-before-you-touch treatment — the 2026-08-25 pass fixed 48 sites and was its
own write-up. `scripts/unpinned-native-local-audit.py` is committed with the
current count so the population is visible and can only be argued DOWN.

The 66 parameter-shape candidates are entirely unassessed here. That is the
shape that produced 40 of the 2026-08-25 pass's 48 fixes, so it is likely the
larger half, and the fix for it is usually a signature change to
`&mut ObjectRef` rather than a pin — which makes every unconverted caller a
compile error instead of something a grep has to find again.

## What would make this cheap

Nothing in this crate needs a bespoke fix. The idiom is always the same, and
`NativeHandleScope` already exists for it. A mechanical conversion of the
`alloc_*` helpers — allocate, root, re-read, store — would close most of the
local half in one pass, and the compiler would carry it.
