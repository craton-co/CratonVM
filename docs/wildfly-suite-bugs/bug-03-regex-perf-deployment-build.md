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

## Update (2026-06-14): (B) implemented behind a default-OFF flag — exposes a latent regex-codegen miscompile (new layer C)

(B) is now implemented, **default-OFF** via `CRATONVM_JIT_VIRTUAL_TIERUP=1`
(`vm/src/runtime/env_cache.rs` + `execute_invokevirtual_cached` /
`execute_jit_call_decoded` in `vm/src/runtime/interpreter.rs`):
- A warmup invocation counter on the invokevirtual/invokeinterface VirtualBytecode
  path (mirrors `execute_invokestatic_cached`), placed **after** all interception
  checks and **before** the method-level monitor; skips `invokespecial` and
  `synchronized` methods. Monomorphic-safe (the receiver class id is already
  verified `== receiver_class_id`).
- `execute_jit_call_decoded` dispatches the compiled instance method using the
  already-decoded `args_slice` (receiver = arg 0); deopt / too-many-args fall
  through to the interpreted frame push with the operand stack untouched.
  `execute_jit_call` (static path) is left byte-identical.

**Mechanism validated (flag ON):**

| benchmark | flag OFF | flag ON | HotSpot |
|-----------|----------|---------|---------|
| `InstBench` instance charAt loop, 12M | 21 613 ms | **285 ms** (~76×) | 20 ms |
| `StrInstBench` instance build()/countDots() | (correct) | **correct** | correct |

InstBench/StrInstBench/Props/CSBench results are bit-identical to HotSpot with the
flag on, and flag-OFF default behavior is unchanged.

**(B) ON exposed a pre-existing JIT codegen miscompile in the regex match engine
(layer C).** With the flag on, `String.replaceAll("[.]","/")` returned `/o/r/g/.../`
(a `/` inserted at every position — empty match everywhere) instead of `org/.../`.

### Layer C — root-caused to `Matcher.search(I)Z`, MITIGATED (skip-listed)
Bisected with the runtime hook `CRATONVM_JIT_BISECT_SKIP` (no rebuild per step;
`skip_list.rs`). Result: **skipping ONLY `java/util/regex/Matcher.search(I)Z`
makes RegexBench + RegexBench2 fully correct again** (allow-only-search → corrupt;
allow-only-`find`/`reset` → correct). So `search`'s *compiled body* is the sole
culprit; every other regex method (incl. `Pattern$Start.match`/`LastNode.match`,
which newly compile when search is skipped) is correct.

The miscompile is NOT in the layer-B dispatch — `InstBench`/`StrInstBench`
(instance methods, incl. object-returning) are bit-identical to HotSpot. Disasm
(`CRATONVM_DBG_JIT_DISASM=java/util/regex/Matcher.search`,
`wildfly-suite/repro/search_disasm.txt`) shows correct field offsets, correct
`root.match` args, and correct result handling; `search` calls the (also-compiled)
`Pattern$Node.match` via the generic JIT→JIT dispatch helper. This matches the
**same signature as the `ByteBuddyState.make` ban in `skip_list.rs`** — "a value/
receiver lost across the JIT→JIT call boundary," a general codegen defect already
tracked there. The exact faulty instruction is a deeper follow-up.

**Mitigation (landed):** `("java/util/regex/Matcher", "search")` added to the
`skip_list.rs` targeted bans. With it, regex is correct under
`CRATONVM_JIT_VIRTUAL_TIERUP=1`; `search` (the hot scan loop) stays interpreted,
but the other regex nodes + the layer-A charAt intrinsics still JIT, so:

| RegexBench2 (warm 20k) | flag OFF | flag ON + search ban | HotSpot |
|------------------------|----------|----------------------|---------|
| precompiled replaceAll | 14 958 ms | **9 529 ms** | 26 ms |

Per-`replaceAll` ≈ 0.48 ms — ShrinkWrap calls it once per class, so ~1000 classes
≈ 0.5 s (was minutes-to-never). The skip-list entry is a **no-op for the default
config** (search only compiles under the still-default-OFF virtual-tierup flag).

### Layer C root cause — CORRECTED (2026-06-15): TWO distinct bugs, earlier conflated

An earlier write-up here attributed layer C to "the precise-oop-maps
post-safepoint-reload gap" based on a gate-toggle test (precise-OFF → corrupt;
`CRATONVM_PRECISE_JIT_MAPS=1` → SIGSEGV). **That conflated two different bugs in
two different methods.** Careful A/B (with `Matcher.search` temporarily un-banned)
separates them:

1. **`String.codePointAt` precise-ON crash — a real precise-maps Stage-A bug,
   FIXED.** The invokevirtual/interface inline MIC/PIC cascade called the compiled
   callee on a class-id hit then `jmp`ed to the *shared* post-safepoint reload, but
   the pre-safepoint spill was only on the slow path → the inline-hit path reloaded
   an un-spilled stale slot into the receiver register → SIGSEGV. Minimal repro
   `wildfly-suite/repro/CPBench.java` (`s.codePointAt(i)`). FIXED in `jit/src/x64.rs`
   (spill before the cascade under `precise_maps`; gate-OFF byte-identical — bt16/
   bt18 golden, full jit suite passes). This was the **SIGSEGV** half of the toggle
   test. ✔ verified fixed under precise-ON.

2. **Compiled `Matcher.search` zero-width corruption — the actual reason for the
   ban — is PRECISE-INDEPENDENT and GC-INDEPENDENT, and is STILL OPEN.** With
   `search` un-banned it corrupts (`/o/r/g/...`) under **all** of: precise-OFF,
   precise-ON (even after fix #1), and `--Xmx 8g` (no GC). So it is NOT the
   precise-maps reload and NOT GC relocation — it is a plain compiled-`search`
   JIT codegen miscompile (manifests only with `search` AND its callee both
   compiled; either interpreted → correct). Root cause within compiled `search`
   not yet pinpointed; the `Matcher.search` skip-list ban remains the mitigation.
   This was the **corrupt** half of the toggle test — a separate bug from #1.

**Consequence:** precise maps does NOT fix layer C (bug #2). The fix #1 above is a
genuine, separate precise-maps improvement, but the `Matcher.search` (and
`ByteBuddyState.make`, AQS/ExecProbe) bans remain necessary regardless of precise
maps. The mitigation table above still holds.

**Mitigation perf (search ban on):**

| RegexBench2 (warm 20k) | flag OFF | flag ON + search ban | HotSpot |
|------------------------|----------|----------------------|---------|
| precompiled replaceAll | 14 958 ms | **9 529 ms** | 26 ms |

### Remaining
1. **Pinpoint + fix the compiled-`search` codegen miscompile (bug #2)** — a plain
   JIT→JIT compiled-instance dispatch corruption, precise/GC-independent. This is
   what the `Matcher.search` ban papers over; same family as `ByteBuddyState.make`.
2. **Flipping `CRATONVM_JIT_VIRTUAL_TIERUP` default-ON is still BLOCKED** — not by
   precise maps (fix #1 landed), but by bug #2 and likely other per-method
   compiled-instance miscompiles the whole-surface compilation would expose. The
   per-method bans don't scale to the full suite, so (B) stays **default-OFF**
   pending those codegen fixes + a full-suite re-test on an uncontended machine.

Until then, run the WildFly Arquillian client with `CRATONVM_JIT_VIRTUAL_TIERUP=1`
(precise maps OFF) — the `Matcher.search` ban keeps regex correct and fast enough
to build the JUnit-5 deployment archive, while static-method hot paths benefit
from layers A/B. (The codePointAt precise-ON fix #1 is independent — it makes the
precise-maps path safe for compiled instance methods that hit the inline cascade,
a prerequisite for any future B+precise default-on, but not sufficient alone.)
