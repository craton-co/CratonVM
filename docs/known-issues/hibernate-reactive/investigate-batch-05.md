# hibernate-reactive — investigate batch 05 of 6

**No investigation done — class names and repro only.** Part of a 71-class FAIL/HANG list split across 6 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 12 classes below — do not touch classes listed in other batch pages.

Found during a **partial** 3-GC-variant (default/G1/ZGC) PostgreSQL run on Azure host `azureuser@20.80.105.49` (stopped early after ~15-28min once a dominant blocker was identified — see `docs/known-issues/hibernate-reactive/vertx-pg-sasl-scram-handshake-fails-20260812.md`). **Most of these classes are LIKELY hitting that same SASL/SCRAM handshake bug** (it blocks session-open for virtually every DB-required test), but that has NOT been confirmed per-class — each class here still needs its own check to rule out a distinct, unrelated defect hiding behind the dominant one. "status seen" reflects what each GC variant's partial run actually recorded before it was stopped; the 3 variants did not all reach the same point in the class list (G1 got further, ~77 classes attempted vs ~28-29 for default/zgc), so absence from a variant's column means "not reached," not "passed." A class showing both FAIL and HANG across variants (`FAIL/HANG`) is raw observed flakiness near the run's stop point, not yet explained. Also set `DOCKER_HOST=unix:///var/run/docker.sock` before reproducing (works around a separate, already-documented JNA NPE in Testcontainers' Docker-strategy probe — `docs/known-issues/hibernate-reactive/jna-native-clinit-nativeversion-npe-20260812.md` — without it you'll hit that bug instead of reaching these classes at all).

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `org.hibernate.reactive.GeneratedPropertyUnionSubclassesTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.HQLQueryParameterNamedLimitTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.HQLQueryParameterNamedTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.HQLQueryParameterPositionalLimitTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.HQLQueryParameterPositionalTest` | HANG | g1=HANG |
| `org.hibernate.reactive.HQLQueryTest` | HANG | g1=HANG |
| `org.hibernate.reactive.HQLUpdateQueryTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.IdentifierGenerationTypeTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.IdentityGenerationWithBatchingTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.IdentityGeneratorDynamicInsertTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.IdentityGeneratorTest` | FAIL | g1=FAIL |
| `org.hibernate.reactive.IdentityGeneratorTypeTest` | HANG | g1=HANG |

## Repro

```bash
cd apps/hibernate-reactive-suite-runner   # on azureuser@20.80.105.49, /data/cratonvm
export DOCKER_HOST=unix:///var/run/docker.sock
./cratonvm-hibreactive-default-wrapper.sh @common.args -Dcraton.batch=1 CratonRunner <ClassName>
# swap the -default- wrapper for -g1- / -zgc- to match the variant(s) that showed the failure
# HotSpot cross-check: run the same class/classpath under stock HotSpot (JDK 25) with the same DOCKER_HOST set
```

