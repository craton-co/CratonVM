# common-w26c-proposal: make the per-slice freed-span drop sub-linear in the side tables

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 45
> of 54).** Not built, not measured. **Gate:** slices per cycle times table
> sizes on a Properties- / TLS-heavy Generational run; retire if negligible.
> **Size:** M.

- **Status:** PROPOSAL. Filed 2026-09-25, gc-common round 2026-09-23, wave 26,
  lane C26. Performance only; no correctness impact. Not measured.
- **Owner:** `vm/src/memory/addr_keyed.rs` (`drop_address_keyed_rows_in`) and
  the side tables it calls.

## Where it stands

The Generational concurrent sweep calls
`addr_keyed::drop_address_keyed_rows_in(shared, &freed_spans)` once per slice
that freed anything (32 Ki objects per slice by default), under the old-gen
guard (`vm/src/runtime/interpreter/gc_and_alloc.rs`, the sliced sweep loop).
Every table it reaches is walked WHOLE per call, whatever the size of the
slice's spans:

- lock-key registry: since gc-common w26-c one in-place pass under the
  registry mutex (`native-builtins/src/lib.rs::gc_drop_lock_keys_in_spans`),
  no copy -- but it still visits every slot of every VM in the process;
- Throwable backtrace shards: `retain` over each shard, write lock taken even
  when the shard is empty;
- `t27_tls::gc_sweep_tls_rows`: copies every weak-owner address of the VM into
  a `Vec` per call, then judges it (the pattern w26-c removed from the
  lock-key registry);
- Locale (`gc_sweep_locale_rows`), logging (`gc_sweep_logging_rows`), and the
  native-io / httpserver / net / zip / SubmissionPublisher / Undertow sweeps:
  an in-place `retain` over the whole table, with a `dyn` predicate per row.

So one concurrent cycle over an old gen of `O` objects costs about
`O / 32Ki` full walks of every side table, all under the old-gen guard (which
blocks old-gen allocation) and under each table's lock (which blocks that
table's mutators in every VM of the process, for the process-wide tables).

## Proposal

1. An address-ordered index where a table is large and process-wide: for the
   lock-key registry, a per-VM `BTreeMap<usize /*last_ptr*/, (u32 /*hash*/,
   u32 /*generation*/)>` maintained at mint (`lock_key_for`), removal (the
   sweeps, `forget_vm_lock_keys`) and remap (the stop-the-world sweep, which
   already visits every slot of the VM and would rebuild it). A slice then
   visits only `range(start..start + len)` per span. Cost: one `BTreeMap`
   insert per mint (the first key per object, not per lookup), under the
   registry mutex it already holds.
2. A span-aware entry point for the TLS weak owners (`t27_tls`), like
   `gc_drop_lock_keys_in_spans`: judge the range test under the table lock in
   one pass instead of copying the addresses out per slice.
3. Batch the slices: `drop_address_keyed_rows_in` only has to run before the
   old-gen guard drops AND something could be allocated onto a freed span.
   Accumulating spans over several slices is unsafe for the direct old-gen
   allocation route (`common-w3g-concurrent-sweep-direct-old-alloc-window`),
   so this needs the allocator to refuse freed-but-undropped spans first --
   probably not worth it next to 1 and 2.

Measure first: count registry slots and TLS weak owners, and slices per
cycle, in a Properties-heavy / TLS-heavy Generational run.

## Confirmation grep

```
rg -n "fn gc_drop_lock_keys_in_spans|fn drop_lock_keys_in_spans_locked" native-builtins/src/lib.rs
rg -n "let candidates: Vec<\(\(TlsOwnerKind, u64\), ObjectRef\)>" native-builtins/src/t27_tls.rs
```

## What retires this page

A measurement showing the per-slice walks are negligible at realistic table
sizes, or the index / span entry points above.
