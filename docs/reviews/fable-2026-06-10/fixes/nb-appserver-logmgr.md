# Fix: nb-appserver-logmgr — B4 logmanager.rs cached ObjectRef has no GC remap

## Finding (B4, HIGH, memory safety)

`native-builtins/src/logmanager.rs` caches live Java object addresses as raw
`u64` in five process-global side-tables, with **no GC scan/remap hook**:

- `singleton_cell()` — `Option<u64>` (the `LogManager` singleton).
- `logger_registry()` — `HashMap<String, u64>` (`java/util/logging/Logger` mirrors).
- `jboss_log_context_singleton()` — `Option<u64>` (the JBoss `LogContext`).
- `jboss_logger_registry()` — `HashMap<String, u64>` (`org/jboss/logmanager/Logger`).
- `attachments()` — `HashMap<(u64,u64), u64>` whose **keys are themselves object
  addresses** `(receiver, AttachmentKey)` plus a value address.

Addresses are stored via `obj.as_ptr() as u64` and reconstructed via
`object_from_u64` → `ObjectRef::from_raw`. The safety comments (e.g. `:201-207`,
"we never free … so the address remains valid") conflate *never freed* with
*never relocated*. After a moving/young GC relocates a cached `LogManager` /
`Logger` / `LogContext` / attachment object, the cached `u64` is stale and
`from_raw` yields a dangling-or-wrong object — a use-after-free. This is the
same bug class MEMORY.md documents for the classloader GC-root gap and the
lang_math cache remap, and `jboss_msc.rs` already solves with a scan + remap pair.

## Root cause

The side-tables hold strong references to Java heap objects that are invisible
to every existing GC root scanner, and they are never updated with the
post-collection pointer map. Nothing pins the objects (so a moving collector may
reclaim them) and nothing repoints the cached addresses (so survivors that
relocate leave the cache dangling).

## Exact change (native-builtins/src/logmanager.rs — the only file I own)

1. **Added `pub fn gc_scan_logmanager_roots(out: &mut Vec<ObjectRef>)`** — reports
   every cached object as a GC root: the singleton, both logger registries, the
   JBoss `LogContext` singleton, and for the attachment table all three of
   `(receiver, key, value)` per entry (the keys must be rooted too, or the
   `AttachmentKey` decays and the post-move key remap can't find its new address).
   Mirrors `jboss_msc::gc_scan_msc_service_roots`. Locks are blocking and never
   held across a Java allocation (no self-deadlock).

2. **Added `pub fn gc_update_logmanager_refs(pointer_map: &HashMap<usize,usize>)`**
   — repoints every stored address to its relocated slot; addresses absent from
   the map (not moved this cycle) are left unchanged. For `attachments` it
   `drain()`s and rebuilds the map because **both halves of the key and the
   value** can relocate (cannot mutate keys in place). Mirrors
   `jboss_msc::gc_update_msc_service_refs`. Early-returns on an empty map.

3. Extended the `#[cfg(test)] reset_state_for_tests()` helper to also clear the
   three JBoss-side tables (it previously cleared only `singleton_cell` +
   `logger_registry`), so test isolation covers all tables the new scan touches.

4. Added four `#[cfg(test)]` tests: scan reports every cached object as a root;
   update repoints every cached address (cardinality preserved, attachment key
   rebuild intact); update is a no-op for an empty pointer map; addresses absent
   from the map are left untouched.

## REGISTRATION REQUIRED (call sites are in files I do NOT own)

The two new functions are inert until wired into the collector. The exact call
sites, mirroring the existing MSC wiring (step 19), are documented in a
load-bearing comment in `logmanager.rs` right above `gc_scan_logmanager_roots`:

- `vm/src/memory/roots.rs` — add **after** line 255
  (`...jboss_msc::gc_scan_msc_service_roots(&mut roots);`):
  ```rust
  cratonvm_native_builtins::logmanager::gc_scan_logmanager_roots(&mut roots);
  ```
- `vm/src/memory/gc.rs` — add **after** line 305
  (`...jboss_msc::gc_update_msc_service_refs(pointer_map);`):
  ```rust
  cratonvm_native_builtins::logmanager::gc_update_logmanager_refs(pointer_map);
  ```

Both `logmanager::gc_scan_logmanager_roots` / `gc_update_logmanager_refs` are
`pub` so the `vm` crate can call them exactly like the `jboss_msc` equivalents
(which `roots.rs`/`gc.rs` already reference). **Until these two lines are added
the stale-pointer hazard persists** — the in-file change is necessary but not
sufficient on its own; the registration must land in those two vm-crate files.

## Files touched

- `native-builtins/src/logmanager.rs` (scan + remap fns, reset helper, 4 tests).
- `docs/reviews/fable-2026-06-10/fixes/nb-appserver-logmgr.md` (this note).

## Tests added

- `b4_gc_scan_reports_every_cached_object_as_root`
- `b4_gc_update_repoints_every_cached_address`
- `b4_gc_update_is_noop_for_empty_pointer_map`
- `b4_gc_update_leaves_unmoved_addresses_untouched`

(Tests build a synthetic `pointer_map`; `gc_update_*` never dereferences the
fake target addresses — it only rewrites `u64`s — and `gc_scan_*`'s
`object_from_u64` is `ObjectRef::from_raw` with no dereference, so using
non-heap synthetic targets is safe in-process. The mock `NativeContext`'s
`alloc_concurrent_synthetic` path is exercised by existing tests in this file.)

## Follow-up & risk

- **Blocking follow-up (REQUIRED):** the two vm-crate registration lines above.
  Owner of `vm/src/memory/roots.rs` + `vm/src/memory/gc.rs` must add them; this
  is the same one-line-each pattern already present for MSC and classloader.
- Risk of the in-file change is low: the new functions are additive and unused
  until registered (no behavior change to existing natives). Lock ordering
  matches the MSC precedent (short critical sections, never held across a Java
  alloc). The attachment-table rebuild allocates a fresh `HashMap` per moving GC;
  attachment counts are tiny (JBoss facade singletons), so cost is negligible.
- Not addressed here (out of scope / not my file): the `attachments` key being
  raw addresses is inherently fragile vs identity-hash keying; the remap keeps it
  correct, but a future cleanup could key on identity-hash to avoid key churn.
