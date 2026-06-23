# SBR-02 — `String.replaceAll` throughput wall (~30–60× slower than HotSpot)

**Status:** 🟠 Open — **perf HANG** (CratonVM-only); overlaps prior **SB-12** (`PluginXmlParserTests`).
**Recommendation:** **HANDOFF** — interpreter/regex throughput; known perf family.
**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probes (2)

`MinRegexProbe`, `RegexLoopProbe`. `RegexLoopProbe`'s own comment: *"Mirror of
`PluginXmlParser.format` — the exact chain that hangs."*

## Symptom — slowness, not deadlock

`MinRegexProbe` runs `input.replaceAll("\\{@code (.*?)}", "`$1`")` in a loop of
`N=200000`:

```
regex=code N=200000
iter 100 ok
iter 1000 ok
iter 10000 ok        <-- CratonVM reached here at the 60 s timeout, then killed
```

- **HotSpot:** prints through `iter 100000 ok`, `final=...`, `DONE 200000`, exits
  0 in ~1–2 s.
- **CratonVM:** reaches only ~10 000 / 200 000 iterations in 60 s. Extrapolated
  ≈ **30–60× slower**. Process makes forward progress (it is **not** a deadlock) —
  it is a regex/`replaceAll` (Pattern/Matcher) throughput problem.

## Root cause (hypothesis)

`java.util.regex` Matcher inner loop is interpreted hot code; either it never
tiers up under CratonVM for this shape, or the `replaceAll`/`appendReplacement`
native path allocates per-iteration and pressures GC. `PluginXmlParserTests`
(SB-12) times out for the same reason. Profile `Matcher.find`/`appendReplacement`
under the JIT to see whether the loop compiles.

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" MinRegexProbe   # ~10k iters in 60s
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" MinRegexProbe                                                          # DONE 200000 in ~1s
```

## Impact

`PluginXmlParserTests` and any AsciiDoc/Javadoc tag-rewriting (`{@code}`,
`{@link}`) build step. Correctness is fine; only throughput. Lower urgency than a
crash but it does manifest as a wall-clock hang in the JUnit suite.
