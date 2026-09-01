# Not a hang: `TestResourceManager.start()` / ShrinkWrap package scanning is ~3-4x HotSpot, not stuck

**Status: characterized, not fixed — likely the same VM-wide per-call
throughput ceiling tracked elsewhere.** Investigated 2026-08-17 (Azure host,
dev base `496bc3c2c` + the jboss-logmanager `getLogger`-cast fix). This
corrects that report's "what this fix exposed next" framing — it is **not** a
hang at `QuarkusTestProfileAwareClassOrderer`.

## What actually happens

Once the Logger-cast bug was fixed, several previously-`NOSTART` classes
(`io.quarkus.aesh.deployment.CliConfigPromptTest`,
`CommandBeanRegistrationTest`, `CliSettingsCustomizerTest`) still showed
`NOSTART` in the full-suite rerun, timing out at the harness's 180s cap. The
JUnit5 bootstrap log's last line before going quiet was the class orderer
announcement (`Using default class orderer
'io.quarkus.test.junit.util.QuarkusTestProfileAwareClassOrderer'`), which
looked like the hang site — but a `--stack-dump-on-timeout 60` capture
showed the main thread **running, not blocked**, with its last recorded
interpreter dispatch point at:

```
io/quarkus/test/common/TestResourceManager.start
  <- io/quarkus/test/AbstractQuarkusExtensionTest.beforeAll
  <- ...ContainerBase.addPackages -> URLPackageScanner.scanPackage
     -> ...foundClass -> ClassLoaderAsset.<init>
```

`QuarkusTestProfileAwareClassOrderer.orderClasses` itself early-returns
immediately for a single-class run (`classDescriptors.size() <= 1`) — the
log line just happens to print right before this genuinely-slow step, not
at the actual cost.

## It finishes — just slowly, and inconsistently under load

Isolated, uncontended reruns of the same three classes (`--shards 1`, no
other GC variant or shard running concurrently):

| class | CratonVM (isolated) | HotSpot |
|---|---:|---:|
| `CliConfigPromptTest` | 48.0s | 13.7s |
| `CommandBeanRegistrationTest` | 56.7s | not measured |
| `CliSettingsCustomizerTest` | 43.4-43.6s | not measured |

All three finish in under a minute alone — roughly **3-4x HotSpot**, not
infinite. All three also produce the identical, correct outcome on both
VMs: `found=1 started=0 failed=0 aborted=0`, no exception logged on either
side (an unexplained-but-VM-agnostic silent skip, not itself a defect —
not investigated further here). The 180s `NOSTART`/timeout seen in the full
6,377-class 2-shard rerun was **host contention** (many classes/shards
competing for CPU) pushing an inherently ~4x-slower-than-HotSpot but
finite workload over the harness's fixed per-class budget, not a defect
that blocks forward progress.

## Likely the same systemic cost as elsewhere, not a new one

`URLPackageScanner.scanPackage` walks a classpath package's classfiles
reflectively (`ContainerBase.addPackages` → `addPackage` → `scanPackage`,
building a ShrinkWrap archive one discovered class at a time) — the same
general shape (broad reflection / many small allocations / many virtual
dispatches) already characterized as CratonVM's worst-case relative to
HotSpot in `perf-bintrees-9x-gap-characterised.md` and the netty
`AdaptiveByteBufAllocator*` throughput-wall docs. Not bisected to confirm
it's the *identical* mechanism, but the shape strongly matches; no new
hypothesis is proposed here.

## What would actually help

No VM fix is proposed by this doc — the systemic per-call throughput gap is
already tracked and unresolved. Harness-level mitigations that would stop
this from reading as `NOSTART`/hang in a full-suite run:
- A longer per-class timeout specifically for `TestResourceManager`-heavy
  classes (mirrors the `CLASS_TIMEOUT_OVERRIDE` mechanism already present
  in `run-quarkus-suite.sh` for other slow classes).
- Less parallelism (fewer concurrent shards/collectors) so the ~4x-slower
  baseline doesn't also have to compete for CPU.

## Related

- The jboss-logmanager `getLogger`-cast report this investigation continued
  from — retired to the non-public archive on 2026-09-01, once the residual it
  had left open (the JBoss `Logger` face was a stub set, so the object that
  fix started handing back could not hold a handler or a level) was closed.
  The correction on this page is what replaced that page's "what this fix
  exposed next" section, so there is nothing left there to read against it.
- `jboss-logcontextinitializer-spi-not-consulted-20260901.md` — the one
  logging divergence from that closure that is still open.
- `perf/perf-bintrees-9x-gap-characterised.md` — the canonical
  characterization of the VM-wide per-call throughput ceiling this likely
  shares a cause with.
