# ES fragile cluster (`is_elasticsearch_suite_jit_fragile_cluster`, whole `org/elasticsearch/`) — CONFIRMED still needed (correction)

**Status: ban kept, re-verified live with a real Elasticsearch 9.6.0-SNAPSHOT
checkout. This corrects `docs/known-issues/es-fragile-cluster-no-fixture-20260726.md`
(same session, earlier), which incorrectly concluded no ES fixture exists
on this host — one does, it was simply not found by the initial search.**

## Fixture found

`/data/data/es-fixture-ivfknn-slicesdense-closure-20260717/` — a complete,
real Elasticsearch git checkout (server module compiled, 2555 real
`*Tests` classes in `server/build/classes/java/test/`, `test/framework`
module compiled with `ESTestCase` etc., and the native `libvec.so` fixture
already built at `lib/platform/linux-x64/libvec.so`). A sibling checkout,
`es-fixture-ivfknn-lucene104-closure-20260717/`, also exists. Both were
missed by the initial `find -iname '*elasticsearch*'` sweep at the start
of this session because they're named after the specific investigation
they were built for (`es-fixture-ivfknn-*`), not "elasticsearch" itself —
worth remembering: **search by content/purpose, not just by expected name,
when a prior "no fixture" claim needs re-checking.**

Repro recipe (per `[[es-suite-adhoc-junitcore-repro-recipe]]` memory, one
correction: `server/build/craton-testcp.txt` omits the `test/framework`
module's own classes because its jar was never built — prepend
`test/framework/build/classes/java/main` and
`test/framework/build/resources/main` manually):

```bash
ES=/data/data/es-fixture-ivfknn-slicesdense-closure-20260717
CP="$ES/test/framework/build/classes/java/main:$ES/test/framework/build/resources/main:$(tr -d '\r' < "$ES/server/build/craton-testcp.txt" | tr '\n' ':' | sed 's/:$//')"
<cratonvm-binary> --java-home /home/victor/jdk25 -Dtests.seed=<seed> -Dtests.asserts=false \
  -Des.path.home="$ES" -Djava.awt.headless=true -cp "$CP" \
  org.junit.runner.JUnitCore <fully.qualified.Tests>
```

## Result

Ran an 18-class spread sample (every ~150th class across the whole
compiled test set, covering `lucene`, `index`, `common`, `cluster`,
`search`, `rest`, `action`, `health`, `snapshots` packages) baseline vs.
`CRATONVM_JIT_ALLOW_PACKAGES=org/elasticsearch/`:

| Class | Baseline | Lifted |
|---|---|---|
| `index.mapper.blockloader.FloatFieldBlockLoaderTests` | 38/120 failed | **41/120 failed** |
| (all other 17 classes) | identical pass/fail/hang counts | identical |

16 of 18 classes are completely unaffected by lifting the ban (including
2 pre-existing hangs, `TextFieldMapperTests` and
`ComposableIndexTemplateTests`, that hang identically either way — not
new). But `FloatFieldBlockLoaderTests` gets **3 additional failures**
under JIT that are not present in the interpreted baseline — a real,
reproducible regression from lifting this ban, even in a modest sample.

## Disposition

**KEEP `is_elasticsearch_suite_jit_fragile_cluster` (`org/elasticsearch/`)
banned.** Confirmed via real testing, not synthetic repro. Given the
package covers 2555+ test classes and only an 18-class sample was run
this session, the true failure surface under JIT is very likely larger
than the one class found here — this sample should be treated as a lower
bound, not a full characterization. A future session with more time
should: (1) widen the sample size significantly now that a working real
fixture recipe exists, (2) drill into exactly which `FloatFieldBlockLoaderTests`
methods regress and why (a `blockloader`-family bug, possibly related to
value-loading/decoding under JIT), (3) decide whether the ban could be
narrowed to something smaller than the whole `org/elasticsearch/` prefix
once the specific corrupting method(s) are identified.

## Related

- Corrects: `docs/known-issues/es-fragile-cluster-no-fixture-20260726.md`
  (same session) — that doc's "no fixture" premise was wrong; keeping it
  in place with a pointer to this doc rather than deleting it, per this
  repo's convention of not silently erasing a prior claim.
- `docs/known-issues/repros/jitban-remaining-20260726/es-test-list-sample.txt`
  — the 18-class sample list used, for reproducibility.

## SUPERSEDED 2026-07-27 — ban REMOVED

The single measurement that kept this ban — `FloatFieldBlockLoaderTests` going
from 38 to 41 failures with the package JIT-eligible — was a symptom of a VM
defect, not of ElasticSearch code. `try_jit_compile_callee` returned a bare
compiled-entry address and dropped its `Arc<CompiledMethod>`, so a concurrent
tier-up `JitCache::put` could unmap the body before the caller called, cached or
baked that address. The same defect was SIGSEGV-ing
`cluster.NodeConnectionsServiceTests` at 4/20 runs in the same window; see
`docs/internal/nodeconnections-retired-jit-code-jump-20260727.md`.

Re-measured on the fixed build, ban-on vs ban-lifted:

- the 19-class spread sample (every ~150th compiled test class, plus
  `FloatFieldBlockLoaderTests`) — **identical line for line**;
- an 8-class second sample over the vector/codec classes that originally
  motivated the ban — identical except for two classes that were re-run
  uncontended, 3 runs each way (below);
- `FloatFieldBlockLoaderTests` — 120 tests, **31 failures both ways** (down
  from 38 on 2026-07-26 through unrelated work since);
- `TextFieldMapperTests` — 149 tests, 24 failures, on **3 runs each way**,
  uncontended (348–581 s per run; the "hang" seen in the contended sample was
  simply the 600 s cutoff);
- `FloatHierarchicalKMeansTests` — **the ban was causing a hang.** With the ban
  ON it times out 3/3 at 900 s; with it lifted it completes in 43–58 s, 9 tests
  / 7 failures, 3/3. Lifting removes a hang here rather than introducing one.
  (The 7 failures are present in both the contended sample and these runs and
  are not new; they were simply unobservable while the class hung.)

The ban's own recommendation — "the true failure surface is likely larger, not
yet fully characterized" — still holds in the sense that only ~27 of 2555
classes were sampled. What changed is that the one measured regression is gone
and none of the sampled classes now differ, so the blanket ban no longer has
evidence behind it.
