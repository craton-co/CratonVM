# Tomcat suite — known issues index

Split out of `docs/internal/fixed-suite-bugs/tomcat/18-fixture-environment-gaps-20260724.md`
(2026-07-24) into one file per independently-actionable item, so different
sessions can pick separate items up in parallel without stepping on each
other. Source data and the full 35-class categorization with root-cause
detail: that doc, plus `apps/tomcat-suite-runner/RESULTS-20260724-cwdfix.md`
on the Azure host. All of this is against the Linux Tomcat fixture at
`/data/data/tomcat-dohead-fixture-20260717` (symlinked
`/data/data/apps/tomcat`), reusable Linux runner at
`apps/tomcat-suite-runner/run-tomcat-suite.sh`.

## Fixture-completion work — ALL 6 IMPLEMENTED 2026-07-23

| Doc | Classes | Outcome |
|---|---:|---|
| [missing-antjar-classpath.md](missing-antjar-classpath.md) | 2 | ✅ Fixed — 1 PASS both, 1 revealed a real regression |
| [missing-httpd-binary.md](missing-httpd-binary.md) | 8 | ✅ Fully fixed — all 8 PASS both VMs, no regressions |
| [largeheap-flat-heap-oom.md](largeheap-flat-heap-oom.md) | 3 | ⚠️ Partial — 2 now PASS HotSpot/reveal regressions, 1 still fails both (narrower) |
| [missing-catalina-localhost-context-configs.md](missing-catalina-localhost-context-configs.md) | 8 | ⚠️ Root-cause theory was wrong (see doc) — real fix was the lib-jars doc below; 2 PASS both, 6 revealed regressions |
| [missing-build-lib-jars.md](missing-build-lib-jars.md) | 1 | ✅ Fixed via `ant deploy` — also fixed most of the "conf/Catalina/localhost" bucket above |
| [unbuilt-virtual-webapp-submodule.md](unbuilt-virtual-webapp-submodule.md) | 1 | ⚠️ Root-cause theory was wrong (no Maven module) — one method now passes, a second method reveals a narrower regression |

As predicted, completing these turned several into real CratonVM
regressions rather than clean passes — see
[regressions-revealed-by-fixture-completion-20260723.md](regressions-revealed-by-fixture-completion-20260723.md)
for the full accounting: **12 PASS both VMs / 9 confirmed CratonVM-only
regressions / 2 still fail on both (different, narrower reasons than
originally documented)**. One regression was root-caused precisely: a `%20`
in a file path isn't decoded back to a space when CratonVM resolves a
`file:` URL, breaking `TestDeployTask`.

Note: none of this fixture work is git-tracked — it all lives on the Azure
host (`/data/data/apps/tomcat`, i.e. `/data/data/tomcat-dohead-fixture-20260717`).
A future session rebuilding this fixture from scratch needs to redo these
steps (see each doc's "RESOLVED" note for the exact commands).

## Needs investigation, not yet root-caused

| Doc | Classes |
|---|---:|
| [untriaged-oddities.md](untriaged-oddities.md) | 3 |
| [hang-classification-unconfirmed-host-contention.md](hang-classification-unconfirmed-host-contention.md) | 9 |

## Real CratonVM bug (not a fixture gap — despite living in the same 35-class "both VMs fail" bucket) — FIXED

The `value_stack.rs` `usize`-underflow panic on a background NIO worker
thread (`TestNonBlockingAPI` / `TestWebSocketFrameClientSSL`, both hitting
`LinkedBlockingQueue.take()`'s `Condition.await()` interface dispatch) was
root-caused and fixed — a missing pre-pop deopt-frame snapshot in
`jit/src/x64.rs`'s generic invoke-dispatch codegen. See
`docs/internal/fixed-suite-bugs/tomcat/value-stack-usize-underflow-nio-worker-panic-FIXED.md`.
`TestNonBlockingAPI`'s separate, unrelated HotSpot-shared failure is tracked
in [untriaged-oddities.md](untriaged-oddities.md).

Total accounted for: 6 + 3 = 9 docs covering all 35 non-PASS classes from
the "true fixture gap" bucket, plus the 2 classes where the panic used to
reproduce (now fixed, folded into the count above via `untriaged-oddities.md`
picking up `TestNonBlockingAPI`'s residual HotSpot-shared issue).
