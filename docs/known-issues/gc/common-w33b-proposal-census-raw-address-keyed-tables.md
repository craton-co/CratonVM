# PROPOSAL: make the address-keyed table census see tables keyed by raw `usize` addresses

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 50
> of 54).** Not built. **Gate:** the census test fails when a new
> integer-address-keyed table lands without a disposition marker. **Size:**
> XS-S.

Status: OPEN (proposal, gc-common w33-b, 2026-09-23 round). Not a defect page.

## What the census sees today

`vm/src/memory/addr_keyed.rs::tests::the_address_keyed_table_census_is_complete`
counts, per source file, the non-comment lines containing `HashMap<ObjectRef` or
`HashSet<ObjectRef`, and requires each file to be listed in `census::AUDITED`
with a stated GC disposition. It exists because a table keyed by an object's
address "breaks nothing at review time; it just starts answering a lookup for a
new object with a dead object's value once the allocator reuses the address".

## What it does not see

A table whose key holds the address as a plain `usize` is the same hazard and
matches neither pattern. Examples on this base, all of which DO have a
disposition, so nothing here is a live bug:

- `native-builtins/src/lang_invoke.rs`, `LAMBDA_CALLSITE_CACHE`:
  `FxHashMap<(usize, LambdaKey), ObjectRef>`, where `LambdaKey`'s `invoked`,
  `sam`, `impl_`, `instantiated` are object addresses. Scanned (key objects
  are roots, or loader-conditional through `defer_or_root` since interpreter
  round i1 wave 9) and re-keyed by `gc_update_lambda_callsite_cache_refs`.
- `vm/src/threading/monitor.rs`, the monitor index
  `FxHashMap<usize, Arc<Monitor>>` (8-byte header): re-keyed by
  `MonitorCleanup::remap_after_gc`, pruned by `prune_dead`.
- The `(vm, identity hash)`-keyed native tables the round has converted
  (`common-w28b-*`, `common-w31b-*`) had the same invisibility: an identity
  hash is not an address, but a table keyed by one is a disposition question
  too (and since the 8-byte header, same-class hashes repeat every 2^20 mints).

A future table of that shape gets no prompt from the gate.

## Proposed direction

1. A marker comment the census also counts, e.g. `// addr-keyed: <disposition>`
   on the declaration line of any table whose key contains an object address
   (or an identity hash) as an integer, and a census row per file, exactly as
   for `HashMap<ObjectRef`.
2. Optionally a heuristic finder in the same test for
   `HashMap<(usize` / `FxHashMap<(usize` / `HashMap<usize` declarations in
   `native-builtins`, `native-io` and `vm/src/runtime`, reported as "classify
   me" (marker present, or listed in an allow-list of non-address `usize`
   keys such as VM identities and class ids). Noisy by nature, so a
   population ratchet rather than a hard gate.

## Confirmation grep

`rg -n "FxHashMap<\(usize|HashMap<\(usize|HashMap<usize" native-builtins/src native-io/src vm/src --glob '*.rs'`

## What would retire it

The census (or a sibling test) failing when a new integer-address-keyed table
is added without a recorded disposition.
