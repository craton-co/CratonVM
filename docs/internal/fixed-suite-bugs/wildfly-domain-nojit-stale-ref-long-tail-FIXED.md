# WildFly domain no-JIT stale-reference long tail — FIXED

Status: FIXED on 2026-07-18.

This document closes the residual described by the former
`docs/known-issues/wildfly-domain-nojit-stale-ref-long-tail.md`. It follows the
larger WildFly stale-reference repair documented in
`wildfly-cce-residuals-blocked-region-and-pin-waves-FIXED.md`.

## Final diagnosis

The remaining failures were not a lambda/synthetic-class registry root gap,
selective-promotion remap gap, or invoke-cache entry-barrier gap.

They were ordinary raw `ObjectRef` locals held across GC-capable work inside
native methods. The repaired ring-aware pin diagnostics made several failures
look like "stale at native entry" because the first attempted pin happened
late, after an earlier callback, allocation, collection materialization, or
array conversion had already moved the object. Pinning the raw local at that
point faithfully reported `PIN-STALE`; it did not prove that the native
received a stale argument.

The high-bit synthetic class ids in the first captures were incidental: the
unrooted locals happened to be stream consumers, spliterators, or MSC injector
lambdas. Later captures involved ordinary classes and confirmed the mechanism
was object lifetime, not synthetic-class ownership.

The required discipline is:

1. Pin every object that will survive the next GC-capable operation while its
   address is still current.
2. Re-read it through that pin immediately before each later use.
3. Treat a group of pins as one strictly LIFO scope and truncate once from the
   first handle.
4. Root every object in a Rust snapshot before invoking Java on any element of
   that snapshot.

## Repairs

### Collections and streams

`native-collections/src/lib.rs` now:

- pins a map's bucket array before a chain walk can invoke Java `equals`;
- refreshes the caller-pinned replacement value immediately before both
  existing-node update stores, including the null-key branch;
- roots both the destination collection and the source varargs array across
  every callback in `Collections.addAll`, refreshing them on each iteration;
- pins an entry-set source before `collect_entries_any` materializes it;
- pins a terminal stream consumer before lazy dispatch or eager stream
  materialization; and
- unwinds each native-owned pin group from its earliest handle.

The last map correction was found by d29. Its store canary showed a stale
replacement value written to JDK HashMap node slot 2 from
`native_map_put_evict_pinned`, reached through
`native_chm_compute_if_present`. `map_keys_equal` had collected after the
replacement's earlier refresh.

### ServiceLoader

`native-builtins/src/service_loader.rs` now:

- roots newly allocated `ArrayList` instances before their Java constructors;
- keeps provider-list, ServiceLoader, constructor-array, constructor, provider,
  and service-class inputs live across reflection and retry paths;
- fixes a LIFO truncation that discarded a later cause pin in
  `provider_construction_error`;
- roots every collected iterator result until synthetic-stream construction;
- keeps `findFirst`'s iterator and result live through `Optional.of`;
- roots every object-valued synthetic-stream input before allocating the
  backing array; and
- roots each `forEach` element across its action callback.

### JBoss MSC

`native-builtins/src/jboss_msc.rs` now:

- pins `ServiceBuilderImpl` at `install` entry and refreshes it before every
  later field read or helper call;
- roots dependency/provides maps, sets, and arrays before `keySet`,
  `entrySet`, `values`, and `toArray`;
- roots dependency registrations, injector lists, and injector arrays for the
  complete capture interval; and
- roots `serviceTarget` immediately after reading it, before provides
  materialization. The old late target pin was a captured live `PIN-STALE`
  site.

### JBoss logging

The d30 follow-up canary reached farther into domain boot and exposed one final
snapshot residual in `native_jboss_logging_logger_do_logf`: it copied an
`Object[]` into raw `Value`s, then invoked `toString()` on the first non-String
parameter. A moving GC made a later parameter stale before `read_string`
reached it.

`native-builtins/src/logmanager.rs` now:

- roots all `doLogf` object arguments before formatting;
- roots every object parameter in the snapshotted array before invoking Java
  on any parameter;
- refreshes each parameter immediately before rendering it;
- keeps the trailing throwable rooted across all parameter callbacks; and
- applies the same message/throwable rule to the FQCN logger overload.

A focused mock-GC regression test remaps the second format parameter during
the first parameter's `toString()`. It verifies that the second callback sees
the remapped address and that the native pin stack returns to zero.

## Refuted paths retained for future investigations

- A plain reference `set_field` did not complete a moving GC in the firing
  batches. The `[SETFIELD-GC]` epoch probe remained silent.
- Repaired `PIN-TABLE-STALE` reports were preceded by `PIN-STALE`; pin-table
  entries were not becoming stale after a valid pin.
- `CRATONVM_DBG_BLOCKED_ACCESS=warn` did not show an
  excluded-while-running/blocked-region census race.
- No new lambda registry, selective-promotion, or invoke-cache repair was
  required.

## Verification

Probe environment:

```text
CRATONVM_DISABLE_JIT=1
CRATONVM_DBG_STALE_OBJREF=1
CRATONVM_DBG_STALE_OBJREF_CYCLES=8
CRATONVM_DBG_BLOCKGC=1
CRATONVM_DBG_UNPIN_RING=1
RUST_BACKTRACE=1
```

Final frozen binary:

```text
probes/cratonvm-wildfly-staleref-d34
sha256 e1e153514ded01d2a62ce062ea6d93a728135003c98e87a01053033906930257
```

Canary results:

- `D31A_001`, 900 seconds: zero `PIN-STALE`, `PIN-UNDERFLOW`, hard stale
  dereferences, or cast failures; server one reached `WFLYSRV0025` and then
  remained clean for the rest of the run.
- `D31A_002`, 1,200 seconds: the same zero-fault result; server two reached
  `WFLYSRV0025` and remained clean for roughly 13 minutes. The peer server in
  each schedule remained alive and actively progressing, giving complementary
  clean completion coverage for both managed-server configurations and 2,100
  aggregate canary-seconds.
- `D33A_001`, 900 seconds on the final post-rebase source: zero
  `PIN-STALE`, `PIN-UNDERFLOW`, hard stale dereferences, or cast failures;
  server two reached `WFLYSRV0025` and remained clean for more than seven
  minutes.
- `D34FINAL_001`, 600 seconds after the last dev synchronization: verdict
  `OK_BOTH_SERVERS`; both server one and server two reached `WFLYSRV0025` in
  the same run, followed by roughly three clean minutes, with zero
  `PIN-STALE`, `PIN-UNDERFLOW`, hard stale dereferences, or cast failures.

The pre-fix controls remained discriminating:

- d29 failed at about 282 seconds with the HashMap existing-node stale-value
  store.
- d30 passed that point, then failed in host-controller `doLogf` after a
  managed server had reached `WFLYSRV0025`.
- d32 passed both earlier points, then failed in server-two
  `native_collections_add_all`: the first destination `add()` callback moved
  the raw source array before the second element read.

Rust verification on the final post-rebase source:

- `cargo test -p cratonvm-native-builtins --lib`: 3,012 passed, 0 failed,
  6 ignored.
- `cargo test -p cratonvm-native-collections --lib`: 74 passed, 0 failed.
- `cargo check -p cratonvm-native-builtins -p cratonvm-native-collections`:
  passed.
- focused moving-GC `doLogf` regression: passed.

Probe binaries/logs are intentionally not committed; they remain in the
isolated Azure worktree's `probes/` directory.
