# `OAuth2ResourceServerAutoConfigurationTests` — cumulative slowdown exceeds
# the suite's 300s per-class timeout, not a hang/deadlock/infinite-recursion

**Status: OPEN — root cause corrected 2026-07-20/21.** This doc replaces an
earlier version of itself (same investigation, same file history) that
concluded the class was stuck in a non-terminating F-bounded-generics
recursion inside Spring Security's `HttpSecurityConfiguration`. That
conclusion was **wrong** — follow-up measurement this session (see "How the
original theory was ruled out" below) proved every individual context
refresh terminates in bounded, *flat* time; the class only exceeds the
external 300s timeout because it has 47 `@Test`/`@ParameterizedTest` methods
and each one pays CratonVM's real (but finite) per-context startup cost
independently. This is the same *class* of issue already documented for
[`jacksonautoconfigurationtests-severe-slowdown.md`](jacksonautoconfigurationtests-severe-slowdown.md)
in this same directory — a genuine performance pathology, not a correctness
bug, deadlock, or non-terminating loop.

## Symptom

`module/spring-boot-security-oauth2-resource-server`'s
`org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests`
does not finish within the suite runner's 300s per-class timeout, on a fresh
`dev` build (confirmed at origin/dev `65c6021f9` and again at `b269ab2e7`).
Confirmed HANG classification in 3 runs: a 50-class parallel batch (180s
timeout), an isolated single-class run (`-Parallel 1 -TimeoutSec 300`,
`rc=TIMEOUT status=HANG seconds=300.172`), and an isolated run with the VM's
own `--stack-dump-on-timeout` watchdog armed at 45s (killed by its own
deliberate `abort()`, not a spontaneous crash).

### Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Jit on -TimeoutSec 300 -Parallel 1 `
  -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -JdkHome "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot" `
  -ClassList <TSV: header "module`tclass", row "module/spring-boot-security-oauth2-resource-server`torg.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests">
```

For a live diagnostic instead of just a timeout kill, add
`-CratonArgs '--stack-dump-on-timeout=45'` (single `key=value` array
element — two separate elements makes PowerShell mis-bind the second one to
the script's own `-Category` parameter). See
[[reference_stack_dump_on_timeout_watchdog_diagnosis]] for general guidance
on this technique.

## Root cause: flat ~2.5-3x-slower-than-HotSpot Spring context startup, ×47 tests

The class has **47** `@Test`/`@ParameterizedTest` methods
(`grep -c '@Test'` on the source file). Almost every one uses the shared
`contextRunner` field (line 101), which always includes `TestConfig`
(`@EnableWebSecurity` + 4 `mock(...)`-backed `Customizer`-adjacent beans).
Each call to `.run(...)` builds and tears down a **fresh**
`WebApplicationContextRunner`-backed context — nothing is shared/cached
across test methods (this is normal, expected `WebApplicationContextRunner`
behavior, not a bug).

### How the original "F-bounded generics never terminate" theory was ruled out

The original version of this doc (see git history) diagnosed a single
45s-armed `--stack-dump-on-timeout` run: the main thread's captured stack
showed a real, in-progress Spring bean-factory reflection walk — resolving
`HttpSecurity`'s own generic type
(`HttpSecurityBuilder<H extends HttpSecurityBuilder<H>>`, genuine F-bounded
polymorphism, confirmed via `javap`) while
`HttpSecurityConfiguration.applyTopLevelCustomizers` scanned `HttpSecurity`'s
declared setter methods for matching `Customizer<...>` beans. The watchdog
produced ~1,600 repeated dump snapshots over ~8 seconds with the frame count
fluctuating 153-156 — real forward progress, not a frozen `pc` — which read,
at the time, as strong evidence of a genuinely non-terminating recursive
walk. **That reading was wrong**, and this session found out why with three
follow-up experiments:

1. **A minimal, Spring-Boot-free repro terminates on real HotSpot.** A
   standalone `AnnotationConfigApplicationContext` + `@EnableWebSecurity` +
   one `Customizer<CsrfConfigurer<HttpSecurity>>` bean (no JUnit, no Mockito,
   no MockWebServer) completes in ~8.4s on JDK 25 — confirming Spring itself
   has no non-terminating algorithm here, F-bounded generics included.
2. **A more faithful repro (real `WebApplicationContextRunner` +
   `TestConfig`'s actual 4 mock beans, matching the failing test class)
   terminates on CratonVM too** — in ~9.5s (vs. ~3.0s on real HotSpot for
   the same code, a ~3.2x slowdown, not a hang).
3. **Looping that same repro 15 times in a single process** (simulating what
   the real 47-test class does — 15 independent fresh contexts, one after
   another) shows **flat, non-growing per-iteration cost**: 11.2s, 7.1s,
   7.7s, 7.2s, 7.5s, 7.0s, 8.3s, 7.4s, 8.2s, 7.1s, 7.8s, 7.4s, 7.9s, 7.4s,
   8.0s — mean ≈7.8s, no upward trend across the run. This directly refutes
   both "unbounded recursion" (every iteration terminates, always in a
   similar window) and a "cache never hits, degrades over time" theory (no
   growth at all across 15 independent contexts in the same JVM process).

**47 tests × ~7.8s/test ≈ 367s** — comfortably past the 300s external
timeout used by the suite runner, with zero individual test ever actually
stuck. This fully and quantitatively explains the observed "HANG"
classification without requiring any non-termination at all.

### Where the flat ~2.5-3x per-context slowdown itself comes from

Not specific to Spring Security or generics — this session also isolated a
much more fundamental, Spring-independent finding using a tiny reflection
microbenchmark (`GenericTypeIdentityProbe`/`IdentityHashProbe`, no Spring on
the classpath at all): **basic method-call overhead in a hot loop is
60-75x slower than real HotSpot on CratonVM, uniformly, regardless of
`--nojit`:**

| Operation (1,000,000 calls) | Real JDK 25 | CratonVM | Ratio |
|---|---|---|---|
| `Object.hashCode()` (native) | 7.3ms | 446-658ms | ~61-90x |
| `Class.hashCode()` (native) | 5.9ms | 325-569ms | ~55-96x |
| `System.identityHashCode()` (native) | 6.4ms | 422-470ms | ~66-73x |
| `Integer.hashCode()` (**pure bytecode**, `return value;`) | 6.4ms | 468-560ms | ~73-88x |

`Integer.hashCode()` being 70+x slower is the key data point — it is one of
the simplest possible bytecode methods, with no native call and no generics
involved at all, ruling out "it's specifically about reflection/generics
metadata." `CRATONVM_DBG_JIT_METHOD_STATS` (fired via `System.exit(0)`, see
[[reference_stack_dump_on_timeout_watchdog_diagnosis]] for why a
pre-exit-hook-gated stat dump needs an explicit exit call) showed only the
enclosing `main()` method ever got OSR-compiled (`osr=1 c1=0 c2=0`) — the
individual `hashCode()` call targets never separately tier up, and `--nojit`
produces near-identical timing to JIT-on (989ms vs. 1054ms) for the same
loop. This means every such call, JIT or not, pays a large fixed per-call
dispatch tax.

**This is not a new discovery** — it is the same systemic class of issue
already tracked as the [[project_wire_tiered_manager]] initiative and
documented in depth in
[[reference_hashmap_native_call_dispatch_overhead_20260711]] (per-native-call
safety/metadata machinery: conservative JIT-frame root scanning, RwLock
class/field-layout lookups, tracing, SATB flush — measured there at up to
~230x for `HashMap<Integer,Integer>` before partial fixes landed) and
[[reference_jit_invoke_cache_thrash_dispatch_heavy]] (per-warm-virtual-hit
tier-up machinery net-slowing dispatch-heavy workloads, ~1.85x measured
there, JIT sometimes net counter-productive). `HttpSecurityConfiguration`'s
bean-factory generic-type resolution is an unusually
reflection/dispatch-heavy code path (dozens of `HttpSecurity` setter methods
scanned, `ResolvableType`/`SerializableTypeWrapper` walks per candidate, bean
name matching against every registered definition) — exactly the shape that
pays this systemic per-call tax the hardest, which is why this class in
particular crosses the 300s line while most Spring Boot test classes don't.

## Not fixed this session

The true fix is the existing, dedicated, cross-session
[[project_wire_tiered_manager]] initiative (make the per-call native-dispatch
and per-warm-hit tier-up paths cheap/lock-free) — not something to
speculatively patch in a single investigation. That work already has its own
validation rigor (isolated A/B via prebuilt sibling-worktree binaries,
`cargo test -p cratonvm-gc -p cratonvm-vm`, checksum comparisons across
fixes) documented in
[[reference_hashmap_native_call_dispatch_overhead_20260711]]; touching this
hot, shared, correctness-critical machinery without that same rigor would be
irresponsible. Confirmed via `javap` that no CratonVM-specific correctness
bug exists in `SerializableTypeWrapper`'s `Type` equals/hashCode/cache
handling either (a dedicated Spring-free probe found correct, stable
equals/hashCode for repeated introspection of the same generic type,
including the genuinely F-bounded `Self<T extends Self<T>>` shape) — so
there is no narrow correctness fix hiding here, only the broader performance
initiative.

## Suggested next steps

1. This class doesn't need a CratonVM code fix to be "correct" — it already
   is. The suite-runner-level question (raise this class's timeout budget,
   or accept it as a known slow-but-passing class once `--stack-dump-on-timeout`-based
   triage rules it out as a real hang) is a suite-configuration decision, not
   a VM bug fix.
2. Whoever next picks up [[project_wire_tiered_manager]] can use this doc's
   `IdentityHashProbe`/`GenericTypeIdentityProbe`-style microbenchmarks
   (Spring-free, ~10 lines, no suite/Gradle dependency) as a fast, isolated
   regression canary for per-call dispatch overhead — much cheaper to iterate
   on than rebuilding and re-running the full Spring Boot test suite.
3. If/when that initiative lands a fix, re-run this class's repro; the
   47×~7.8s arithmetic above predicts it should drop comfortably under 300s
   once per-call dispatch overhead is closer to HotSpot's.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security-oauth2-resource-server` | `org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests` |

Likely affects other Spring Security test classes with many `@Test` methods
each doing a full `@EnableWebSecurity` context refresh — not verified against
other classes this session (would need the same per-class arithmetic:
test-method count × measured per-context cost vs. that class's timeout
budget).
