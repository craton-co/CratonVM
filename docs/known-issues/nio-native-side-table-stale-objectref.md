# native-io NIO side-tables cache raw `ObjectRef`s across calls — stale after a moving GC

Status: open (hazard documented; the in-call windows were fixed 2026-07-07)

Date observed: 2026-07-07 (while closing
`gc-blocked-thread-frame-stale-thread-mirror`; dev @ `c15cee62`)

## The hazard

The NIO selector layer keeps raw Java object addresses in Rust-side
registries that no GC scans, roots, or remaps:

- `SelectorState.keys[*].key_obj` (`native-io/src/nio_selector.rs`) — the
  `SelectionKeyImpl` object, captured at `register` time and replayed into
  `selector.selectedKeys()` by `populate_selected_keys_field` on every
  select.
- `sk_table()[key_hash].channel` (+ attachment) — dispatched on by
  `refresh_selector_handles` → `channel_net_fd`.

Any **moving** young collection between the caching call and a later use
leaves these values pointing at vacated addresses. The downstream symptom is
the familiar recovered-stale-receiver shape: `Stale pointer detected in
invokevirtual receiver (all-zero header)` + a `NoSuchMethodError
java/lang/Object.<method>` fallback, or — worse — a *reused* slot silently
dispatching on the wrong live object.

In practice selector keys/channels promote to old gen quickly (registered
once, long-lived), so the exposure is concentrated in an object's first young
collections; observed at low rate (≈1 event per Tribes `TestDataIntegrity`
run pre-fix).

## What was already fixed (2026-07-07, gc-blocked-mirror branch)

The *in-call* staleness windows: `populate_selected_keys_field` now pins the
selectedKeys `set` + pending keys across its GC-capable `Set.add` loop, and
the blocking select/receive/accept/connect natives re-sync their held refs
via `end_blocking_region_refs` after their GC-blocking regions. What remains
is the *cross-call* window described above.

## Proper fix direction

Re-resolve objects from Java-side truth at use time instead of trusting the
cached address: the selector's own `keys` Set (heap truth, GC-remapped) can
be enumerated and matched to registry entries by identity hash (`key_hash` is
already the identity hash — the established side-table pattern). Alternatively
register these tables as remappable GC roots the same way
`ThreadRegistry::update_thread_objs_after_gc` handles thread mirrors.
