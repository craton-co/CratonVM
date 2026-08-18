# A relocating young collection leaves live `String` references reading back NULL — `-XX:+UseGenerationalGC` only

**Status: OPEN, reproduced and isolated to the collector 2026-08-18 on `dev`
`24a5d4528`. Not fixed. Two Spring Boot classes fail 18/73 and 11/34 under
Generational and pass 73/73 and 34/34 under the shipped default (ZGC) in the
same session, on the same binary, with HotSpot green on both.**

## The failure

```
java.lang.NullPointerException: Cannot invoke "String.hashCode()" because "<local4>" is null
java.lang.NullPointerException: Cannot invoke "String.equals(Object)" because
    the return value of "java.lang.reflect.Method.getName()" is null
```

Both shapes are a **live reference reading back null**: a `String` local that
was non-null when it was stored, and a `Method`'s own name field. Neither is a
Java-level nullability question — `Method.getName()` cannot return null on any
conforming JDK, and the local is the receiver of the call that dereferences it.

Everything downstream is bean-wiring noise from the same root: Spring catches
the NPE in a `@Bean` factory method and rethrows it as
`UnsatisfiedDependencyException` / `BeanCreationException`, so one lost
reference fails a whole context and every test that asks for it.

## Measured

Azure Linux, one class per process, `--Xmx 2g`, JIT on, real JDK 25 backend,
the three load-bearing suite env vars, 400 s cap. Same binary for both collector
arms — the only difference is the flag.

| Class | HotSpot | ZGC (shipped default) | `-XX:+UseGenerationalGC` |
|---|---|---|---|
| `FlywayAutoConfigurationTests` | ✓73/73, 9 s | ✓73/73, 79 s | **18 FAIL**, 78 s |
| `IntegrationAutoConfigurationTests` | ✓34/34, 11 s | ✓34/34, 85 s | **11 FAIL**, 76 s |
| `Log4J2LoggingSystemTests` | ✓63/63, 9 s | ✓63/63, 51 s | ✓63/63, 72 s |

`UnsatisfiedDependencyException` appears **zero** times in either ZGC log and
64/38 times in the Generational ones, so this is not a flake that happens to
have landed on one arm.

## Why this is the collector and not the JIT

`[moving-young] fallback` peaks at **#3 / #5 / #1** on these three runs. The
death-spiral this workload used to show — peaks of #2048, no completion in 700 s —
is gone (see the retired page under "Related"), which means the young collector
now **actually relocates** on these classes where it used to decline every
attempt and stay non-moving.

That is the whole point. The fallback existed to refuse compaction whenever the
root set could not be verified; with the refusals gone, the compaction runs, and
what it does to an unverified root is now observable. A non-moving collector
(ZGC) cannot expose the bug because it never moves the object, and the same
binary passes there.

**This is the risk the retired page named explicitly** — "the fix might just move
the failure". It did: from a throughput spiral to a wrong answer. A page that
prices a repair by counting fallback cycles is measuring the *refusals*, not the
correctness of what happens when they stop.

## What is NOT established

- **Which root class is lost.** A local slot and a field inside a `Method`
  mirror both read null, so at minimum one precise-map or metadata edge is not
  being updated across relocation. No attempt was made here to attribute it to a
  specific root source.
- **Whether the reference is nulled or stale.** "Reads back null" is what the
  NPE message proves; a stale-but-non-null reference would present differently
  and has not been ruled out elsewhere in the same run.
- **Whether G1 shows it.** Not measured — only the two collectors above were run.

## Reproduction

```bash
# fixture: apps/spring-boot (built test classes + per-module cratonvm-test-cp.txt)
/data/sb4.sh <cratonvm> module/spring-boot-flyway \
  org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests gen 400 \
  -XX:+UseGenerationalGC
# and the same command without the flag for the passing control
```

The NPEs are in the `.out.log`, under the bean-creation stack traces:

```bash
grep -c 'because "<local4>" is null' /data/sb-out/gen/FlywayAutoConfigurationTests.out.log
```

## Related

- retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md —
  the page this was found from. Its mechanism (the fallback spiral) is closed;
  this is what became visible underneath it.
- `docs/known-issues/springboot/quartz-endpoint-web-native-memory-runaway-20260818.md`
  — the other finding from the same re-measurement, collector-independent and
  unrelated to this one.
