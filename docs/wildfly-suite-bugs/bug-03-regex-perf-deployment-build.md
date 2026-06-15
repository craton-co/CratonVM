# Bug 03 — `java.util.regex` is 50–600× slower than HotSpot (Gap C: deploy-phase "hang")

**Severity:** Medium (performance, CratonVM-only). Not a crash or deadlock — the
operation makes progress but is so slow it never finishes in practice. This is the
**Gap C** blocker for running the WildFly Arquillian client under CratonVM.

## Symptom
Running the WildFly Arquillian client under CratonVM (KRun + remote container, after
[bug-02](bug-02-zipfile-entries-null.md) fixed the ShrinkWrap NPE) never completes:
a 600 s run prints `BEGIN` then nothing. A `--stack-dump-on-timeout 150` dump shows
the main thread **executing** (frames change between dumps — not blocked on I/O),
deep in deployment-archive building:

```
DeploymentGenerator.loadAuxiliaryArchives
 JUnitJupiterDeploymentAppender.buildArchive
  ContainerBase.addPackages / addPackage
   URLPackageScanner.scanPackage / handleArchiveByFile / foundClass
    AssetUtil.getFullPathForClassResource
     java.util.regex.Matcher.replaceAll -> find -> Pattern$Start.match -> Pattern$BmpCharProperty.match
```

ShrinkWrap calls `AssetUtil.getFullPathForClassResource` (a regex `replaceAll`)
**once per class** while packaging the JUnit-5 + Arquillian container archive
(hundreds–thousands of classes). Under CratonVM each call is slow enough that the
whole archive build takes minutes-to-never.

## Quantification — `RegexBench` (2000 iterations, 58-char input)
[`RegexBench.java`](../../wildfly-suite/repro/RegexBench.java):

| | HotSpot 25 | CratonVM | Slowdown |
|--|-----------|----------|----------|
| `String.replaceAll("[.]","/")` ×2000 | 54 ms | 2691 ms | **~50×** |
| precompiled `Matcher.replaceAll` ×2000 | 3 ms | 1830 ms | **~600×** |

## Deeper root cause — not regex-specific: native-bridged char accessors
Further benchmarks (warm, JIT on) localise it to **per-char VM→native boundary
crossings**, not regex or allocation:

| benchmark (warm) | HotSpot | CratonVM | slowdown |
|------------------|---------|----------|----------|
| precompiled `Matcher.replaceAll` ×20000 | 27 ms | 10 398 ms | ~385× |
| same, JIT **off** (`--nojit`) | — | 14 228 ms | (JIT helps only ~27%) |
| `String.replace(char,char)` ×5000 (native) | 7 ms | 2 705 ms | ~386× |
| **`charAt` loop, NO allocation** (11.6 M calls) | 17 ms | **18 081 ms** | **~1063×** |
| `substring` (allocation) ×200000 | 10 ms | 501 ms | ~50× |

The decisive one: a tight `charAt`/`length` loop **with no allocation** is ~1063×
slower, while an allocation-heavy `substring` loop is only ~50×. So the cost is
**not** allocation/GC and **not** regex-engine-specific — it is the per-call cost of
the hot String accessors. CratonVM's JIT has OSR (1000-backedge) and an invocation
threshold (2000), but `String.charAt`/`length` are **native-bridged** (they appear on
the JIT skip-list / are serviced by Rust natives), so even a JIT/OSR-compiled loop
must cross the VM→native boundary on **every** `charAt` (~1.5 µs) instead of HotSpot's
intrinsified direct char-array read (~1.5 ns). The `java.util.regex.Pattern$Node.match`
loop calls `charAt` per character per position, so it inherits the same ~400–1000×
penalty; ShrinkWrap calls it per class.

## Update (2026-06-14): two independent root causes — (A) FIXED, (B) is the real regex blocker

Investigation split the slowdown into **two separate causes**. The original
write-up assumed the `Pattern$*.match` loop was JIT-compiled but slow because of
native `charAt`; in fact it is **not compiled at all** (cause B). Both must be
fixed for regex to be fast.

### (A) native-bridged char accessors in *compiled* code — **FIXED**
The JIT already had complete, unit-tested call-site intrinsic codegen for
`String.length/charAt/isEmpty/hashCode/equals/compareTo/indexOf` (the
`STRING_ACCESS`/`STRING_SEARCH` regions in `jit/src/x64.rs`), but it was
**dormant**: the interpreter passed `string_layout_resolver: None` at the tier-up
`try_compile` sites ("later wave"). Fix:
- `vm/src/runtime/interpreter.rs` — added `resolve_string_field_layout()` and
  wired it into both `try_jit_upgrade_with_gate` `try_compile` sites (main
  invocation-threshold tier-up + recursive inline-callee). (`try_jit_compile_callee_slow`
  was already wired.)
- Extended it to **`CharSequence.charAt/length/isEmpty`** behind a receiver
  class-id guard: `jit/src/lib.rs` (`StringFieldLayout.string_class_id`,
  `try_resolve_string_intrinsic` now returns a `guard_class_id` and matches
  `java/lang/CharSequence`) + a `[recv+0] == string_class_id` guard in the
  `STRING_ACCESS` codegen (`jit/src/x64.rs`) that deopts to native dispatch for
  any non-String CharSequence (e.g. `StringBuilder`) — the same pattern the CRC32
  intrinsics use. Regex calls `charAt`/`length` through `CharSequence`
  (`Matcher.text`), so this is required for the regex receiver type.

Measured (JDK 25 boot, 58-char input; `wildfly-suite/repro`):

| benchmark | before | after | HotSpot |
|-----------|--------|-------|---------|
| `CharAtBench` static charAt loop, 12M calls | ~18 000 ms | **195 ms** | 10 ms |
| `CSBench` **CharSequence**-typed charAt, 12M | 10 827 ms | **395 ms** | — |

~90× / ~27×. Correct (sum/acc bit-identical to HotSpot). 31 codegen unit tests
pass (`intrinsic_string_access` incl. new CharSequence-guard tests,
`intrinsic_string_search`).

### (B) instance methods never invocation-tier-up — **the real regex blocker, OPEN**
After (A), `RegexBench2` was **unchanged (~15 000 ms)**. `CRATONVM_DBG_JITC=1`
shows **zero** regex methods compile — `Pattern$*.match` / `Matcher.find/replaceAll`
run in the **interpreter**, where `charAt` is always native-bridged, so the
call-site intrinsic never applies (it only fires in *compiled* callers).

Root cause, confirmed with a zero-code-change A/B (`CharAtBench` vs `InstBench`,
identical charAt loop body):

| same loop, called 200k× | CratonVM | HotSpot |
|-------------------------|----------|---------|
| in a **static** method  | **195 ms** | 10 ms |
| in an **instance** method | **24 729 ms** | 20 ms |

The 126× gap is purely static-vs-instance. Reason: `increment_invocation` (the
warmup counter that triggers `try_jit_upgrade_with_gate`) is called in **exactly
one** place — `execute_invokestatic_cached`. Instance methods (invokevirtual /
invokeinterface, in `execute_invokevirtual_cached`) have **no invocation counter**;
their only paths to the JIT are OSR (needs ≥1000 back-edges in a *single*
invocation) and direct-call-callee compilation. Regex spreads its work across many
**short-loop instance methods**, so none ever cross OSR and none compile.

**Fix direction for (B):** give the invokevirtual/invokeinterface dispatch path an
invocation counter mirroring `execute_invokestatic_cached` (increment →
`try_jit_upgrade_with_gate` → update invoke cache to `Jit`). This is a
**large-blast-radius** change (it makes the whole instance-method surface
JIT-eligible, interacting with the curated skip-list and `execute_jit_call`
receiver handling), so it needs full WildFly/Kafka/Tomcat suite re-test on an
uncontended machine — deferred. Once it lands, regex gets fast *for free* because
the (A) intrinsics are already in place.

Secondary follow-up: the OSR (`try_osr`) and early-compile `x64::compile` paths
still pass `string_layout: None` and don't run `try_resolve_string_intrinsic`, so a
*single* hot-loop instance method that DOES OSR-compile won't get charAt
intrinsified yet. Lower priority than (B) (won't help regex's short loops).

Until (B) lands, the CratonVM Arquillian client still cannot build the JUnit-5
deployment archive in reasonable time. (The no-container per-class suite is
unaffected — it never builds a real deployment, so it never hits this hot path.)
