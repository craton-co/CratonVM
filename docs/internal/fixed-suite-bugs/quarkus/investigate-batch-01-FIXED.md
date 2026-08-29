# quarkus — investigate batch 01 of 1 — FIXED 2026-08-13

Retired from `docs/known-issues/quarkus/`. All seven classes now PASS on
CratonVM on all three GC variants, and the two that carry real test methods
return the same counts HotSpot does.

## What the page reported

Seven classes seen as HANG at the harness's flat 180 s per-class cap during
the partial 3-GC-variant full-scope run of 2026-08-12 (see
docs/known-issues/quarkus/run-checkpoint-20260812.md). No investigation had
been done — the page was a class list plus a repro template.

| class | reported | now |
|---|---|---|
| `io.quarkus.aesh.deployment.CommandBeanRegistrationTest` | g1=HANG | PASS ×3 |
| `io.quarkus.aesh.deployment.CommandExceptionExitCodeTest` | g1=HANG | PASS ×3 |
| `io.quarkus.aesh.deployment.CommandExecutionListenerTest` | HANG ×3 | PASS ×3 |
| `io.quarkus.aesh.deployment.CommandFailureExitCodeTest` | HANG ×3 | PASS ×3 |
| `io.quarkus.arc.test.unproxyable.RequestScopedFinalMethodsTest` | g1=HANG | PASS ×3 |
| `io.quarkus.bootstrap.resolver.maven.test.PomProfileReposEffectivePomTest` | zgc=HANG | PASS ×3, ok=1 |
| `io.quarkus.bootstrap.resolver.maven.test.ProxyAndMirrorSettingsReposTest` | default=HANG | PASS ×3, ok=1 |

## None of them hung

Run standalone, every one of the seven finished — uniformly, at ~110 s, against
HotSpot's ~17 s for the same class on the same classpath. A time that uniform
across seven unrelated test classes is a fixed cost, not test work, and the
cost was classpath resource enumeration. Everything below came out of chasing
that one number.

## Defect 1 — getResources re-derived the whole classpath per probe

SmallRye Config's `AbstractLocationConfigSourceLoader.isInClassloader` is
`classLoader.resources(uri.getPath()).anyMatch(..)`. Quarkus's config bootstrap
runs it once per discovered config source per profile: **1388 times over a
4230-entry classpath for one test class**. HotSpot serves those 1388 calls with
82,275 filesystem syscalls. CratonVM used **11,224,496** — 4.43M `statx` and
6.79M `readlink`, a factor of 136. Three causes:

* **The canonicalize cache is a 1024-entry FIFO and the scan's working set IS
  the classpath.** With 4230 entries the cycle evicts every root exactly before
  its next use, so the hit rate is zero and every probe re-runs `realpath` on
  every root. The cap (Round 7 audit, HIGH #4) was there to bound growth from
  arbitrary *probe* paths; roots are a different population, bounded by the
  entry list, and now memoize separately.
* **`getResources("")` names the classpath roots themselves**, and the generic
  path answered it with an `exists()` statx, two `canonicalize` calls (`dir`,
  and `dir`-with-trailing-slash — distinct cache keys) and an `is_dir()` statx
  per directory entry. A `Directory` entry is a directory that is on the
  classpath; both facts were settled when it was admitted.
* **The enumeration was eager.** It built a `java.net.URL` — a 13-slot
  synthetic plus three `String`s — for every match before the caller looked at
  one, so a short-circuiting consumer paid for all 1843. The JDK's enumeration
  is lazy per element, which is the whole reason HotSpot's numbers are small:
  `anyMatch` stops at the first hit, about eight entries in.
  `Enumeration$Impl` now carries a slot saying its elements are URL specs and
  materializes each in `nextElement`.

Fixed in `classloading/src/class_path.rs` and
`native-builtins/src/classloader.rs`.

## Defect 2 — a hard-coded descriptor that the library had outgrown

`native_quarkus_logging_handle_failed_start` wrote its own copy of
`LoggingSetupRecorder.initializeLogging`'s descriptor, with six
`Ljava/util/List;` parameters. This Quarkus revision's recorder takes seven —
it grew a per-named-handler formatter map — so every call raised
`NoSuchMethodError`. That was swallowed as a logging-setup failure, and the
JUnit extension that triggers it (`io.quarkus.test.config.LoggingSetupExtension`)
then failed to construct, so **the class was discovered and never started**:
`found=1 started=0 ok=0 failed=0`, which the harness scores as PASS.

Both bootstrap classes were in that state. Reading the descriptor off the class
and filling the argument vector from it (either arity works) is what let them
run at all — and running is what exposed defect 3.

Fixed in `native-builtins/src/phases_late.rs`.

**The general lesson is the scoring rule, not this method.** `found>0 &&
failed==0` counts "an extension blew up in `beforeAll`, nothing ran" as a pass.
Any quarkus PASS with `started=0` is worth re-reading as unknown, not green.

## Defect 3 — the native xerces scanner read `XMLChar.CHARS` unpinned and uninitialized

With the two bootstrap classes finally running, both failed: `Failed to load
beans`, from `Sisu.addClassLoader`, which parses every
`../../../../apps/META-INF/plexus/components.xml` on the classpath while concurrently
class-loading 591 bean classes. 5–22 of the 46 documents failed on every run
with `ParseError at [1,2]: The markup in the document preceding the root
element must be well-formed` or `[1,11]: The processing instruction target
matching "[xX][mM][lL]" is not allowed`. Sequentially all 46 parse; HotSpot is
clean either way.

CratonVM replaces xerces' `XMLEntityScanner.scanQName`/`scanContent` with
natives. Both read the `XMLChar.CHARS` character-class table, and both got it
wrong in a way that only shows under allocation pressure:

* `xmlchar_chars_array` accepted a merely **loaded** class. `XMLChar.<clinit>`
  assigns `CHARS = new byte[0x10000]` first and spends the rest of its body
  filling it, so another thread could read the reference mid-fill and get a
  zero mask for every character below the fill point. Ordinary bytecode cannot
  hit this — the `getstatic` inside the real `scanQName` carries the
  initialization barrier. Replacing that method with a native dropped it.
* The table was then held as a **raw `ObjectRef` across `load`,
  `invokeListeners` and `addSymbol`** — all of which allocate, and so can move
  it. Every other object in those two functions is already held through a pin
  and re-read; the table was the one that wasn't.

Either way `isNameStart('c')` answers false and a perfectly good document is
rejected at its second character.

Fixed in `native-builtins/src/lib.rs` and `native-builtins/src/xml_xerces.rs`.

**Shape worth keeping: replacing a JDK method with a native drops the
initialization barrier the bytecode carried.** A native that reads a static
must ask for initialization itself; `class_id_by_name` answers "loaded", which
is not the same question. The same `class_id_by_name`-then-`get_static_field`
idiom appears at roughly ten other sites in `native-builtins`; only this one
has measured evidence behind it, and the rest were left alone rather than
changed blind.

## Verification

* 7/7 PASS on default / G1 / ZGC, 2 shards, the harness's own 180 s cap.
  `PomProfileReposEffectivePomTest` and `ProxyAndMirrorSettingsReposTest`
  report `started=1 ok=1`, identical to HotSpot; the other five report
  `found=1 started=0`, which is what **HotSpot also reports** — those are
  `@QuarkusTest`-style classes the flat-classpath harness structurally cannot
  bootstrap, and that is a harness scope limit, not a VM defect.
* Standalone per-class wall time ~110 s → ~55 s; syscalls 11.2M → 0.98M.
* `Mix` probe (46 concurrent `components.xml` parses under 591 concurrent
  class loads): 5–14 failures per run → 0, four runs.
* `cargo test` green for `cratonvm-classloading` (793), `cratonvm-native-builtins`
  (3540) and `cratonvm-vm --lib` (2501). The three failures in
  `jit_local_exception_handler_tests` were confirmed pre-existing on `dev`.

## Two things left standing, deliberately

1. **Headroom, not correctness.** ~55 s standalone is still ~3× HotSpot's
   ~17 s, and under six concurrent forks on this shared 8-core host the flat
   180 s cap is still marginal for these startup-bound classes.

   **Followed up 2026-08-13, and the obvious next lever was not the answer.**
   The profile still showed `ClassLoader.lambda$resources$0` and the
   `isInClassloader` predicate at ~39% of interpreted samples, and a
   microbenchmark said the enumeration was 40× off HotSpot — 1.20 ms vs
   0.03 ms — when `anyMatch` matches on element 1, because the scan still ran
   to the end even though the URLs were built lazily. Making the *scan* lazy
   too (`ENUM_ELEMENTS_LAZY_SCAN`) took that 1.20 ms to 0.06 ms, a 20×.

   It moved these seven classes **0%**. An interleaved A/B, two rounds per
   binary per class, came back between −8% and +5% — noise. The reason is in
   the other half of the same measurement: a call that *consumes* the
   enumeration costs 20.9 ms here against HotSpot's 11.7 ms, only 1.8×, and
   this workload's `isInClassloader` calls are overwhelmingly of that kind.
   1388 calls × ~12 ms is very nearly HotSpot's entire ~17 s runtime, so
   SmallRye asking 1388 times is the cost, on both VMs, and CratonVM is
   already within ~2× of HotSpot per call in both shapes.

   So the residual is **not** the enumeration. Whatever is left belongs to
   ordinary interpretation of the 1843-element stream pipeline, and a page
   that wants to close it should start from a fresh profile rather than from
   this one. The lazy scan landed anyway — it is a real 20× on a common shape
   (`findFirst`/`anyMatch` over `resources()`, and the singular
   `getResource`), it matches the JDK's own laziness, and it is neutral here.
2. **The page's repro command names a `--gc` flag `run-quarkus-suite.sh` does
   not accept.** GC variant is selected by `--bin` (a wrapper) alone. Anyone
   copying that command gets the usage text and no run. Corrected in
   docs/known-issues/quarkus/investigate-INDEX.md.
