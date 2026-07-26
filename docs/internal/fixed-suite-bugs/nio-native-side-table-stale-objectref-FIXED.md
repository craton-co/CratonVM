# native-io NIO side-tables cache raw `ObjectRef`s across calls — stale after a moving GC [FIXED]

Status: FIXED 2026-07-07 (branch `fix/nio-sidetable-stale-objectref-20260707`,
dev @ `1d00ec72`).

This doc's headline claim — that `SelectorState.keys[*].key_obj` and
`sk_table()[key_hash].channel`/`attachment` have "no GC scans, roots, or
remaps" — was already stale when the doc was written (2026-07-07, dev
`c15cee62`). The scan/remap pair it asks for under "Proper fix direction" had
existed since 2026-06-21:

- `gc_scan_selector_roots` (`native-io/src/nio_selector.rs:2192`) pushes every
  per-selector `keys[*].key_obj` AND every `sk_table` `channel` / `selector` /
  `attachment` ref as a GC root; wired into `collect_roots`
  (`vm/src/memory/roots.rs:548`).
- `sk_table_update_after_gc` (`native-io/src/nio_selector.rs:2127`) remaps the
  same set through the GC pointer map; called from `update_all_roots`
  (`vm/src/memory/gc.rs:390`).

Landed by `b76896e5` and `61447fc3` (both 2026-06-21) — ancestors of
`c15cee62`. Confirmed both run on every moving-collection path (generational
moving young / selective-promotion tenure / major mark-compact, and G1 young /
mixed / evacuation-failure retry all compose into the single `pointer_map`
the once-per-GC initiator's `update_all_roots` consumes — see "Audit" below).
The double-remap-per-thread hypothesis floated during triage was also
checked and is not real: `update_all_roots`'s global-table remap steps run
exactly once per GC (on the initiator only); non-initiator/blocked threads
remap only their own thread-local state via separate, disjoint code paths
(`apply_pointer_map_to_thread`, `check_post_block_gc_refs`).

## But: the audit found a REAL sibling bug, same shape, same file

`Selector.selectedKeys()` / `Selector.keys()` are backed by
`selector_selected_keys` / `selector_keys`
(`native-io/src/nio_selector.rs:2600`/`:2624`), both of which call
`build_set` (`:2655`) to allocate a **fresh** `java.util.HashSet` and populate
it with the ready/registered `key_obj`s. `build_set` ran
`HashSet.<init>` and a `Set.add` loop against the freshly-allocated `set` held
only in an **unpinned Rust local** — each of those `invoke_special`/
`invoke_virtual` calls runs Java bytecode that can trigger a moving GC, so a
collection mid-loop left `set` (or a not-yet-added `key`) pointing at a
vacated from-space address. This is the exact bug shape the doc's "in-call
fixes" section already fixed in the sibling function
`populate_selected_keys_field` — `build_set` was the one caller of this
pattern that never got the same treatment.

Reproduced 2026-07-07 via `CRATONVM_GC_STRESS=4194304` (forces a young GC
every 4MB allocated) against Tomcat Tribes'
`org.apache.catalina.tribes.test.channel.TestDataIntegrity`: an all-zero-header
`java/util/Set` receiver + `NoSuchMethodError Object.add`, caller
`org/apache/catalina/tribes/transport/nio/NioReceiver.listen()V` — exactly at
bytecode `Selector.selectedKeys():Ljava/util/Set; → Set.iterator()`.

**Fix**: `build_set` now pins `set` (and each pending key) with
`ctx.pin_native_root` and re-reads through the pin via `ctx.read_native_pin`
before every dispatch, unpinning once at the end — identical to
`populate_selected_keys_field`'s existing pattern
(`native-io/src/nio_selector.rs:2687-2703`, commit on this branch).

## Audit: every moving path runs scan + remap exactly once

Every collection that can move an object funnels through one of six
GC-initiator sites in `../../../vm/src/runtime/interpreter.rs` (`maybe_gc` :682/:853,
`maybe_gc_forced` :941/:978, `force_gc_from_native` :1113/:1152). Each:

1. builds its root set with `collect_roots` (runs `gc_scan_selector_roots`)
   BEFORE `VmHeap::collect_garbage`, and
2. calls `update_all_roots(shared, thread, &result.pointer_map)` AFTER, which
   runs `sk_table_update_after_gc` with the full composed map.

Internal moving-phase maps all compose into that one `GcResult::pointer_map`:
generational moving young + promotion (`gc/src/gen_heap.rs:3255ff`), Phase-5
major mark-compact chaining minor entries through `compact_map`
(`gc/src/gen_heap.rs:3877-3914`), the non-moving young sweep's selective
promotion (`evac_map`, `gc/src/gen_heap.rs:6186`), and G1 young / mixed /
evacuation-failure retry (`gc/src/g1.rs:6377-6401`, `retry_after_evacuation_failure`
at `:1659`). Non-moving phases (G1 concurrent mark/remark, old-gen concurrent
mark-sweep, G1 cleanup which only frees zero-live regions) need only the root
scan, which they get.

## Verification (2026-07-07, dev `1d00ec72`)

- `cargo test -p cratonvm-native-io`: 330/330 pass, including the scan/remap
  symmetry tests `gc_scan_selector_roots_collects_sk_table_and_key_obj_refs`
  and `gc_scan_selector_roots_skips_absent_attachment`
  (`native-io/src/nio_selector.rs:3891`/`:3944`).
- New targeted regression: `../../../vm/tests/nio_selector_build_set_gc.rs` +
  `../../../vm/tests/resources/cratonvm/NioSelectorBuildSetGc.java` — drives 400
  connect/register/select/`selectedKeys().iterator()` cycles under
  `CRATONVM_GC_STRESS=65536` (a stress level ~64x tighter than the Tribes
  repro's, targeted precisely at this code path). PASSES post-fix
  (0.95s, zero stale-pointer/NoSuchMethodError signal); reproduces the crash
  pre-fix.
- Tomcat Tribes `TestDataIntegrity` under `CRATONVM_GC_STRESS=4194304`:
  pre-fix crashed inside `NioReceiver.listen()` at iteration ~1 (all-zero
  `java/util/Set` receiver). Post-fix: runs substantially further with zero
  selector/SelectionKey/sk_table-attributed stale-pointer signal. (The run
  does surface unrelated stale-pointer warnings in Tribes' own
  `ObjectInputStream`/`ObjectOutputStream` message serialization path — a
  different, pre-existing native subsystem with the same missing-pin shape;
  spun off separately, not fixed here since it is unrelated to the NIO
  selector tables this doc covers.)
- Baseline (`CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1`, no GC-stress
  amplifier) `TestDataIntegrity` run: 3/5 pass, 2 fail on UDP-multicast
  packet-count assertions (`expected:<10000> but was:<9341/9998>` — ordinary
  lossy-transport counting, no stale-pointer/NoSuchMethodError signal at all).
  Matches the doc's own caveat that absence of signal under normal load is
  weak evidence; the GC-stress amplifier was what made the real bug
  observable.

## Original doc (historical, as written 2026-07-07)

> Status: open (hazard documented; the in-call windows were fixed 2026-07-07)
>
> Date observed: 2026-07-07 (while closing
> `gc-blocked-thread-frame-stale-thread-mirror`; dev @ `c15cee62`)
>
> ### The hazard
>
> The NIO selector layer keeps raw Java object addresses in Rust-side
> registries that no GC scans, roots, or remaps:
>
> - `SelectorState.keys[*].key_obj` (`../../../native-io/src/nio_selector.rs`) — the
>   `SelectionKeyImpl` object, captured at `register` time and replayed into
>   `selector.selectedKeys()` by `populate_selected_keys_field` on every
>   select.
> - `sk_table()[key_hash].channel` (+ attachment) — dispatched on by
>   `refresh_selector_handles` → `channel_net_fd`.
>
> Any **moving** young collection between the caching call and a later use
> leaves these values pointing at vacated addresses. The downstream symptom is
> the familiar recovered-stale-receiver shape: `Stale pointer detected in
> invokevirtual receiver (all-zero header)` + a `NoSuchMethodError
> java/lang/Object.<method>` fallback, or — worse — a *reused* slot silently
> dispatching on the wrong live object.
>
> In practice selector keys/channels promote to old gen quickly (registered
> once, long-lived), so the exposure is concentrated in an object's first young
> collections; observed at low rate (~1 event per Tribes `TestDataIntegrity`
> run pre-fix).
>
> ### What was already fixed (2026-07-07, gc-blocked-mirror branch)
>
> The *in-call* staleness windows: `populate_selected_keys_field` now pins the
> selectedKeys `set` + pending keys across its GC-capable `Set.add` loop, and
> the blocking select/receive/accept/connect natives re-sync their held refs
> via `end_blocking_region_refs` after their GC-blocking regions. What remains
> is the *cross-call* window described above.
>
> ### Proper fix direction
>
> Re-resolve objects from Java-side truth at use time instead of trusting the
> cached address: the selector's own `keys` Set (heap truth, GC-remapped) can
> be enumerated and matched to registry entries by identity hash (`key_hash` is
> already the identity hash — the established side-table pattern). Alternatively
> register these tables as remappable GC roots the same way
> `ThreadRegistry::update_thread_objs_after_gc` handles thread mirrors.
