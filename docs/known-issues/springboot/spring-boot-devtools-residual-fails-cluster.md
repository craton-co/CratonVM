# `spring-boot-devtools` residual failure cluster

**Status: OPEN — REGRESSED 2026-07-31.** `ChangeableUrlsTests` and
`RestartClassLoaderTests` are failing/crashing again on the 2026-07-31 full
suite rerun, with **different symptoms** than the ones this doc originally
closed (see the Regression note below). The other three classes in the
original "retired scope" table were not re-verified in this pass.

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

## Regression note (2026-07-31)

Full Spring Boot suite rerun (`craton-rerun-20260731/all-jit`) found both
`ChangeableUrlsTests` (FAIL) and `RestartClassLoaderTests` (CRASH) failing
again. In both cases the failure signature is **not** the one this doc
originally described — these look like newly-introduced, unrelated defects
rather than a reversion of the 2026-07-18 fix itself.

### `ChangeableUrlsTests.urlsFromJarClassPathAreConsidered`

```
Expecting actual:
  [..., file:/.../project%2520space/target/classes/, file:/.../does-not-exist/target/classes/, ...]
to contain exactly (and in same order):
  [..., file:/.../project space/target/classes/, ...]
but some elements were not found:
  [file:/.../project space/target/classes/]
and others were not expected:
  [file:/.../project%2520space/target/classes/, file:/.../does-not-exist/target/classes/]
```

`ChangeableUrls.getUrlsFromManifestClassPathAttribute` (in
`spring-boot-devtools`, real Spring bytecode, not CratonVM native) walks each
manifest `Class-Path` entry: it first tries the raw entry, and if
`new File(referenced.getFile()).exists()` is false, retries with
`URLDecoder.decode(entry, UTF_8)` before giving up and logging it as a
non-existent entry (not added to the result). Two things are wrong in the
actual CratonVM output:

1. `project%20space/target/classes/` should decode to `project space/...`
   (matching the real directory the test created) but instead shows up as
   `project%2520space/...` — i.e. doubly percent-escaped, as though the `%`
   from the still-undecoded `%20` got re-escaped to `%25` rather than the
   whole token decoding to a literal space.
2. `does-not-exist/target/classes/` — a directory the test deliberately never
   creates — ends up in the *urls* result instead of being logged as a
   non-existent entry, meaning the `File(...).exists()` check that should
   gate it out returned `true` (or was skipped) for this entry.

Reviewed both native `URLDecoder.decode` registrations
(`native-builtins/src/deprecated_io_util.rs::url_decode_named` and
`native-builtins/src/phases_early.rs::p52_url_decode_named`, both registered
for `(String, Charset)`) — each is a straightforward correct percent/`+`
decoder by inspection, so the defect is more likely in how the two
`new URL(jarUrl, entry)` resolutions interact across loop iterations (e.g.
some shared/stale state between the raw-entry attempt and the decoded-entry
attempt) than in the decoder itself. Needs a targeted repro to pin down
further — not root-caused yet.

### `RestartClassLoaderTests` (CRASH)

No test output at all — the whole process aborts during setup/execution:

```
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error:
class format error in
org/springframework/boot/devtools/restart/classloader/Sample: defineClass1:
not a valid class file (bad magic)
```

This class's `getUpdatedClass` test deliberately calls
`Class.forName(..., reloadClassLoader)` on a `ClassLoaderFile` whose bytes are
`new byte[10]` (all zero — invalid magic), via
`RestartClassLoader.findClass` → `ClassLoader.defineClass(String,byte[],int,int)`
→ the JDK-internal `defineClass1` native
(`native-builtins/src/lang_system.rs::native_classloader_define_class1`,
which calls `validate_classfile_header` and returns
`define_class_format_error` on bad magic). The test expects this to surface
as a normal, catchable `java.lang.ClassFormatError`
(`assertThatExceptionOfType(ClassFormatError.class).isThrownBy(...)`), but on
CratonVM it instead unwinds all the way to the top-level `main-vm run()` and
kills the whole process — i.e. the exception is not being delivered through
the interpreter's normal per-thread exception dispatch when raised this deep
under `Class.forName` → custom classloader → `defineClass`. The 2026-07-18
verification run passed this exact test (`getUpdatedClass`, part of the
45/45 total), so this is a regression, but the mechanism is unrelated to the
originally-documented "loader/resource identity and cache scope" fix — it
looks like a newer defect in how a `ClassFormatError` thrown from deep inside
`defineClass1` propagates. Not root-caused further within this pass.
