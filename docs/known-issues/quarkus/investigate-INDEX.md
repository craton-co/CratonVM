# quarkus — FAIL/HANG classes needing investigation (index)

**7 unique classes** (union of FAIL/HANG across all 3 GC-variant runs, 2026-08-12), split into 1 pages of up to 15 classes each so multiple people can pick up a page without duplicating work. **No investigation done here** — this is a raw class list plus a repro command template; each page's classes are its own to investigate, not shared with any other page.

Found during a **partial**, STOPPED-early 3-GC-variant (default/G1/ZGC) full-scope run on Azure host `azureuser@20.80.105.49` (~6,377-class harness, 2 shards/variant, stopped deliberately after ~12hrs / ~14% coverage to free host resources -- see `docs/known-issues/quarkus/run-checkpoint-20260812.md` for exact per-shard stop points and how to resume the remaining ~5,549 classes). PASS rate on the ~2,645 classes actually attempted was 99.5%+ -- this is a SMALL, not-yet-representative sample: only 7 unique classes hit HANG across all 3 variants combined, and zero hit FAIL. Do not read "only 7 classes" as "quarkus is nearly clean on CratonVM" -- ~5,549 of 6,377 classes were never reached.

## Pages

- [batch 01](investigate-batch-01.md) — 7 classes (io.quarkus.aesh.deployment.CommandBeanRegistrationTest .. io.quarkus.bootstrap.resolver.maven.test.ProxyAndMirrorSettingsReposTest)
