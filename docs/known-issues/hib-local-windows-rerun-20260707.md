# Hibernate 121-class non-passed list — local Windows rerun (2026-07-07)

| | |
|---|---|
| **What** | 4-shard local rerun of the same 121-class HotSpot-pruned non-passed list used in the 2026-07-05 Azure sweep (`apps/hib-suite-runner/nonpassed.txt`), on a local Windows box instead of the Azure Linux host. |
| **Build** | `dev` merged to `d0a779f6` (branch `test/hib-local-rerun-20260706`, worktree `C:\craton\CratonVM-hib-local-rerun-20260706`, binary `cvhiblocal0706.exe`). |
| **Config** | real-JDK (`--java-home`, JDK 25.0.1), JIT on, `SHARDS=4`, `TIMEOUT=1200`. Wall time: 156m23s. |
| **Result** | `PASS=16 FAIL=75 CRASH=17 HANG=10 ABORTED=3` (of 121). |

## Why rerun the same list on a different host

The 2026-07-05 findings (bytecode-enhancement cluster, temporal-skew,
`JpaLargeBlobTest`, `InPredicateTest`, etc.) were all captured against `dev`
`49aaf713`. Between then and `d0a779f6` a large number of independent fix
branches landed (evidenced by ~20 parallel worktrees on this box targeting
exactly those findings). This rerun's purpose was to see how much of the
121-class list `dev`'s progress had already resolved, on a different
OS/environment as an incidental cross-check.

## 16 classes now genuinely fixed (confirmed `found == ok`)

```
annotations.embeddables.collection.xml.EmbeddableWithOneToMany_HHH_11302_xml_Test
bytecode.enhancement.lazy.proxy.BidirectionalProxyTest
bytecode.enhancement.lazy.proxy.DeepInheritanceProxyTest
bytecode.enhancement.lazy.proxy.DeepInheritanceWithNonEntitiesProxyTest
bytecode.enhancement.lazy.LazyAbstractManyToOneNoProxyTest
engine.spi.EntityEntryTest
softdelete.SoftDeleteFetchModeTests
boot.jaxb.internal.stax.LocalXmlResourceResolverTest
annotations.enumerated.ormXml.OrmXmlEnumTypeTest
mapping.fetch.depth.NoDepthTests
annotations.fetchprofile.FetchProfileTest
cdi.lifecycle.ExtendedBeanManagerNotAvailableDuringTypeResolutionTest
serialization.CacheKeyEmbeddedIdEnanchedTest
annotations.configuration.ConfigurationTest
service.ClassLoaderServiceImplTest
```
This confirms real progress on the bytecode-enhancement/lazytoone cluster
([hib-bytecode-enhancement-loader-faithful-linking.md](hib-bytecode-enhancement-loader-faithful-linking.md)),
and confirms the "generic 120s-timeout wall" cluster
([hib-linux-fail-bucket-triage-20260703.md](hib-linux-fail-bucket-triage-20260703.md))
was indeed slowness/load-related rather than deterministic — several of
those classes (`ConfigurationTest`, `FetchProfileTest`, `OrmXmlEnumTypeTest`,
`LocalXmlResourceResolverTest`, `ExtendedBeanManagerNotAvailableDuringTypeResolutionTest`)
now pass outright.

**Not a fix — harness false positive:** `query.hql.FunctionTests` also shows
`PASS` in the raw TSV, but with `found=123 ok=0` — it never actually started
any test method; it's now blocked earlier by the new SQL-placeholder-duplication
bug (below) during fixture setup. `rerun.sh`'s status computation doesn't check
`ok == found`, so this reads as a false PASS. See the harness-gotcha note in
[hib-temporal-sql-parameter-placeholder-duplication.md](hib-temporal-sql-parameter-placeholder-duplication.md).

## Findings that evolved (same class, different/deeper symptom now)

- **`JpaLargeBlobTest`**: was `FAIL` (`NoSuchMethodError: java/lang/Object.read()I`,
  see the archived probe in `docs/internal` if still present) → now **`HANG`**
  (`rc=124`, ran the full 1200s). **Investigated 2026-07-07, see the "2026-07-07:
  fast-fail became a multi-hour non-hang" section of
  [hib-jpalargeblobtest-object-read-nosuchmethod.md](../internal/fixed-suite-bugs/hib-jpalargeblobtest-object-read-nosuchmethod.md):**
  not a blocking call — a `--stack-dump-on-timeout` probe against current
  `dev` shows the main thread genuinely still executing, stuck in neither a
  lock nor a native call, but grinding through the test fixture's own
  200 MiB byte-at-a-time `InputStream.read()` loop (2704 near-identical
  stack dumps in a 3s window, all at the same leaf frame, zero root-count
  growth). Both 2026-07-05/06 crash fixes removed the early aborts that used
  to mask this; the class was apparently never previously exercised to
  completion. Not a VM correctness regression — no fix landed, doc-only.
- **`InPredicateTest`**: was `FAIL` (`NullPointerException: ... "values" is null`)
  → now `FAIL` with a completely different symptom: `TimeoutException:
  testInPredicate(...) timed out after 120 seconds` (total `ms=608596` across
  the class). The null-values NPE appears fixed; a new slowness/hang-adjacent
  issue replaced it.
- **`type.temporal.LocalDateTimeTest` / `OffsetTimeTest`**: the 2026-07-05
  "every value off by exactly 1 hour" symptom
  ([hib-temporal-localdatetime-offsettime-one-hour-skew.md](hib-temporal-localdatetime-offsettime-one-hour-skew.md),
  if that doc still exists — it was lost from the main worktree by an
  unrelated concurrent `git` operation between sessions and needs
  re-creating) **no longer reproduces**. `LocalDateTimeTest` now fails on
  the SQL-placeholder-duplication bug before any round-trip completes
  (`ok=54 failed=36 aborted=72` — see
  [hib-temporal-sql-parameter-placeholder-duplication.md](hib-temporal-sql-parameter-placeholder-duplication.md));
  `OffsetTimeTest` now `HANG`s outright. The 1-hour-skew doc's data predates
  the 2026-07-06 temporal GC fix (`3240cb75`) that changed this code path —
  treat that doc as **superseded/stale** if recreated; the current blocker
  for both classes is the placeholder-duplication bug or a hang, not a
  skew.

## HANG cluster grew 3 → 10 — likely host load, not confirmed as new bugs

```
bytecode.enhancement.orphan.OrphanTest
id.enhanced.OptimizerConcurrencyUnitTest
type.temporal.OffsetTimeTest
type.temporal.ZonedDateTimeTest
boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
function.json.JsonArrayUnnestTest
joinedsubclassbatch.IdentityJoinedSubclassBatchingTest
annotations.xml.ejb3.Ejb3XmlElementCollectionTest
lob.JpaLargeBlobTest
type.temporal.OffsetDateTimeTest
```
This box had ~20-30 concurrent git worktrees with active builds/agent
sessions during this run (`git worktree list` showed dozens of in-progress
fix branches). Several of these classes took 90-140s just to complete
*successfully* elsewhere in this same run (e.g. `FetchProfileTest` at 137s,
`ConfigurationTest` at 95s) — under contention, marginal classes plausibly
tip over the 1200s ceiling without being genuinely broken. **Not
individually re-documented as new bugs** pending a clean, uncontended rerun;
flagging here so the count isn't mistaken for 7 new hangs.

## Not yet analyzed

The 17 CRASH and 75 FAIL entries were not individually triaged against the
2026-07-05 Azure baseline in this pass — this doc covers only the headline
deltas (newly-fixed, evolved, and the new placeholder-duplication bug) found
while validating the rerun. Full raw data: `apps/hib-suite-runner/out-hiblocal0706-20260707-032452/results.tsv`
(local, not committed — `apps/` is gitignored).
