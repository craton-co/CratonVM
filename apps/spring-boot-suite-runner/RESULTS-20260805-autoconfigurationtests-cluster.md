# The five `*AutoConfigurationTests` docs — four were one bug, 2026-08-05

Five known-issue pages were filed on 2026-08-05, all for classes named
`*AutoConfigurationTests`. The question asked was whether they share a cause.
**Four of the five do. The fifth does not, and is now the control that proves
the other four.**

## Result

| Module | Class | 08-05 full suite | Fixed binary | Same bug? |
|---|---|---|---|:-:|
| `spring-boot-flyway` | `FlywayAutoConfigurationTests` | **HANG** 300.1s | **PASS 73/73** | yes |
| `spring-boot-jackson` | `JacksonAutoConfigurationTests` | FAIL 22/69, 32 containers | **PASS 162/162** | yes |
| `spring-boot-amqp` | `RabbitAutoConfigurationTests` | FAIL 42/76 | **PASS 78/78** | yes |
| `spring-boot-tomcat` | `TomcatServletWebServerAutoConfigurationTests` | FAIL 8/18 | **PASS 18/18** | yes |
| `spring-boot-jooq` | `JooqAutoConfigurationTests` | **HANG** 300.0s | **still HANG** (900s Azure / 1200s local) | **no** |

The shared cause is `383e7f5cf` — *a recycled `JitInvokeInfo` address let one
call site serve another's dispatch* — which landed on `dev` at 13:22 -0300 on
08-05, after the full-suite binary was cut. Four different-looking symptoms,
one mechanism: a compiled call site inheriting a recycled site's resolution
and calling the wrong native.

| Class | Filed symptom | What it really was |
|---|---|---|
| Flyway | valid `Bad method descriptor`, `Class`→`PoolEntry` CCE | `Class.cast` returned its RECEIVER |
| Jackson | `AnnotationUtils.findAnnotation` returned bare `null` | a reference-returning reflective site inherited another native's answer |
| Rabbit | `NoSuchMethodError <unknown class 2147484086>.apply` | `VIRTUAL_TARGET_CACHE`: correct method name, previous site's class |
| TomcatServletWebServer | `Elements.getType(int)` returned `null` | same aliasing, one more return value |

**Every one of the four filed hypotheses was wrong** — an HSQLDB stall, an
annotation-proxy native returning null, a lambda-metafactory naming gap, and a
malformed environment-variable-derived property name. Each is recorded as
refuted on its own retired page, with the diagnostic that would *not* have
found it.

## Which of them actually regressed

Worth separating, because "all five regressed" is not what the recorded rows
say:

| Class | 08-02 | 08-05 | Regressed in that window? |
|---|---|---|---|
| Flyway | PASS 220.8s | HANG | **yes** |
| Rabbit | PASS 264.5s | FAIL 42/76 | **yes** |
| TomcatServletWebServer | PASS 78.1s | FAIL 8/18 | **yes** |
| Jackson | **HANG 300.1s** | FAIL 22/69 | no — already broken, differently |
| jOOQ | **HANG 300.2s** | HANG | no — hung in both |

Three regressed cleanly inside the window the aliasing landed in. Jackson was
already failing on 08-02 as a *hang*; the aliasing changed its symptom rather
than creating it. jOOQ was and remains a separate problem.

## Verification

One process per class, runner env vars set, HotSpot control on the identical
classpath. Fixed binary = `dev` @ `96acd76ed` (local) / `origin/dev` (Azure),
both with `383e7f5cf`.

| Class | HotSpot (local) | CratonVM (local) | CratonVM (Azure, load ~30) |
|---|---:|---:|---:|
| Flyway | 9.3 / 11.4s | 214.1 / 220.9 / 253.4s | 224 / 232s |
| Jackson | 18.9s | 308.0s | 336 / 343s |
| Rabbit | 10.2s | 228.1s | 233s |
| TomcatServletWebServer | 6.7s | 112.1s | 54s |
| jOOQ | 7.5s | **TIMEOUT 1200s** | **HANG 900s** |

The doc-specific error strings are absent from every fixed run's stdout and
stderr (`metaAnnotation`/`DisabledCondition`; `<unknown class`/`NoSuchMethodError`;
`isIndexed`/`buildDefaultToString`). Rabbit's two surviving
`class path resource [...]` lines are the fixture's own `[foo]`/`[bar]`
missing-keystore assertions, not the `[null]` of the bug.

## One class still does not fit its budget

`JacksonAutoConfigurationTests` is correct now but takes **336-343s against
the base 300s shard timeout**, so the suite would keep calling it a `HANG`.
It is not stuck — it completes with all 162 tests every time. Added a
documented carve-out (`JACKSON-BUDGET.1`, 900s) in `run-spring-boot-suite.ps1`,
matching how `RabbitAutoConfigurationTests` (900) and
`OriginTrackedYamlLoaderTests` (1100) are already handled.

The carve-out buys an honest verdict, not a fix: 336s against a 22.4s HotSpot
baseline is 15x, and the retired 2026-07-22 page recorded ~225s for the same
162 tests, so this class has also gotten slower. That ratio belongs to the
Spring-bootstrap throughput work.

## jOOQ: what the negative control shows

Same binary, same host, same launcher — still HANGs. Per-line deltas locate
the cost **inside the test body**, between `Start completed` and
`Shutdown initiated`: a *successful* jOOQ test costs 90-175s, while a context
that hits the `org.jooq.conf.Settings` NPE costs ~31 ms. That refutes the
jOOQ page's own guess that the NPE (or retrying around it) is what stretches
each cycle. Its two remaining problems — the 90-175s test body and the
`Settings` NPE — are separable and both still open on
`docs/known-issues/springboot/jooqautoconfigurationtests-timeout-regression-20260805.md`,
which is corrected in this change rather than retired.

## Docs retired

- `jacksonautoconfigurationtests-disabledcondition-npe-20260805.md` → `…-npe-FIXED-20260805.md`
- `rabbitautoconfigurationtests-lambda-nosuchmethoderror-20260805.md` → `…-nosuchmethoderror-FIXED-20260805.md`
- `tomcatservletwebserverautoconfigurationtests-configurationpropertyname-npe-20260805.md` → `…-npe-FIXED-20260805.md`

(`flywayautoconfigurationtests-silent-hang-after-hsqldb-validate-20260805.md`
was retired earlier the same day; see
`RESULTS-20260805-jit-site-cache-aliasing-verify.md`.)

## Reproduce

```bash
ssh -i ~/.ssh/azure.pem victor@20.83.144.174
E=/data/data/wt-flywayfix-20260805/target/release/cratonvm-sbfour
/data/sbrun.sh $E jit module/spring-boot-jackson \
  org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests /tmp/sbf-out 2 900
/data/hsrun.sh module/spring-boot-jackson \
  org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests /tmp/sbf-hs 1
```
