---
name: spring-boot-pluginxmlparser-hang
description: Spring Boot buildSrc PluginXmlParserTests hangs (CV-only) under both --nojit and JIT; HotSpot passes 2/2 in ~2s. Root cause narrowed to java.util.regex engine / GC / JIT, not Xerces DOM. Moved into docs/known-issues 2026-06-22 after a fresh dev d95a836e re-run reconfirmed the hang (P-core-pinned, watchdog off, 360s). Sibling context: spring-boot-buildsrc-coldpath-hangs-2026-06-22.
metadata:
  type: known-issue
  area: jit, gc, regex, throughput
---

# PluginXmlParserTests hangs in `java.util.regex` (NOT Xerces DOM)

> **CONFIRMED STILL OPEN on dev `d95a836e` (2026-06-22).** Re-run P-core-pinned
> (`0xFFFF`), default watchdog disabled, 360s timeout: **killed at 360s, no result**
> (HotSpot PASS 2/2, 1s). Moved from the gitignored suite folder
> (`apps/spring-boot/cratonvm-bug-reports/SB-SUITE-HANG-01-pluginxmlparser-regex.md`)
> into `docs/known-issues/` because the bug still reproduces. See
> [[spring-boot-buildsrc-coldpath-hangs-2026-06-22]] for the full suite run.

**Status:** 🔴 Open — deep regex-engine / GC / JIT issue. Root cause narrowed,
not yet fixed.
**Severity:** CV-only HANG. HotSpot passes 2/2 in **2s**; CratonVM hangs
(300s+ timeout) under **both `--nojit` and JIT**.
**Affected test:** `org.springframework.boot.build.mavenplugin.PluginXmlParserTests`

## ⚠ Correction of the earlier diagnosis

The first report blamed Xerces `DOM2DTM.nextNode` (XPath-over-DOM tree walk).
**That was wrong** — a transient watchdog snapshot. Verified this session:

- `DOMWalkProbe` (manual deferred-DOM walk, 2131 nodes) **terminates** on CratonVM,
  identical to HotSpot — the DOM navigation links are NOT corrupted.
- `XPathProbe` (XPath over the document root AND relative to a deep `mojo` node —
  exactly what `PluginXmlParser.textAt`/`nodesAt` do) **completes** on CratonVM:
  `groupId`, `artifactId`, `mojo count=6`, `goal='build-info'`, `param count=5`.

So XPath/DOM is fine. The real hang is downstream, in `PluginXmlParser.format()`.

## Real location

Stack dump of the live test (`--stack-dump-on-timeout 30`, JIT-on) — parsing
**completed** (frames 56–60), the hang is in description formatting:

```
56 PluginXmlParserTests.parseExistingDescriptorReturnPluginDescriptor
57 PluginXmlParser.parse
58 PluginXmlParser.parseMojos
59 PluginXmlParser.parseParameters
60 PluginXmlParser.parseParameter
61 PluginXmlParser.format                         ← the 4 .replaceAll(...) chain
62 java/lang/String.replaceAll
63 java/util/regex/Pattern.compile(String)
64 java/util/regex/Pattern.<init>
65 java/util/regex/Pattern.compile()              ← LOOP (last_pc>pc backward branch)
66 java/lang/String.codePointAt
67 java/lang/String.checkIndex
```

`format()` (PluginXmlParser.java:114) does 8 literal `String.replace` + 4
`String.replaceAll` with reluctant groups, e.g. `\{@code (.*?)}` → `` `$1` `` and
`<a href=.\"(.*?)\".>(.*?)</a>` → `$1[$2]`.

Other dump snapshots of the same hang land in
`Matcher.replaceAll → appendReplacement → appendExpandedReplacement → end → checkGroup`
(the `$1` group-expansion loop) — i.e. the loop floats between `Pattern.compile()`
and the `Matcher` replace path depending on timing.

## What is and isn't true (this session's measurements)

Minimal repro `runner/MinRegexProbe.java` — `input.replaceAll(regex, repl)` in a loop:

- **Single `replaceAll` calls work** (`runner/RegexProbe.java`, `RegexLoopProbe`
  one-shot): all 4 format() regexes return the correct result, fast, matching HotSpot.
- **`--nojit`:** N=50 iterations complete instantly; N=500 does NOT finish in 30s.
  Identical iterations, so this is **not linear slowness** — throughput **degrades**
  as the run proceeds (GC pressure from per-call `Pattern`/`Matcher`/node allocation),
  eventually crawling. So `java.util.regex` is **pathologically slow** on CratonVM
  even in the interpreter, and gets slower over time.
- **JIT-on:** hangs *harder* — after ~500–10000 iterations one `replaceAll` call
  never returns (a genuine loop), in whichever regex method JIT-compiled
  (`Pattern.compile()` most often; sometimes `Matcher.appendExpandedReplacement`).
- **CPU when hung:** ~9% of one core (0.27s CPU / 3s wall) after an initial
  multi-core burst — i.e. it transitions from busy to **near-idle/blocked**, NOT a
  tight CPU spin. Consistent with a GC-safepoint stall or allocation-bound crawl,
  not catastrophic backtracking (which would peg a core).
- Big heap (`--Xmx 8g`) does **not** help (young-gen nursery is fixed-size).

The real test hangs under **both** jit and nojit, so the *primary* blocker is the
nojit slowness/stall (the JIT loop is a second, distinct defect on top).

## Hypotheses (for the deep fix)

1. **GC / allocation pathology.** `replaceAll` allocates a fresh `Pattern` (+ `int[]
   temp`, a tree of `Node`s), a `Matcher`, and a `StringBuilder` per call. If some
   structure is pinned as a GC root and accumulates (cf. the `lhm_overlay` O(n)-GC
   leak, memory `reference_lhm_overlay_gc_leak`), young GC walks grow without bound
   → each iteration triggers a slower GC → apparent hang. The near-idle CPU + the
   "degrades over iterations" signature fit a GC-root leak or a safepoint stall.
2. **JIT counted-loop miscompile** (the *second*, jit-only defect): the
   `for (x=0; x<patternLength; x += Character.charCount(c)) c = …codePointAt(x)` loop
   in `Pattern.compile()` (and the `cursor`-walk in `appendExpandedReplacement`)
   stops advancing under JIT after warmup — the known "counted-loop / backward-compare
   loop" archetype (cf. skip_list: `Calendar.isFieldSet`, `Arrays.fill`, Xerces
   `skipString`). `CRATONVM_JIT_BISECT_SKIP` of `Pattern.compile` /
   `Matcher.append*` did NOT close it, so the looping method is a smaller callee not
   yet pinned.

## Repro

```bash
cd apps/spring-boot/buildSrc/runner
CV=C:/craton/CratonVM-sbloop/target/release/cratonvm.exe
# minimal — slow under nojit, hangs under jit:
"$CV" --java-home "$JDK" --nojit -cp . MinRegexProbe code 500       # >30s, no DONE
"$CV" --java-home "$JDK"          -cp . MinRegexProbe code 200000   # hangs ~iter 10k
# real test:
cd ..; CP=$(cat test-classpath.txt)
"$CV" --java-home "$JDK" --stack-dump-on-timeout 30 -cp "runner;$CP" \
    RunJUnit org.springframework.boot.build.mavenplugin.PluginXmlParserTests
```
HotSpot: `JUNIT_RESULT tests=2 passed=2 failed=0` in ~2s.

## Next step

Separate the two defects:
1. Profile a single `Pattern.compile`/`Matcher.replaceAll` under nojit to find the
   GC/allocation hot spot (`--verbose:gc`; watch young-GC frequency vs iteration).
   Check for a regex object pinned as a GC root (Pattern cache? a native side-table?).
2. For the JIT loop: get the disasm of the actual non-advancing callee
   (`CRATONVM_DBG_JIT_DISASM`) — likely a tiny `cursor`/`x`-incrementing accessor.
   If a clean per-method skip can't be found, a `java/util/regex/` package skip in
   `vm/src/jit/skip_list.rs` is the codebase-sanctioned stopgap (matches the many
   counted-loop entries already there).
```
