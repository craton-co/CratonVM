# hibernate-reactive — investigate batch 03 of 6

> **STALE AS A DEFECT LIST (2026-08-12).** Both blockers this list was collected behind — the SASL/SCRAM handshake failure and the JNA `Native.<clinit>` NPE — were root-caused and fixed the same day, and a 40-class rerun of the head of `testlist.txt` on the Azure host afterwards recorded PASS=39 NOTESTS=1 with no `before()` timeout at all. Regenerate from a full run before working these classes one by one; see `docs/internal/fixed-suite-bugs/hibernate-reactive/testcontainers-jackson-jit-stall-blocks-eventloop-20260812-FIXED.md`.

**No investigation done — class names and repro only.** Part of a 71-class FAIL/HANG list split across 6 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 12 classes below — do not touch classes listed in other batch pages.

Found during a **partial** 3-GC-variant (default/G1/ZGC) PostgreSQL run on Azure host `azureuser@20.80.105.49` (stopped early after ~15-28min once a dominant blocker was identified — see `docs/internal/fixed-suite-bugs/vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md`). **Most of these classes are LIKELY hitting that same SASL/SCRAM handshake bug** (it blocks session-open for virtually every DB-required test), but that has NOT been confirmed per-class — each class here still needs its own check to rule out a distinct, unrelated defect hiding behind the dominant one. "status seen" reflects what each GC variant's partial run actually recorded before it was stopped; the 3 variants did not all reach the same point in the class list (G1 got further, ~77 classes attempted vs ~28-29 for default/zgc), so absence from a variant's column means "not reached," not "passed." A class showing both FAIL and HANG across variants (`FAIL/HANG`) is raw observed flakiness near the run's stop point, not yet explained. Also set `DOCKER_HOST=unix:///var/run/docker.sock` before reproducing (works around a separate, already-documented JNA NPE in Testcontainers' Docker-strategy probe — `docs/internal/fixed-suite-bugs/jna-native-clinit-nativeversion-npe-20260812-FIXED.md` — without it you'll hit that bug instead of reaching these classes at all).

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `org.hibernate.reactive.EagerElementCollectionForEmbeddableEntityTypeMapTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EagerElementCollectionForEmbeddableTypeListTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EagerElementCollectionForEmbeddedEmbeddableMapTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EagerElementCollectionForEmbeddedEmbeddableTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EagerManyToOneAssociationTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EagerOneToManyAssociationTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EagerOneToOneAssociationTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EagerOrderedElementCollectionForEmbeddableTypeListTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EagerTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EagerUniqueKeyTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EmbeddedIdTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.EmbeddedIdWithManyEagerTest` | FAIL | g1=FAIL |

## Repro

```bash
cd apps/hibernate-reactive-suite-runner   # on azureuser@20.80.105.49, /data/cratonvm
export DOCKER_HOST=unix:///var/run/docker.sock
./cratonvm-hibreactive-default-wrapper.sh @common.args -Dcraton.batch=1 CratonRunner <ClassName>
# swap the -default- wrapper for -g1- / -zgc- to match the variant(s) that showed the failure
# HotSpot cross-check: run the same class/classpath under stock HotSpot (JDK 25) with the same DOCKER_HOST set
```

