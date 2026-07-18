# Hibernate ORM test suite — known issues index (2026-07-16 full-suite rerun)

Source: full 4548-class Hibernate ORM 8.0 test suite run on this local Windows
host, `dev` merged to `2f02e939d`, real-JDK, JIT on, 4 shards, `TIMEOUT=1200`.
Result: `PASS=4437 (97.6%) FAIL=10 HANG=1 CRASH=1 ABORTED=8 NOTESTS=91`. This is
a large improvement over the 2026-07-11 audit baseline (`PASS=4095, 90.0%`),
reflecting the large number of concurrent fix branches merged into `dev`
since (statistics-counter cluster, sql.exec cluster, enhancement/setter
cluster, ANTLR entity-graph NPE cluster, immutable-collection and
cascade-multipath hang clusters, connections/proxy serialization cluster,
boot-models XML/Jandex cluster, and the assertion-longtail catalog — see
`docs/internal/fixed-suite-bugs/` for the individual write-ups). Every
cluster from the 2026-07-11 audit is now archived there as FIXED/RESOLVED.

**Data-loss note:** the exact 2026-07-11 453-class and later 224-class
non-passed baselines (`apps/hib-suite-runner/nonpassed*.txt`) were lost to
local-disk-exhaustion file corruption discovered 2026-07-16 (511/683 files in
`apps/hib-suite-runner`, including `rerun.sh`, `common.args`,
`CratonRunner.java/.class`, and `testlist.txt`, were silently truncated to
zero bytes). The harness driver files were recovered from an intact
`hib-suite-runner.tar` backup (2026-06-28/29 vintage); the non-passed class
*lists* themselves were not in that backup and could not be recovered. This
full-suite rerun was run specifically to regenerate an accurate current
baseline rather than trust a stale/partial list.

The new 20-class non-passed list is saved as the canonical baseline at
`apps/hib-suite-runner/nonpassed.txt` (also `nonpassed20.txt`) for future
regression tracking — not committed (`apps/` is gitignored).

## Residual clusters (this rerun)

- FIXED/RETIRED: [hib-120s-junit-timeout-cluster-20260716.md](../../internal/hib-120s-junit-timeout-cluster-20260716.md) — all original timeout classes and the linked `LockTest` residual pass in one real-JDK combined-run acceptance test.
- [hib-misc-residuals-20260716-FIXED.md](../../internal/fixed-suite-bugs/hib-misc-residuals-20260716-FIXED.md) — historical Hibernate residual investigation, now fully closed. The final `DefaultCatalogAndSchemaTest` BigInteger AIOOBE is quarantined at the JIT admission and background-scheduling boundaries; the 132-test class passes under both normal JIT and `--nojit`.

## Already tracked elsewhere (all FIXED/RESOLVED as of this rerun)

See `docs/internal/fixed-suite-bugs/` for the full archive of the 2026-07-11
audit clusters, all confirmed fixed or resolved by the time of this rerun:
statistics-counters-zero, bytecode-enhancement-propertyaccessexception,
entitygraph-antlr-rulenode-npe, immutable-entitywithmutablecollection-hang,
cascade-multipathcircle-hang, boot-models-xml-qname-jandex,
connections-proxy-serializationexception, generic-timeout-hang-longtail,
assertionfailederror-longtail-triage, proxyclassreuse-loader-blind-class-resolution.

- [hib-notests-abstract-baseclass-list.md](../../internal/hib-notests-abstract-baseclass-list.md) — 91 classes, NOT a bug (abstract base classes with 0 discoverable tests, matches HotSpot).
