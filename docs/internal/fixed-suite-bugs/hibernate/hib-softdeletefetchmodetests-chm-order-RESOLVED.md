# `SoftDeleteFetchModeTests` — resolved: `ConcurrentHashMap` default-capacity mismatch reordered entity processing relative to HotSpot

**Status: FIXED.** Branch `fix/hib-softdelete-statemanagement-npe-20260705`, dev base `ad5d5cfe`.

Supersedes [`docs/known-issues/hib-softdeletefetchmodetests-statemanagement-null-npe.md`](../../known-issues/hib-softdeletefetchmodetests-statemanagement-null-npe.md) (removed from `../../../known-issues` by this fix).

## Two separate findings

### 1. The originally-reported NPE was already fixed on `dev` before this investigation started

The known-issues doc (discovered 2026-07-05 at dev `49aaf713`) reported
`NullPointerException: ... "this.stateManagement" is null` during
`SoftDeleteFetchModeTests` bootstrap. Reproducing with a binary built fresh
from the *current* dev tip (`ad5d5cfe`, 64 commits ahead of `49aaf713`)
showed the NPE **no longer reproduces** — `SoftDeleteStateManagement`'s
`<clinit>` now runs correctly and `Stateful.getStateManagement()`'s
reflective `Field.get(null)` returns the real singleton. The original repro
had been run against a stale binary (47 commits behind dev) borrowed from
another worktree for speed; once discovered, all further work used a binary
built fresh from the worktree's own dev-based branch.

Which of the 64 intervening commits fixed it was not bisected — a plausible
candidate flagged during investigation (not confirmed) is a commit resolving
"`java.lang.Class` mirror field slots by name instead of hardcoded JDK-25
indices," which would explain a `mirror_class_id`/`ClassId(0)` resolution
failure silently no-op'ing `ensure_class_initialized` for a class reached
only via a `Foo.class` literal (exactly this scenario: `SoftDeletable`'s
`setStateManagementType(SoftDeleteStateManagement.class)`). Not chased
further since the NPE is confirmed gone on current dev.

### 2. The actual current-dev residual bug: `ConcurrentHashMap` iteration order diverges from HotSpot

With the NPE gone, `SoftDeleteFetchModeTests` still failed, but with the
*original* symptom from `hib-linux-fail-bucket-triage-20260703.md`:
`AssertionFailedError: Expecting UnsupportedMappingException` (the test's
`fail(...)` fires because `buildSessionFactory()` no longer throws the
expected validation error).

**Root cause:** Hibernate's `MappingModelCreationProcess.execute()`
processes entity persisters by iterating an `EntityPersisterConcurrentMap`
(backed by a real `java.util.concurrent.ConcurrentHashMap<String,...>`) in
map order — `Root` and `Child` in this test. `ToOneAttributeMapping`'s
LAZY+`@SoftDelete` validation (`ToOneAttributeMapping.java:457-466`) is
checked synchronously while building `Root`'s `child` attribute mapping, and
requires `Child`'s own `prepareMappingModel()` (which populates
`Child.auxiliaryMapping` / `getSoftDeleteMapping()`) to have **already run**.
HotSpot's real `ConcurrentHashMap` processes `Child` before `Root` for these
two specific entity-name hashes; CratonVM's native `ConcurrentHashMap`
reimplementation processed `Root` before `Child` — the opposite order —
so `Child.getSoftDeleteMapping()` was still `null` when `Root`'s check ran,
silently skipping the exception.

Verified directly with a minimal probe (`ConcurrentHashMap<String,String>`
with just these two keys): HotSpot yields `Child, Root`; CratonVM (before
fix) yielded `Root, Child`. Both hash codes are identical between JVMs
(`String.hashCode()` is spec-defined) — the divergence is purely in
CratonVM's bucket-order emulation.

CratonVM's `ConcurrentHashMap` (`../../../../native-collections/src/lib.rs`) is a full
native reimplementation using a segmented layout (16 independent mini
hash-tables, for write-striping concurrency) rather than HotSpot's single
flat table. An existing mechanism, `chm_reorder_by_virtual_bucket`
(`native-collections/src/lib.rs:~27607`), already exists specifically to
reconstruct HotSpot's flat-table iteration order for order-sensitive callers
(added for an earlier, similar bug affecting Spring's
`SimpleAliasRegistry`/`XmlBeanDefinitionReaderTests`) — it stable-sorts
collected entries by `hash & (virtual_total_capacity - 1)`, where
`virtual_total_capacity` is meant to approximate "what real JDK's table size
would currently be."

The bug: `chm_total_capacity()` computed this by summing the physical
bucket-array length of all 16 segments for a **default-constructed**
(no-arg) `ConcurrentHashMap()` — `16 segments × 4 starting capacity = 64` —
but real JDK's no-arg constructor lazily sizes its actual table to
`DEFAULT_CAPACITY = 16` on first `put`, only growing past that once entries
exceed the 0.75 load-factor threshold. For a lightly-loaded map (2 entries,
as here), real JDK's table stays at 16. The virtual-capacity mask (63
instead of 15) put `Root` and `Child` in different relative bucket order
than a real 16-slot table would.

Numerically, for this test's exact keys (`Root` hash=-779410042,
`Child` hash=1594025208, JDK/CHM spread = `h ^ (h>>>16)`):

| virtual capacity (mask) | Root bucket | Child bucket | order |
|---|---|---|---|
| 16 (mask 15) — HotSpot's real default | 13 | 10 | **Child, Root** (matches HotSpot) |
| 64 (mask 63) — CratonVM's old default | 13 | 58 | **Root, Child** (bug) |

## Fix

`../../../../native-collections/src/lib.rs`: `native_chm_init_default` (the no-arg
`ConcurrentHashMap()` constructor) now allocates `CHM_DEFAULT_INIT_SEGMENTS`
(4) segments of `CHM_DEFAULT_SEGMENT_CAP` (4) each — total 16, matching
HotSpot's real default — instead of the previous
`CHM_DEFAULT_SEGMENTS × CHM_DEFAULT_SEGMENT_CAP` (16 × 4 = 64).

An initial attempt kept 16 segments but shrank each to capacity 1 (also
totalling 16) and was rejected: `native_map_put`'s resize check
(`size + 1 > (cap * 3) / 4`) integer-truncates the threshold to 0 for
`cap == 1`, so the very first insert into any segment immediately triggers
a resize, and the segment capacities (hence `chm_total_capacity`'s sum) no
longer stay at 16 even for a two-entry map. 4 segments of capacity 4 keeps
the same total but tolerates up to 3 entries per segment before resizing,
matching real JDK's actual behavior for lightly-loaded default maps.

**Trade-off:** default-constructed `ConcurrentHashMap`s that are never given
an explicit initial capacity now have 4-way write-striping instead of
16-way, for their entire lifetime (segment count is fixed at construction;
only each segment's own capacity grows via resize). This is a throughput/
contention consideration, not a correctness one, and is judged acceptable
given the concrete, deterministic iteration-order-correctness win — no test
in the regression sweep below exercises heavy concurrent-write contention on
a default CHM. Explicit-capacity constructors
(`native_chm_init_capacity`/`native_chm_init_full`, and their own already-
correct `tableSizeFor`-based sizing from an earlier fix for
`SimpleAliasRegistry`) are unaffected by this constant.

## Verification

- `SoftDeleteFetchModeTests`: `found=1 ok=1 failed=0` (was `failed=1`).
- Minimal `ConcurrentHashMap<String,String>` probe with the exact two entity
  names: now prints `Child, Root` (was `Root, Child`), matching HotSpot.
- 14-class regression sample from `passed.txt` (diverse: JAXB/StAX, bytecode
  enhancement/proxy generation, dialect tests): all still pass, 0 failures.
