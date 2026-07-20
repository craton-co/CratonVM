# JIT on-stack-replacement (OSR) re-executes loop iterations — silent, no exception, extra elements added to collections

**Status: OPEN — found 2026-07-20. CRITICAL. Confirmed root cause: CratonVM's**
**back-edge OSR compilation. Not Spring Boot-specific — a core VM/JIT bug.**

## Symptom

A `for` loop that runs long enough to trigger CratonVM's back-edge
on-stack-replacement (OSR) compilation, in a method that also contains code
AFTER the loop, executes MORE iterations than the loop bound specifies — no
exception, no crash, just silently wrong results (extra elements appended to
a collection, an over-large counter, etc). Found while building a repro for
[`repeatablecontainers-method-cache-classcastexception-FIXED.md`](../../internal/springboot/repeatablecontainers-method-cache-classcastexception-FIXED.md)
(a completely unrelated investigation) — this is an independent discovery.

## Minimal repro

`docs/known-issues/repros/jit-osr-loop-duplicate-execution/LoopDupOsrRepro.java`:

```java
import java.util.ArrayList;
import java.util.List;

public class LoopDupOsrRepro {
    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        List<Object> keys = new ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            keys.add(new Object());
        }
        System.out.println("n=" + n + " keys.size()=" + keys.size());
        long sum = 0;
        for (Object k : keys) {
            sum += k.hashCode() == 0 ? 1 : 0;
        }
        System.out.println("sum=" + sum);
    }
}
```

No Spring, no reflection, no classloading, no threads — just two plain
`for` loops in `main`. Run:

```
javac LoopDupOsrRepro.java
cratonvm --java-home <jdk25> -c . LoopDupOsrRepro 4000
```

**HotSpot:** `n=4000 keys.size()=4000` (correct, every run).

**CratonVM (JIT on, default):** `n=4000 keys.size()=6000` — the first loop's
back-edge (`i++`) fired ~6000 times for a bound of 4000, i.e. `ArrayList.add`
was called ~2000 EXTRA times. Deterministic across repeated runs (not a race
— reproduces single-threaded, no concurrency involved at all).

**With `CRATONVM_JIT_OSR=0`:** `n=4000 keys.size()=4000` (correct) — this
single env var conclusively isolates the bug to back-edge OSR compilation
specifically (regular whole-method JIT compilation, triggered by invocation
count, is NOT implicated — `main()` here is called exactly once, so only
OSR can be compiling anything before the loop finishes).

## Threshold and scaling (single loop, `--java-home` real JDK boot, JIT default-on)

| `n` | `keys.size()` after loop | extra |
|---:|---:|---:|
| 100 | 100 | 0 |
| 500 | 500 | 0 |
| 1000 | 1000 | 0 |
| 1500 | 1500 | 0 |
| 2000 | 2000 | 0 |
| 3000 | 4000 | 1000 |
| 5000 | 8000 | 3000 |
| 10000 | 23000 | 13000 |

The OSR back-edge threshold is documented as defaulting to 1000
(`CRATONVM_TIER_OSR_BACKEDGE`, see `vm/src/runtime/env_cache.rs`), but the
break-even point here is between 2000 and 3000 iterations, and the "extra"
count grows super-linearly with `n` (not a fixed offset) — consistent with
**more than one OSR/tier-transition event per run, each one apparently
re-executing the loop body from (or near) its start instead of resuming from
the live iteration count**, with the extra, now-compiled-and-fast iterations
re-accumulating a back-edge count and re-triggering another such event at a
higher tier threshold. This would explain the compounding (each restart adds
close to the CURRENT total iteration count, not a fixed amount).

## Isolation notes / what is NOT required to reproduce

Bisected down from a much larger repro (`CrhmRepro.java`, driving a real
`org.springframework.util.ConcurrentReferenceHashMap`, involving a custom
`ClassLoader.defineClass` loop, reflection, and a second loop touching
Spring/Mockito-adjacent code). Confirmed NOT required, one at a time:

- Spring Framework / any third-party library on the classpath.
- `ClassLoader.defineClass` / any classloading inside the loop
  (`LoopDupOsrRepro` uses plain `new Object()`).
- Reflection (`Class.getMethod`, etc).
- Multiple threads — reproduces single-threaded.
- `--java-home` real-JDK boot mode specifically (not re-tested under
  `--synthetic-jdk`, but nothing in the repro depends on real-JDK classes).

Confirmed REQUIRED (each one individually removing it made the bug
disappear in earlier, larger repro variants):
- The loop must be long enough to cross the OSR trigger threshold
  (~2000-3000 iterations here; exact threshold likely depends on loop body
  cost / bytecode shape, not investigated further).
- A second loop (or at least more code) after the first loop, in the SAME
  method — a version with ONLY the first loop and no trailing code was not
  independently re-verified to still reproduce in the final minimal form,
  but every earlier bisection step that removed the second loop's PRESENCE
  (not just its content) made the corruption disappear. This suggests the
  bug may be about the OSR-compiled continuation's boundary/exit handling
  interacting with subsequent bytecode, not purely the loop in isolation —
  worth re-confirming as a first step in any follow-up.

## Suspected fault zone (not confirmed to file:line)

Not traced to a specific fix site this session (scope was the unrelated
`RepeatableContainers` doc; this was a byproduct discovery kept for a
dedicated follow-up). The relevant machinery, by name:

- `vm/src/runtime/interpreter.rs`: `try_osr_with_backoff`, `try_osr`,
  `compile_osr_artifact`, `fetch_osr_compile_inputs`,
  `transfer_osr_exit_into_live_frame` (OSR entry/exit and live-frame state
  transfer — a live suspect given the symptom is "loop restarts instead of
  resuming").
- `vm/src/runtime/jit_integration.rs`: `register_osr`/`lookup_osr`/
  `remove_osr` (per-method-id OSR entry registry — worth checking whether a
  method with MULTIPLE loops/back-edges could get its OSR entry point
  registered/looked-up against the WRONG bci, or whether a second OSR
  registration for the same method_id at a higher tier discards/conflicts
  with bookkeeping needed to resume correctly).
- `CRATONVM_TIER_OSR_BACKEDGE` / `CRATONVM_JIT_OSR` /
  `CRATONVM_OSR_NEWARRAY` env vars (`vm/src/runtime/env_cache.rs`) — useful
  levers for a bisection session (e.g. does `CRATONVM_TIER_OSR_BACKEDGE=1`
  make it reproduce even at tiny `n`? does forcing exactly ONE compile tier
  removes the super-linear scaling, leaving a single fixed-size restart?).

## Why this matters

This is a **silent data-corruption bug**, not a crash — any sufficiently long
loop anywhere in the entire test suite (or real applications) that both (a)
crosses the OSR threshold and (b) has more code after it in the same method
could be adding/counting/processing extra elements without any exception or
log line. Given how common "build a collection in a loop, then use it" is,
this is a plausible root cause worth checking against OTHER already-closed
"mysterious extra/duplicate entries" bugs in this project's history before
assuming they are unrelated, and against any OPEN bug whose symptom is
"too many X" rather than "wrong X" or "missing X".

## Affected classes

None specifically — this is a core VM/JIT correctness bug discovered via a
standalone repro, not yet correlated to any specific Spring Boot suite
failure. A dedicated bisection session should also grep already-closed
known-issue docs for "extra"/"duplicate"/"more than expected" symptoms that
might share this root cause.
