# H2 — missing `SecureRandom` SHA1PRNG provider (fatal)

## Status
**FIXED** (dev `ec4f328`, 2026-06-05) — native interception of the `SecureRandom.getInstance(String)` / `(String,String)` / `(String,Provider)` / `getInstanceStrong()` static factories (`native-builtins/src/securerandom.rs`), returning a real OS-CSPRNG-backed `SecureRandom`. Verified: **0** `no SecureRandom` occurrences in `TestScript --nojit` and a 240 s `TestAll --nojit` run (was the terminal failure at ~167 s).

## Severity
**HIGH / fatal** — stops `org.h2.test.TestAll` entirely (~163–167 s into the run).

## App / suite
- **App:** H2 Database (`apps/h2database`)
- **Suite:** `org.h2.test.TestAll`
- **Command:** see `continue_prompt_h2_testall.md` / `test-infra/run-four-apps-suite.sh`
- **Logs:** `test-infra/suite-results/apps-four-20260604-233229/h2-testall.log`, `test-infra/suite-results/h2-retest.log`

## Symptom

```
Error in thread "main" runtime error: not implemented: no SecureRandom SHA1PRNG implementation in any provider
```

The suite exits with **rc=1** immediately after this message. Tests that run before the fatal error may still show other failures (column count, lock timeouts).

## HotSpot behavior

On the same classpath and command, HotSpot also exits rc=1 but **earlier** on an unrelated missing dependency (`org.postgresql.jdbc.PgConnection` in `TestPgServer`). HotSpot never reaches the SHA1PRNG code path in this bounded run.

## CratonVM behavior

CratonVM runs further into `TestAll` (~167 s wall) then hits the unimplemented SHA1PRNG path and aborts. No fallback provider is registered.

## Root cause (suspected)

CratonVM’s JCA / `SecureRandom` SPI layer does not register an implementation for algorithm name **`SHA1PRNG`**. H2 (and other legacy code) explicitly requests this algorithm by name rather than using the platform default.

**Suspect areas:**
- `java.security.Security` provider registration
- `SecureRandom.getInstance("SHA1PRNG")` native or JDK-delegation path
- SunJCE / SUN provider stubs

## Impact

- Full H2 upstream regression suite cannot complete.
- Any application that hard-codes `SHA1PRNG` (common in older Java crypto and test code) will fail similarly.
- Basic JDBC smoke (`H2Probe`, `jdbc:h2:mem:`) **passes** — this is not a general JDBC blocker.

## Reproduce

```bash
cd apps/h2database/h2
CV=target-bench/release/cratonvm.exe
JDK="C:/Program Files/Java/jdk-25"
CP="temp;ext/jts-core-1.19.0.jar;..."   # full CP in continue_prompt_h2_testall.md

"$CV" --java-home "$JDK" -Xmx1g -cp "$CP" org.h2.test.TestAll
# wait ~160s → fatal SHA1PRNG
```

Minimal probe (if not already present):

```java
import java.security.SecureRandom;
public class Sha1PrngProbe {
    public static void main(String[] a) throws Exception {
        SecureRandom r = SecureRandom.getInstance("SHA1PRNG");
        System.out.println("OK " + r.getAlgorithm());
    }
}
```

## What to fix

1. Implement or delegate `SecureRandom` for `"SHA1PRNG"` (match JDK 25 behavior — typically Sun provider).
2. Re-run `TestAll`; confirm suite progresses past the prior fatal point.
3. Watch for follow-on failures (column count, MVStore locks) documented separately.

## Related

- [bug-h2-prepared-statement-column-count.md](bug-h2-prepared-statement-column-count.md)
- [bug-h2-mvstore-sys-lock-timeout.md](bug-h2-mvstore-sys-lock-timeout.md)
- Index: `apps/h2database/CRATONVM_BUGS.md`
