# `spring-boot-devtools` residual failure cluster

**Status: FIXED/RETIRED 2026-07-18**

## Resolution

The five originally reported DevTools failures are closed. Three had already
been resolved on the current `dev` baseline before this closure run:

- `RemoteUrlPropertyExtractorTests` (Logback `Logger` context construction)
- `ChangeableUrlsTests` (jar manifest `Class-Path` resolution)
- `RestartClassLoaderTests` (loader/resource identity and cache scope)

This change resolves the two remaining causes:

1. Derby's `InternalDriver.connect(String, Properties, int)` used a temporary
   executor only to enforce its login timeout. On CratonVM, real embedded
   in-memory startup can exceed Hikari's 30-second deadline even though the
   connection itself succeeds. The JDBC bridge now performs the real Derby
   `getAttributes` and `getNewEmbedConnection` sequence synchronously for
   `jdbc:derby:memory:` URLs only. Non-memory URLs retain Derby's normal
   timeout path. Preserving `getAttributes` is essential: it carries URL
   attributes such as `;create=true` into the connection factory.
2. `aastore` could receive an unboxed primitive from native/reflection
   boundaries despite the bytecode verifier's reference contract. The heap
   then retained an internal autobox sentinel instead of a Java object, which
   serialization exposed as `unknown_4294967295`. Both interpreter paths now
   materialize the appropriate real primitive wrapper before storing into a
   reference array.

## Verification

Used the unique release binary
`C:\craton\cratonvm-springboot-devtools-closure-20260718-019f7680.exe`
(SHA-256 `2D5D94FFE97BBF4ABBAB15F0FCB5832BC6B576B3A0681A30B2DE3E180E744FBC`)
and the shared Spring Boot fixture at
`C:\craton\CratonVM\apps\spring-boot`.

All five classes pass in both modes:

| Mode | Result | Wall time |
|---|---|---:|
| JIT | 5/5 classes, 45/45 tests pass | 75.225s |
| `--nojit` | 5/5 classes, 45/45 tests pass | 77.101s |

The JIT Derby class completed 11/11 in 61.043s; the `--nojit` run completed
it in 65.598s. `HttpRestartServerTests` passes in both modes, confirming the
serialization boundary no longer creates an invalid class sentinel.

Result directories:

- `apps\spring-boot-suite-runner\.suite\devtools-closure-20260718-019f7680-release-jit2`
- `apps\spring-boot-suite-runner\.suite\devtools-closure-20260718-019f7680-release-nojit`

`cargo check -p cratonvm-native-builtins --lib` also completed successfully.

## Retired scope

| Module | Class |
|---|---|
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.RemoteUrlPropertyExtractorTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.autoconfigure.DevToolsPooledDataSourceAutoConfigurationTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.restart.ChangeableUrlsTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.restart.classloader.RestartClassLoaderTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.restart.server.HttpRestartServerTests` |
