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

### Layer C bug #2 — ROOT-CAUSED + FIXED (2026-06-15): JIT virtual-dispatch bail resolved on the static call-site class

Bug #2 was **not** a miscompile of `search`'s body, nor a JIT→JIT register/ABI
fault. It was a **dispatch-resolution** bug in the JIT virtual-call runtime
helper. Bisection (`CRATONVM_JIT_BISECT_SKIP`) pinned it to the
`Matcher.search` ↔ `Pattern$Start.match` pair (skipping *either* fixes it;
skipping any other regex node does not). Disasm of compiled `search` showed the
`root.match(this, from, text)` call site goes through `jit_invoke_virtual_mic`
with a 4-element arg slice `[receiver, matcher, from, text]`.

Chain:
1. `root.match` is statically typed `Pattern$Node`; the receiver is a
   `Pattern$Start`. Its compiled entry needs the hidden ctx register, so the
   call is "4 args **with ctx**".
2. `try_call_compiled_entry`'s register tables only cover ≤3 with-ctx args, so it
   returns `None` → the helper falls to `bail_to_interpreter`.
3. **The bug:** `bail_to_interpreter` called `invoke_or_native(info.class_name,
   …)` — and for a *virtual* call `info.class_name` is the **static** call-site
   type `Pattern$Node`, not the receiver's runtime class `Pattern$Start`.
   `invoke_or_native` binds to the class name it is handed (it does NOT
   re-dispatch on the receiver), so it ran the **concrete base**
   `Pattern$Node.match` — an unconditional zero-width "accept" (`matcher.last =
   i; return true`). That accepts at every position, so `find()` reports a
   zero-width match before every character and `replaceAll("[.]","/")` yields
   `/o/r/g/...`.

Why "both compiled" was required: only when `search` is compiled does the call
route through `jit_invoke_virtual_mic` (→ overflow bail). When `search` is
interpreted it uses the interpreter's own receiver-resolving invoke; when
`Start.match` is interpreted there is no compiled entry to overflow on, so the
helper takes its receiver-resolved cold path. Both correct — only the
compiled→compiled register-overflow bail hit the static-class path.

**Fix** (`vm/src/jit/helpers.rs`): `bail_to_interpreter` now resolves the
dispatch class from the **receiver's runtime class** for virtual/interface kinds
(`invoke_kind` 0/2) via the new `virtual_dispatch_class` helper — mirroring the
receiver resolution `jit_invoke_virtual_mic`'s cold/miss paths already use
(array→`Object`, synthetic-id→static fallback). Statically-bound kinds
(invokespecial=1 → `invoke_special_shared`; invokestatic=3 → static class) are
unchanged. This also covers the exception-reroute bail in the MIC hit path.

Result: `Matcher.search` now **compiles correctly** under
`CRATONVM_JIT_VIRTUAL_TIERUP`; the skip-list ban is **removed**. `RegexBench`
returns `org/junit/jupiter/...` and runs at interpreter speed or better
(replaceAll x2000: 1593 ms compiled-correct vs 5742 ms corrupt vs 1582 ms
interp). bt16/bt18 golden hold gate-OFF and B-ON.

### Remaining
1. **Flip `CRATONVM_JIT_VIRTUAL_TIERUP` default-ON + full-suite re-test** — bug #2
   (this dispatch bug) and the codePointAt precise-ON crash (fix #1) are both
   fixed. Remaining gate is a full WildFly-suite re-test on an uncontended
   machine to confirm no other per-method compiled-instance miscompiles surface
   once the whole instance-method surface compiles.

Run the WildFly Arquillian client with `CRATONVM_JIT_VIRTUAL_TIERUP=1` — regex is
now correct and fast with `search` compiled, and static + instance hot paths
benefit from layers A/B. (The codePointAt precise-ON fix #1 is independent — it
makes the precise-maps path safe for compiled instance methods that hit the
inline cascade, a prerequisite for any future B+precise default-on.)

## Update (2026-06-22): opt-in fast Rust-regex native for `String.{replaceAll,replaceFirst,matches}` (SBR-02)

Even with all of the above (virtual-tierup default-ON, layers A/B/C fixed), the
real-JDK `java.util.regex` engine running interpreted/partially-compiled is still
**~100–125× slower** than HotSpot for `String.replaceAll`-in-a-loop. SBR-02
(Spring Boot `MinRegexProbe` / `RegexLoopProbe`, mirror of the hanging
`PluginXmlParserTests`) measured ~0.78 ms/iter for `input.replaceAll("\\{@code
(.*?)}", "`$1`")` (HotSpot ≈ 0.002 ms/iter). The remaining gap is the
interpreted Matcher/Pattern inner loop itself — a JIT-codegen-quality problem that
is open-ended to close.

CratonVM already ships a **fast, cached Rust regex native** for these methods
(`native-builtins/src/lang_string.rs::native_string_{replace_all,replace_first,
matches}`, backed by the `regex` / `fancy-regex` crates with a bounded
`(pattern,flags)` compile cache). It was previously **dormant in real-JDK mode**:
it is only wired up by `register_synthetic_overrides` (`#[cfg(feature =
"synthetic-jdk")]`, compiled out of the default CLI), so the real-JDK build ran
the JDK bytecode and never reached the native.

**Fix (opt-in, default-OFF):** `CRATONVM_NATIVE_STRING_REGEX=1` routes
`String.replaceAll` / `replaceFirst` / `matches` to the cached Rust native.
- `env_cache::native_string_regex()` (default-OFF gate; `cached_is_set!`).
- `register_essential_natives` (the real-JDK registration path) now registers the
  three String regex natives **when the env is set** — so with the gate off they
  stay unregistered and the real bytecode runs (default byte-identical).
- `force_native_over_real_jdk_bytecode` (interpreter.rs) returns `true` for those
  three (class, method, descriptor) triples when the gate is on, so the registered
  native **wins** over the JDK bytecode at dispatch.
- The native's replacement expansion was made **Java-faithful**
  (`JavaRegex::replace_{all,first}_java` + `parse_java_replacement`): `$N`
  (digit-bounded by group count, so `$10` with 2 groups = group 1 then literal
  `0`), `${name}`, and `\`-escapes (`\$`→`$`, `\\`→`\`) — the engine's own
  `$$`/greedy-`$NN`/no-`\`-escape syntax would otherwise diverge from
  `Matcher.appendReplacement`.

**Measured (JDK 25 boot; `scratch/regexperf`, clean machine):**

| benchmark | gate-OFF (real Java) | gate-ON (native) | HotSpot | speedup vs OFF |
|-----------|----------------------|------------------|---------|----------------|
| `MinRegexProbe code` `replaceAll` ×N | 0.785 ms/iter | **0.0115 ms/iter** | 0.002 ms/iter | **~68×** |
| `MinRegexProbe` N=200000 wall | never finishes / minutes | **2.3 s** | 0.41 s | — |

A 30-case `RegexParity` battery (literal/group/named/zero-width/greedy-reluctant/
split/backref/lookahead, replace + replaceFirst + matches) is **byte-identical to
HotSpot** both gate-OFF and gate-ON for ASCII/Latin input. Java-faithful
replacement covered by `native-builtins` unit tests (`java_replacement_tests`).

**Why opt-in (not default-ON):** the Rust `regex` engine's Perl classes are
**Unicode by default**, whereas Java's `\d` / `\w` / `\s` / `\b` are **ASCII-only**
unless `UNICODE_CHARACTER_CLASS` is set. So `"٣".matches("\\d")` is `false`
under HotSpot but `true` via the native (Arabic-Indic digit). This only affects
**non-ASCII** input to `\d`/`\w`/`\s`/`\b`; the SBR-02 / Javadoc-tag patterns
(`\{@code (.*?)}`, `<a href=...>`, …) don't use those classes, so they are exact.
Keeping it opt-in preserves the real-Java-first default. Closing the
`\d`/`\w`/`\s`/`\b` ASCII-parity gap in `translate_java_regex` (only when
`UNICODE_CHARACTER_CLASS` is absent) is the prerequisite for any future default-ON.

**Secondary finding (out of scope for SBR-02):** `RegexLoopProbe` (the exact
`PluginXmlParser.format` chain) is, after this fix, bottlenecked on its **8 literal
`String.replace(CharSequence,CharSequence)` calls** (~1.17 ms/iter for the chain),
not regex — a general interpreter-throughput issue for that non-regex method (no
native exists for the `(CharSequence,CharSequence)` overload). The real
`PluginXmlParser.format` runs per-plugin (hundreds of calls), where ~1 ms/call is
tolerable; the *hang* was the regex recompile-per-call, which this fix removes.
Routing literal `String.replace(CharSequence,CharSequence)` to a Rust native
(byte-identical, zero regex-semantics risk) is a clean follow-up.

**Secondary finding — RESOLVED (SBR-02 follow-up).** Added
`native_string_replace_charseq` (`native-builtins/src/lang_string.rs`): reads
this/target/replacement and returns `ctx.create_string(&s.replace(&target, &repl))`.
Registered for `String.replace(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;`
in `register_essential_natives` and routed via `force_native_over_real_jdk_bytecode`,
both **under the same `CRATONVM_NATIVE_STRING_REGEX` gate** as the regex natives
(default-OFF → real Java bytecode is still the default). Rust `str::replace` is
byte-identical to Java's literal overload: non-overlapping, left-to-right, and an
empty target inserts the replacement at every position (`"abc".replace("","-")` →
`-a-b-c-` in both). Validation (`scratch/regexperf/`, JDK 25):
- gate-ON native is **byte-identical to the real-JDK bytecode (gate-OFF)** across
  empty-target, consecutive/overlapping, replacement-contains-target, not-found,
  and unicode cases (`ReplaceParity.java`);
- full HotSpot parity on all ASCII cases (the only HotSpot diff is the pre-existing
  Windows-console charset rendering of non-ASCII — CV's replace result is correct);
- `Split replace` N=200000: gate-OFF 182065ms → gate-ON 8702ms (**~21× faster**),
  identical `last.len=158`. The `PluginXmlParser`/`RegexLoopProbe` chain is now
  fully fast under the gate.

Note: the `(char,char)` overload already has its own unconditional native
(`native_string_replace`) and is unaffected. A separate synthetic-mode native for
the `(CharSequence,CharSequence)` overload also exists
(`phases_early::register_core_stdlib_extras`, reached only via
`register_synthetic_overrides`); the new gated registration is the real-JDK-mode
counterpart.

## Update (2026-06-22 final): ASCII-class parity closed → gate flipped **default-ON**

The two prerequisites the "opt-in" sections above called out (ASCII Perl-class
parity + a literal-`replace` native) are now both done, so
`CRATONVM_NATIVE_STRING_REGEX` is **default-ON** (opt-out `=0` / `false`). The
"default-OFF / opt-in" wording in the two preceding 2026-06-22 sections describes
the *first* increment (committed separately) and is superseded here.

**ASCII-default Perl classes** (`ascii_perl_classes` in `native-builtins/src/lib.rs`,
applied by `compile_java_regex` whenever `UNICODE_CHARACTER_CLASS` is absent — i.e.
always for `String.{replaceAll,matches}`). Java's `\d`/`\w`/`\s`/`\b` are ASCII-only
by default; the `regex` crate's are Unicode. Rewrite: `\d`→`[0-9]`, `\D`→`[^0-9]`,
`\w`→`[a-zA-Z0-9_]`, `\W`→`[^a-zA-Z0-9_]`, `\s`→`[ \t\n\x0B\f\r]`, `\S`→`[^…]`,
`\b`/`\B`→`(?-u:\b)`/`(?-u:\B)`. Negated forms expand to *Unicode-mode* negated
classes (`[^0-9]` matches any scalar except an ASCII digit = Java's `\D`, and stays
valid-UTF-8-safe, unlike `(?-u:\D)` which matches a non-ASCII *byte*). Inside a
`[...]` class the positive forms expand to bare ranges (`[\d.]`→`[0-9.]`); the rare
inside-class negated forms are left untouched (documented narrow residual). After
this, `"٣".matches("\\d")` is `false` (== HotSpot) and `\d+`/`\w+`/`\s`/`\b` all
behave ASCII-style.

**Validation (default-ON):** a 46-case `RegexParity` battery — now including
non-ASCII `\d`/`\w`/`\s`/`\b` and 7 literal-`replace` cases — is **byte-identical to
HotSpot** both default-ON and opt-out (`=0`); opt-out is also byte-identical to the
baseline dev binary (default truly unchanged when disabled). 8
`java_replacement_tests` pass (incl. `ascii_perl_classes_parity`,
`ascii_word_boundary`). `MinRegexProbe` ~2.3 s (was never-finishes); the
`PluginXmlParser.format`/`RegexLoopProbe` chain (regex + 8 literal `replace`) is now
fully fast.

**Residual risk (why the opt-out exists):** the Rust engine still differs from
`java.util.regex` on some advanced features — possessive quantifiers, certain
`\p{…}` property names, Unicode case-folding edge cases — which fall back to
`fancy-regex` or a `PatternSyntaxException`. `CRATONVM_NATIVE_STRING_REGEX=0`
restores the exact real-JDK engine for any app that hits such a gap.
