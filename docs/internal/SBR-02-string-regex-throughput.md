---
name: SBR-02-string-regex-throughput
description: FIXED + merged to dev — Spring Boot buildSrc PluginXmlParserTests hung in java.util.regex (String.replaceAll / literal String.replace chain). Resolved by flipping CRATONVM_NATIVE_STRING_REGEX default-ON (commit 0d7dfc28, merge 01375f90): replaceAll/replaceFirst/matches/replace(CharSequence,CharSequence) route to fast cached Rust-regex natives. Was docs/known-issues/spring-boot-pluginxmlparser-hang.md.
metadata:
  type: fixed-bug
  area: regex, throughput, jit, gc
---

# SBR-02 — `java.util.regex` / `String` regex throughput wall (FIXED)

> **STATUS: ✅ FIXED + merged to `dev`.** Resolved by the SBR-02 native String
> regex work: `CRATONVM_NATIVE_STRING_REGEX` flipped **default-ON**
> (`0d7dfc28` "ASCII-class parity + literal replace native; flip
> CRATONVM_NATIVE_STRING_REGEX default-ON", merge `01375f90`; original native
> landed `9e724658`). This file was `docs/known-issues/spring-boot-pluginxmlparser-hang.md`
> and is retained here per the known-issues triage rule (fixed-bug writeups live
> in `docs/internal/`).

**Affected test:** `org.springframework.boot.build.mavenplugin.PluginXmlParserTests`
(Spring Boot buildSrc). HotSpot passes 2/2 in ~1–2s; CratonVM previously hung
(300s+) under **both `--nojit` and JIT**.

## Root cause

`PluginXmlParser.format()` (PluginXmlParser.java:114) runs a chain of 8 literal
`String.replace(CharSequence,CharSequence)` + 4 `String.replaceAll(...)` with
reluctant groups (`\{@code (.*?)}` → `` `$1` ``, `<a href=.\"(.*?)\".>(.*?)</a>`
→ `$1[$2]`, …), once per parsed `<parameter>` description.

On the real-JDK path, every `replaceAll` allocates a fresh `Pattern` (with an
`int[] temp` + a tree of `Node`s), a `Matcher`, and a `StringBuilder`, then runs
the `java.util.regex` engine **interpreted** — and every `Matcher`/`Pattern`
step crosses the VM→native `String`-accessor boundary (`codePointAt`,
`checkIndex`, …). The literal `replace(CharSequence,CharSequence)` overload runs
an interpreted per-char scan. Combined this is **30–600× slower than HotSpot**,
and throughput degrades over the run under GC pressure from the per-call
allocation, so a tight `format()` loop crawls to an apparent hang. A second,
JIT-only counted-loop pathology stacked on top of the interpreter slowness
(the `Pattern.compile()` / `Matcher.append*` cursor walks).

Earlier diagnoses that blamed Xerces `DOM2DTM.nextNode` (XPath/DOM walk) were
wrong — `DOMWalkProbe`/`XPathProbe` both complete on CratonVM identical to
HotSpot. The hang was purely downstream in `format()`'s regex/replace chain.

## Fix

`vm/src/runtime/interpreter.rs::force_native_over_real_jdk_bytecode` routes
`String.{replaceAll,replaceFirst,matches}` **and** the literal
`String.replace(CharSequence,CharSequence)` to CratonVM's fast cached
`regex`/`fancy-regex` natives (`native-builtins` `lang_string.rs`), which are
Java-faithful (`$N` / `${name}` / `\`-escapes, ASCII-default `\d`/`\w`/`\s`/`\b`)
and validated byte-identical to HotSpot. Gated by
`CRATONVM_NATIVE_STRING_REGEX`, now **default-ON** (opt-out `=0`/`false` reverts
to the real-JDK bytecode as the safety net). These are faithful alternate
implementations (like intrinsics), not synthetic stubs.

Because the entire `format()` chain (4 `replaceAll` + 8 literal `replace`) is now
served by the native, neither the interpreter slowness nor the JIT counted-loop
defect is reachable on the default path — no `java/util/regex/` JIT skip was
needed.

## Verification (dev `99510377`, 2026-06-22, branch `fix/pluginxmlparser-hang-recheck`)

`RegexLoopProbe` is a line-for-line mirror of `PluginXmlParser.format()`
(`scratch/regexperf/RegexLoopProbe.java`). Binary `cvpxph.exe`.

| Run | Mode | Iters | Result | Time |
|-----|------|-------|--------|------|
| RegexLoopProbe | `--nojit`, default gate-ON | 100,000 | ✅ DONE | 7.0s |
| RegexLoopProbe | JIT, default gate-ON | 100,000 | ✅ DONE | 7.4s |
| RegexLoopProbe | JIT, default gate-ON | 1,000,000 | ✅ DONE (no late JIT loop) | 56.8s |
| MinRegexProbe `code` | JIT, default gate-ON | 200,000 | ✅ DONE | 2.0s |
| RegexLoopProbe | `--nojit`, **gate-OFF** (`=0`) | 5,000 | ⚠ ~20s (old path, confirms the bug) | 19.9s |

- **Parity:** `RegexLoopProbe` final output is **IDENTICAL** to HotSpot jdk-25.
- gate-OFF 5,000 iters = 20s extrapolates to ~400s for 100k → the original
  300s+ hang. gate-ON 100k = 7s. The default-ON flip is the fix.

## Repro (for the record)

```bash
cd scratch/regexperf            # RegexLoopProbe.java mirrors PluginXmlParser.format
CV=.../target/release/cvpxph.exe
JH="C:/Program Files/Java/jdk-25"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
"$CV" --java-home "$JH"        -cp . RegexLoopProbe 100000   # default ON: ~7s, DONE
CRATONVM_NATIVE_STRING_REGEX=0 "$CV" --java-home "$JH" --nojit -cp . RegexLoopProbe 5000  # OFF: ~20s
```

## Residuals / caveats

- The real-JDK `java.util.regex` engine is still interpreted-slow on the
  opt-out path (`=0`) — that's the safety net, not the default. A genuine engine
  speedup is a separate, larger effort.
- The opt-out exists because Rust `regex` Unicode-class semantics differ from
  Java's ASCII-default `\d`/`\w`/`\s` in some non-ASCII corners; the native uses
  ASCII-default classes to match Java and a parity battery confirms HotSpot
  output, but the gate lets an app fall back if it hits a regex-feature gap.
