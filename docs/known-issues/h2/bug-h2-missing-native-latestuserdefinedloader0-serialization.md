# `jdk/internal/misc/VM.latestUserDefinedLoader0()` throws `UnsatisfiedLinkError` in a default build — the fix exists in source but is gated behind an opt-in Cargo feature

## Status
**OPEN — build-configuration bug, not a missing implementation.** Found
while investigating H2 suite regressions in the twelfth-pass `TestUpgrade`
session's follow-up full-suite run (2026-07-30/31), then traced back to its
real cause via `git log -S`.

## Severity
**MEDIUM-HIGH** — every real Java-serialization deserialization
(`ObjectInputStream.readObject()`) that resolves an ordinary class (not a
proxy) calls this native from `ObjectInputStream.readClassDesc()` →
`resolveClass()` → `latestUserDefinedLoader()`, in **every** build,
including plain default (`cargo build --release -p cratonvm-cli`, no
extra `--features`) real-JDK-mode builds — this is not a synthetic-jdk-only
code path. Any code that round-trips a `java.io.Serializable` object
through H2 (H2's own `JdbcUtils.deserialize`/`serialize`, used for e.g.
`OTHER`/`JAVA_OBJECT` columns and function results holding
non-`Value`-convertible objects) is affected unconditionally.

## Affected test classes (H2 suite, this run)
- `org.h2.test.jdbc.TestPreparedStatement` (`testSetObject`, deserializing a
  `java.sql.Timestamp` written earlier in the same test)
- `org.h2.test.store.TestObjectDataType` (`testCommonValues`, deserializing
  a `java.sql.Timestamp`/`java.util.Date`)
- `org.h2.test.unit.TestSampleApps` (`org.h2.samples.Function`'s
  `get_x(make_point(10,20))`, deserializing a custom `Point` object stored
  via a Java function call)

All three fail identically and are almost certainly just the first three of
*many* possible triggers.

## Symptom
```
[NativeBridge] 1 unregistered native methods:
  MISSING: jdk/internal/misc/VM.latestUserDefinedLoader0()Ljava/lang/ClassLoader;
...
org.h2.jdbc.JdbcSQLDataException: Deserialization failed, cause:
  "java.lang.UnsatisfiedLinkError: jdk/internal/misc/VM.latestUserDefinedLoader0()Ljava/lang/ClassLoader;"
	at org/h2/util/JdbcUtils.deserialize(JdbcUtils.java:433)
	at java/io/ObjectInputStream.readObject(ObjectInputStream.java:445)
	...
	at java/io/ObjectInputStream.resolveClass(ObjectInputStream.java:745)
	at java/io/ObjectInputStream.latestUserDefinedLoader(ObjectInputStream.java:2438)
	at jdk/internal/misc/VM.latestUserDefinedLoader(VM.java:353)
```

## Root cause — the fix already exists, it's just compiled out
This native was root-caused and correctly fixed once already, in commit
`20519be46` ("Fix Hibernate CacheKeyEmbeddedIdEnanchedTest: deserialization
ignored the caller's classloader", 2026-07-06) — a real stack-walking
implementation (`latest_user_defined_loader_class` in
`native-builtins/src/serialization.rs`, walking `NativeContext::
frame_class_ids()` for the first frame whose loader id is `>= 2`) replaced
what the commit's own message calls "a stub that always returned null."
That commit **is** an ancestor of current `dev` and the code **is** still
present, unmodified, in `native-builtins/src/serialization.rs`.

But the actual `registry.register("jdk/internal/misc/VM",
"latestUserDefinedLoader0", ...)` call in `native-builtins/src/lib.rs`
(~line 15959) is wrapped in:
```rust
#[cfg(any(feature = "experimental-serialization", feature = "synthetic-jdk"))]
registry.register(
    "jdk/internal/misc/VM",
    "latestUserDefinedLoader0",
    "()Ljava/lang/ClassLoader;",
    |ctx, _args| { ... },
);
```
and `native-builtins/Cargo.toml` declares both gating features as
**default-off** (`default = []`, `synthetic-jdk = []`,
`experimental-serialization = []`). A plain `cargo build --release -p
cratonvm-cli` — what the H2 suite runner (and, presumably, any other
default product build) actually uses — therefore compiles this native
**out entirely**, silently reverting to the pre-fix "unregistered, throws
`UnsatisfiedLinkError`" behavior the July 6 commit was written to eliminate.

This gate looks unintentional for this specific native: the fix commit's
own rationale is a plain, general correctness fix for **real-JDK-mode**
`ObjectInputStream` deserialization (the Hibernate bug it fixed has nothing
to do with the synthetic/experimental JDK modes those two features
otherwise gate) — real bytecode calls this native unconditionally in every
build mode. It's most likely that this registration call simply lives
inside a `#[cfg(...)]`-gated block that predates the fix (originally scoped
to synthetic-serialization-specific natives) and the July 6 change edited
the closure body in place without noticing — or without it mattering yet,
since nothing had exercised the default-build gap until now — that the
whole registration was still gated.

## Suggested fix
Move the `jdk/internal/misc/VM.latestUserDefinedLoader0` registration out
from under the `experimental-serialization`/`synthetic-jdk` feature gate so
it's always registered (it has no dependency on anything else those
features guard — `latest_user_defined_loader_class` only uses
`NativeContext::frame_class_ids()`/`loader_id_of_class`, both
unconditionally available). Verify this doesn't accidentally pull in the
rest of `serialization.rs`'s synthetic read/write path (`ois_read_object`
et al.) under a default build — those may have their own, legitimate
reasons to stay opt-in; only this one native needs to move.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.jdbc.TestPreparedStatement
```
Confirm the fix at the source level first (cheaper than rebuilding+running
H2): `strings <cratonvm-bin> | grep latestUserDefinedLoader0` should find
the symbol in a build with the feature(s) enabled; a default `--release`
build should not register it as a runtime `NativeMethodRegistry` entry
(check the `[NativeBridge] N unregistered native methods` startup
diagnostic, or grep the registered-natives dump if one exists).

## Confirmed independently via the Spring Framework suite (2026-07-31) — much larger blast radius than H2 alone

Same day, different suite, same root cause: a full 2847-class Spring
Framework suite run (dev `2f138f04e3`, 4-shard, `apps/spring-suite-runner`,
plain `cargo build --release -p cratonvm-cli` — no extra `--features`, same
default build this doc already describes) hit the identical
`UnsatisfiedLinkError: jdk/internal/misc/VM.latestUserDefinedLoader0()Ljava/lang/ClassLoader;`
on **61 of 90 FAIL classes (68% of that run's entire FAIL count)** — every
one a test whose method name involves Java serialization (`serializable()`,
`canSerializeProxies()`, `cacheExceptionRewriteCallStack()`, etc.), spread
across modules with no other relationship: `spring-aop` (proxy
serialization — `AspectProxyFactoryTests`, `CglibProxyTests`,
`JdkDynamicProxyTests`, `MethodMatchersTests`, `AopUtilsTests`, and 9 more),
`spring-context` (`StaticApplicationContextTests`,
`ComponentScanAnnotationIntegrationTests`, and others), `spring-tx`
(`TransactionInterceptorTests`, `AnnotationDrivenTests`,
`TransactionAttributeSourceAdvisorTests`), `spring-orm` (every
`HibernateEntityManagerFactory*IntegrationTests` and
`EclipseLinkEntityManagerFactory*IntegrationTests` variant's
`canSerializeProxies()`), `spring-context-support` (all of
`JCacheAspectJ*Tests`/`JCacheJavaConfigTests`/`JCacheNamespaceDrivenTests`/`JCacheStandaloneConfigTests`),
`spring-messaging`, `spring-core`, `spring-web`, `spring-webmvc`.

This confirms the doc's own severity assessment ("MEDIUM-HIGH... any code
that round-trips a `Serializable` object") was if anything an
**underestimate** — this is not an H2-specific or even a database-adjacent
gap, it is the single highest-yield native-registration fix currently
available across the whole Spring suite. Fixing the Cargo feature gate
described above would very likely flip most or all of these 61 classes to
OK in one build, since the actual native implementation
(`latest_user_defined_loader_class`) is already correct and unmodified —
this needs zero new Rust logic, only moving the `registry.register(...)`
call out from under `#[cfg(any(feature = "experimental-serialization",
feature = "synthetic-jdk"))]`.

Full affected-class list from this run available in the Spring suite
session's own investigation; not reproduced here in full to avoid
duplicating this doc's existing H2-side detail — the point of this addendum
is blast-radius evidence, not a new investigation.

## Related
`docs/internal/fixed-suite-bugs/hibernate/hib-linux-fail-bucket-triage-20260703.md`
and
`docs/internal/fixed-suite-bugs/hibernate/serializationhelpertest-null-classloader-forname-scope-leak-FIXED.md`
both reference this native from the original Hibernate investigation — the
Hibernate suite's own build/test harness presumably enables one of the two
gating features, which is why that regression was never observed there.
Worth checking whether `experimental-serialization`/`synthetic-jdk` are
enabled by whatever CI/build config produces the "official" product binary
end users actually get, versus what ad-hoc `cargo build --release -p
cratonvm-cli` (used by essentially every suite-runner script referenced
throughout `docs/known-issues/`) produces — if the two differ, this
specific gap may be invisible in whatever build config is actually shipped
but very visible (as it was here) in every suite session's own from-scratch
builds.
