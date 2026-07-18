# Jackson/Json Mixin Module Entries AOT TestCompiler mismatch — fixed

**Status: FIXED — 2026-07-18**

## Symptom

Both Spring Boot AOT test classes failed all three assertions under CratonVM:

- `module/spring-boot-jackson`:
  `JacksonMixinModuleEntriesBeanRegistrationAotProcessorTests`
- `module/spring-boot-jackson2`:
  `JsonMixinModuleEntriesBeanRegistrationAotProcessorTests`

The two generated-context assertions observed that the original `@Bean`
factory had been invoked again (`scanningInvoked == true`), and the runtime-hint
assertion found the expected mixin reflection hint absent.

## Root cause

This was the same reflective descriptor loader-identity defect fixed by
`38e386992` (`Fix Spring AOT descriptor loader identity`), not a separate AOT
generation or `javac` divergence.

`TestCompiler` executes generated AOT code through a forked class loader. When
CratonVM constructed reflective method descriptors for the configuration's
factory method, `descriptor_to_class_mirror_via_loader` could resolve a
same-named return/parameter class from the global application namespace rather
than the defining forked-loader namespace. Spring's AOT registration logic then
compared unequal `Class` identities and did not apply the precomputed mixin
registration contribution. The fresh context consequently executed the real
factory method and re-scanned the classpath.

The existing repair consults the defining-loader side table, checks that exact
loader namespace, and invokes that loader's `loadClass(String)` before retaining
the legacy global fallback. It applies to reflective method, field, and
constructor descriptor resolution.

## Regression boundary and validation

On Azure (`victor@20.83.144.174`), the Spring Boot fixture classpaths were
regenerated with `/home/victor/jdk25` before every run. The pre-fix binary
`/data/bin/cratonvm-sb-graphql-getpackage-20260718` reproduced the original
Jackson class result exactly: `tests=3 failed=3`, with both
`scanningInvoked` assertions true and the missing runtime-hint assertion.

The task-specific release binary built from `38e386992`,
`/data/bin/cratonvm-jacksonmixin-aot-fix38-20260718-019f7428`
(`SHA-256 46da593b7ad724b65715b6e21bb42b278a11b667fc53d2819ec736110dc209cb`),
passed the complete focused matrix:

| Class | JIT | `--nojit` |
|---|---:|---:|
| `JacksonMixinModuleEntriesBeanRegistrationAotProcessorTests` | 3/3 | 3/3 |
| `JsonMixinModuleEntriesBeanRegistrationAotProcessorTests` | 3/3 | 3/3 |

Logs are retained under
`/data/tmp/jacksonmixin-aot-20260718-019f7428-baseline/` on the validation
host. The original record is retired because both affected sibling paths and
both CratonVM execution modes now pass across the demonstrated pre/post-fix
boundary.
