# ShrinkWrap package scanning was ~6x HotSpot because `getResource` walked the whole classpath — FIXED 2026-09-02

Retired from `docs/known-issues/quarkus/`, where it stood as *"Not a hang:
`TestResourceManager.start()` / ShrinkWrap package scanning is ~3-4x HotSpot,
not stuck"*. The "not a hang" half was right and is kept below. The other half
— **"likely the same VM-wide per-call throughput ceiling tracked elsewhere"** —
was a guess the page itself flagged as unbisected, and it was wrong. The cost
was a specific defect in `ClassLoader.getResource`, and it is fixed.

## What the page got right, and keeps

A `--stack-dump-on-timeout` capture showed the main thread **running, not
blocked**, in

```
io/quarkus/test/common/TestResourceManager.start
  <- io/quarkus/test/AbstractQuarkusExtensionTest.beforeAll
  <- ...ContainerBase.addPackages -> URLPackageScanner.scanPackage
     -> ...foundClass -> ClassLoaderAsset.<init>
```

`QuarkusTestProfileAwareClassOrderer.orderClasses` early-returns for a
single-class run; the log line naming it just happens to print before the slow
step. Nothing here changes that. The three `io.quarkus.aesh.deployment` classes
finished in 43-57 s isolated against HotSpot's ~14 s, and the `NOSTART` in the
full-suite rerun was the harness's fixed per-class cap under contention.

## What it got wrong

> the same general shape (broad reflection / many small allocations / many
> virtual dispatches) already characterized as CratonVM's worst-case relative
> to HotSpot [...] Not bisected to confirm it's the *identical* mechanism, but
> the shape strongly matches; no new hypothesis is proposed here.

The bisect the page skipped is one control arm, and it refutes the claim
immediately. Reduce the named chain to a probe
(`apps/quarkus-suite-runner/probes/ShrinkWrapScanProbe.java`) with four arms —
the scan itself, the `getResource` primitive under it, a `getResource` MISS,
and a plain allocation-plus-dispatch loop that touches no classloader — and run
all three arms interleaved, three rounds, on one host:

| arm | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| ShrinkWrap scan, 185 assets (ms) | 88 / 95 / 89 | 515 / 614 / 583 | **232 / 246 / 242** |
| `getResource` HIT (µs/call) | 329 / 322 / 331 | 1821 / 2110 / 2228 | **452 / 429 / 429** |
| `getResource` MISS (µs/call) | 2029 / 2009 / 2069 | 2927 / 3258 / 3064 | 3154 / 3124 / 3143 |
| alloc+dispatch CONTROL (ms) | 7 / 15 / 16 | 2462 / 2657 / 2600 | 2338 / 2685 / 2595 |

**The control arm is ~200x HotSpot. The scan was 6x.** A workload governed by
the generic per-call ceiling cannot be 6x while that ceiling is 200x — so
whatever the scan was paying, it was not that. (The control is a
`StringBuilder`/`ArrayList` loop, a shape HotSpot's JIT optimises very
aggressively, so 200x overstates the true ceiling. It does not matter: the
argument needs only that the control does not behave like the scan, and it
does not, by two orders of magnitude.)

The two arms that DO move are the two the fix touches. The two that must NOT
move — the MISS, which has to walk every entry either way, and the control —
do not. That is the whole bisect, and it is three lines of probe.

## The defect

`ClassLoaderAsset.<init>` is `classLoader.getResource(name)`, so a package scan
is one `getResource` per discovered class. `cl_get_resource` did:

```rust
let urls = ctx.find_all_resource_urls(resource_name);   // every entry
let url_str = if let Some(first) = urls.first() { ... } // element 0
```

It built every matching URL across the whole classpath and returned element 0.
HotSpot's `getResource` stops at the first hit. On the quarkus harness
classpath — **309 entries, 265 jars and 44 directories** — a name that hits in
entry 4 still cost the remaining 305: a hash probe per archive, and per
DIRECTORY an `exists()` plus a `canonicalize`, i.e. filesystem syscalls, on
every call.

`Class.getResource` (`lang_class.rs`) had the identical shape. Both are fixed
through one shared helper, because a fix to one of two sibling doors is a fix a
bisect can miss on the other.

**The short-circuiting walk already existed.** `ClassPath::next_resource_url_from`
was written for the lazy `getResources` enumeration in this directory's
`investigate-batch-01-FIXED.md` (Defect 1, the 136x `statx`/`readlink`
finding), along with the `name_supports_incremental_scan` gate for glob names,
which can match several times inside ONE entry and so must not take the
first-hit path. The singular doors had simply never been wired to it. Nothing
new was built here.

## Verification

* `the_incremental_walk_returns_what_the_whole_list_walk_returns_first`
  (`classloading/src/class_path.rs`) asserts the equivalence the fix rests on,
  on a fixture where the name hits in TWO entries and the first hit is not the
  last entry — a single-hit fixture cannot tell "returns the first" from
  "returns the only one", and a last-entry hit cannot tell "stopped early" from
  "walked everything". It pins the glob gate too.
* `CRATONVM_GETRESOURCE_FIRST_HIT=0` restores the whole-list walk. Every number
  in the table above is that one flag on one binary, so the comparison carries
  no cross-binary or cross-commit confound.
* `scan.assets=185` in every arm: the fix changes what it COSTS to find the
  resources, never which ones are found.
* `cargo test -p cratonvm-native-builtins --lib` 4194 pass;
  `-p cratonvm-classloading --lib` 802 pass; `regression-suite/run.sh` 81/81;
  `cargo test -p cratonvm-types` flag guards pass with the switch declared and
  the two generated flag docs regenerated.
* Quarkus core suite, 313 classes, same binary, flag on vs off: four classes
  differ and **none is attributable** — run isolated with the flag both ways,
  each of the four answers identically in 8 s
  (`DependencyPresentTwiceInTheGraphWithDifferentClassifierAndTypeTestCase`,
  `DirectDependencyOverridesManagedDependencyTestCase`,
  `DirectUserDepsOverrideTransitiveExtDepsTest`,
  `DependencyInProfileActiveByDefaultEffectiveModelBuilderTest`). They are
  300 s cap crossings under load, and the direction is mixed — three one way,
  one the other — which a real regression would not be. The two arms took
  19m32s and 16m45s of wall clock for identical work; Overwatch and other
  sessions' builds ran throughout.

## What remains, honestly

**2.6x, and now it really is the systemic per-call cost.** After the fix the
primitive is at parity (429 µs against HotSpot's 322-331, i.e. 1.3x) while the
scan is still 2.6x. Of CratonVM's ~240 ms, about 80 ms is `getResource`
(185 × 429 µs); on HotSpot about 60 ms of 90 ms is. So the residual is roughly
160 ms of CratonVM against 30 ms of HotSpot for the same ordinary Java — the
`URLPackageScanner` walk, ShrinkWrap's per-path-segment archive bookkeeping,
and the name mangling around them. That IS the ceiling
`docs/known-issues/perf/perf-bintrees-9x-gap-characterised.md` tracks, it is now the dominant
term, and it is tracked there. What was never true is that it accounted for the
6x.

**The `found=1 started=0` outcome** the page recorded on both VMs and left
unexplained is a harness-scoring question, not a VM one, and this directory
already owns it: `investigate-batch-01-FIXED.md`'s Defect 2 ends with *"Any
quarkus PASS with `started=0` is worth re-reading as unknown, not green"* —
`found>0 && failed==0` scores "an extension blew up in `beforeAll`, nothing
ran" as a pass. Checked against the 313-class core suite as it stands today:
**zero PASS rows have `found>0` with `ok=0`**, so the local corpus is currently
clean of that shape. The three `aesh` classes that showed it are
extension-module classes this harness does not build, so they could not be
re-checked here.

**The harness mitigations the page proposed** — a per-class timeout override
for `TestResourceManager`-heavy classes, and less parallelism — are not needed
for this cause any more and were not added. The cap crossings that remain on
this host are load, and a timeout override tuned against a machine running a
game would be a worse lie than the one it replaced.

## Related

* `investigate-batch-01-FIXED.md` — same directory. Its Defect 1 built the
  incremental walk this fix finally wired the singular doors to, and its
  Defect 2 owns the `started=0` scoring rule.
* `docs/known-issues/perf/perf-bintrees-9x-gap-characterised.md` — the per-call ceiling, which
  the residual 2.6x really is and the original 6x really was not.
* The jboss-logmanager `getLogger`-cast report and
  `jboss-logcontextinitializer-spi-not-consulted-20260901-FIXED.md`, both in
  this directory — the rest of the quarkus bootstrap chain this page belonged
  to, closed 2026-09-01.
