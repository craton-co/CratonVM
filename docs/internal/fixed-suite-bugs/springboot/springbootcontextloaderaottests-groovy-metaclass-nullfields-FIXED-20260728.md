# `SpringBootContextLoaderAotTests` — Groovy `GroovySystem.<clinit>` null-fields report — CLOSED

**Status: CLOSED 2026-07-28.** The reported `CachedClass.getFields()` null
result no longer reproduces on current `dev`; no new runtime source change was
required for this closure.

## Original report

The 2026-07-23 rerun reported an `ExceptionInInitializerError` from
`GroovySystem.<clinit>`, where Groovy's `MetaClassImpl.addFields` attempted to
iterate a null `CachedField[]`. The report correctly ruled out the ordinary
zero-field case and narrowed the suspect path to the privileged lambda around
`Class.getDeclaredFields()` and `Stream.toArray(CachedField[]::new)`, but it
did not establish a CratonVM source file and line as the cause.

## Closure evidence

From current `origin/dev` commit
`614166e94ad30f8f27300914abaaf02385e6e17c`, a fresh, isolated release
`cratonvm-groovy-metaclass-20260728.exe` was built with JDK 25 / MSVC. Its
SHA-256 was
`68FB26D05145FB06EE39F5652D6EBA1370B810AE3F7FBE500350DC723A2E219F`.

Each class ran in a fresh process through the Spring Boot suite runner against
`C:\craton\CratonVM\apps\spring-boot`. Every run emitted an accepted
`SBRUNNER_RESULT` with zero failures, aborted tests, and failed containers.

| Scope | Classes | `-Jit on` | `-Jit off` |
|---|---:|---:|---:|
| Reported AOT regression | 1 | 1/1 | 1/1 |
| Core Groovy configuration regressions | 2 | 2/2 | 2/2 |
| Groovy-template bootstrap regressions | 4 | 40/40 | 40/40 |
| **Total** | **7** | **43/43** | **43/43** |

The explicit residual search covered every compiled Spring Boot suite class
whose name includes `Groovy`, plus the originally reported AOT class. This
exercises the same JVM-wide, first-use Groovy metaclass initialization in 14
independent processes. No related residual remains.

## Resolution

The old failure cannot be reproduced with the current dev-based executable,
and its original report did not identify an unfixed source-level defect.
Retire it from `docs/known-issues`; this closure records the complete current
JIT and interpreter evidence rather than assigning an unproven root cause.
