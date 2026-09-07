# A refresh contract in a PARAMETER MODE is laundered by a by-value wrapper

| | |
|---|---|
| **Status** | OPEN — rule added and calibrated, **18 laundering helpers, 36 call sites that then reuse their own copy.** None fixed yet. |
| **Scope** | `native-collections` 14 helpers / 34 sites, `native-builtins` 4 / 2, `native-io` 0, `native-api` 0. |
| **Tool** | `scripts/unpinned-native-local-audit.py --launder` |
| **Calibration** | reports the 2026-09-06 `collect_own_property_names` defect AND its caller before the fix; silent after |

## The shape

`ordered_snapshot_kv(obj: &mut ObjectRef)` states its own contract:

> "Taking `obj` by `&mut` is the point: it forces every caller's own receiver
> to be refreshed across the walk instead of silently carrying a pre-GC address
> into the pins and virtual dispatches that follow."

**A parameter mode only binds the immediate call.** A wrapper that takes the
same receiver BY VALUE satisfies that `&mut` with a COPY:

```rust
fn helper(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let mut this = this;                 // a copy
    ordered_snapshot_kv(ctx, &mut this); // refreshes the COPY
}                                        // refresh dies here
```

and its caller walks on with the pre-GC address. No rule scoped to one
function can see it: the callee looks correct, and the staleness is in the
caller. Both loop-aware and straight-line rules miss it, and so does the
in-source guard test in `properties_sidetable.rs`.

**This is the shape that produced an observed crash**, not a static finding:
`collect_own_property_names` was exactly this, and the `defaults` read its
caller then performed was the SIGSEGV traced by gdb on 2026-09-06.

## Calibration

`properties_sidetable.rs` at `80db5d314` and its parent:

| | helpers | call sites that reuse |
|---|---:|---:|
| before the fix | 2, incl. `collect_own_property_names` | 1 — `native_properties_property_names` passes `p` at :3602, uses it at :3603 |
| after the fix | 1 | 0 |

Line 3603 is the `props_defaults(ctx, p)` whose stale receiver crashed.

## One arm was removed after it fired 244 times

The first version also reported "re-reads a refresh into its own copy" —
a function that pins its parameter and re-reads it for its own body. **That is
the correct, ubiquitous idiom**, not laundering: nothing is promised to the
caller. It reported 244 helpers in one crate, which is what said it was the
wrong question. Only the `&mut`-to-a-refreshing-API arm has a contract being
defeated.

A second correction: the caller-side scan now stops at a REBINDING, because a
registration function holds several closures that each do
`let this = obj_arg(args, 0)?`, and without it every later closure read as a
reuse of the earlier one's receiver. `native-builtins` fell 3 sites to 2.

## The flagship: `stream_elements`, whose own doc says it is `&mut`

```rust
/// Back-compat alias: `stream_elements` is now `&mut` and itself materialises
/// lazy streams + real `ReferencePipeline`s via `toArray`.
fn stream_elements_mut(ctx: &mut dyn NativeContext, stream: ObjectRef) -> ... {
    stream_elements(ctx, stream)
}
```

`stream_elements` is **not** `&mut`. It is `mut stream: ObjectRef` — by value,
mutable binding — pins the stream, and refreshes its own copy. The comment
asserts the exact contract the signature fails to provide, and ten callers
pass a receiver and use it again afterwards.

It has **111 call sites**, so flipping the signature is a change of its own and
is not attempted here.

## The 18

`native-collections`: `stream_elements`, `prim_stream_values`, `cslm_arrays`,
`cslm_ensure_capacity` (three parameters), `lbq_ensure_capacity`,
`pq_ensure_capacity`, `ad_grow`, `cowal_ensure_lock_and_array`,
`lli_resnapshot`, `alloc_view_backing`, `native_map_put_evict_pinned`,
`rooted_across1`.
`native-builtins`: `collect_store_entries`, `capture_huc_ssl_context`,
`https_has_session`, `https_peer_chain_or_throw`.

`rooted_across1` is worth its own look: a helper whose entire job is rooting,
taking its receiver by value.

## What this does NOT establish

No dynamic proof for any of the 36, the same caveat the sibling pages carry.
The difference remains that this shape has a proven instance.

## Next

* Fix `stream_elements` by making the signature match its own documentation,
  and let the 111 call sites fall out of the compiler.
* The `_ensure_capacity` family is the same shape repeated and probably one
  edit each.
* The rule does not check whether the caller's later use is on a path the GC
  can reach; it reports the reuse and leaves that to the reader.
