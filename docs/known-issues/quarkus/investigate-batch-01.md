# quarkus — investigate batch 01 of 1

**No investigation done — class names and repro only.** Part of a 7-class FAIL/HANG list split across 1 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 7 classes below — do not touch classes listed in other batch pages.

Found during a **partial**, STOPPED-early 3-GC-variant (default/G1/ZGC) full-scope run on Azure host `azureuser@20.80.105.49` (~6,377-class harness, 2 shards/variant, stopped deliberately after ~12hrs / ~14% coverage to free host resources -- see `docs/known-issues/quarkus/run-checkpoint-20260812.md` for exact per-shard stop points and how to resume the remaining ~5,549 classes). PASS rate on the ~2,645 classes actually attempted was 99.5%+ -- this is a SMALL, not-yet-representative sample: only 7 unique classes hit HANG across all 3 variants combined, and zero hit FAIL. Do not read "only 7 classes" as "quarkus is nearly clean on CratonVM" -- ~5,549 of 6,377 classes were never reached.

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `io.quarkus.aesh.deployment.CommandBeanRegistrationTest` | HANG | g1=HANG |
| `io.quarkus.aesh.deployment.CommandExceptionExitCodeTest` | HANG | g1=HANG |
| `io.quarkus.aesh.deployment.CommandExecutionListenerTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.quarkus.aesh.deployment.CommandFailureExitCodeTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.quarkus.arc.test.unproxyable.RequestScopedFinalMethodsTest` | HANG | g1=HANG |
| `io.quarkus.bootstrap.resolver.maven.test.PomProfileReposEffectivePomTest` | HANG | zgc=HANG |
| `io.quarkus.bootstrap.resolver.maven.test.ProxyAndMirrorSettingsReposTest` | HANG | default=HANG |

## Repro

```bash
# on azureuser@20.80.105.49, /data/cratonvm/apps/quarkus-suite-runner
echo <ClassName> > /tmp/one.txt
./run-quarkus-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 180 --out /tmp/repro \
  --bin /data/cratonvm/target-zgc/release/cratonvm-quarkus-default-wrapper.sh
# swap the -default- wrapper for -g1- / -zgc- to match the variant(s) that showed the failure
# HotSpot cross-check: run the same class/classpath under stock HotSpot (JDK 25)
```

