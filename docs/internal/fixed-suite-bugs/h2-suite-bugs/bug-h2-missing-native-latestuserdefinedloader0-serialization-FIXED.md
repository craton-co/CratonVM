# FIXED — `jdk/internal/misc/VM.latestUserDefinedLoader0()` threw `UnsatisfiedLinkError` in every default (`-p cratonvm-cli`) build

## Status
**FIXED 2026-07-31**, branch `fix/vm-latestuserdefinedloader0-20260731`
(merged to `dev`). All three reported H2 test classes now produce output
**byte-identical to HotSpot JDK 25**, and **63 of the Spring Framework suite's
90 FAIL classes flipped to OK** in a controlled A/B on the same host. Retired
from `docs/known-issues/h2/`.

## The bug, confirmed
The doc's root cause was correct. `registry.register("jdk/internal/misc/VM",
"latestUserDefinedLoader0", ...)` in `native-builtins/src/lib.rs` carried

```rust
#[cfg(any(feature = "experimental-serialization", feature = "synthetic-jdk"))]
```

and neither feature is in the default set of `cratonvm-vm`
(`default = ["awt", "management"]`) or `cratonvm-cli`. Confirmed at the binary
level before touching anything: `strings` on a `-p cratonvm-cli` release build
from `dev` found **zero** occurrences of `latestUserDefinedLoader0`.

**Why it was gated** — the one detail the original report guessed at and got
slightly wrong. It was not a stale block that the 2026-07-06 fix edited without
noticing. The gate was added *deliberately*, for a real reason, by the commit
that wrote the implementation: the handler calls
`serialization::latest_user_defined_loader_class`, and `pub mod serialization`
is itself `#[cfg(any(experimental-serialization, synthetic-jdk))]`. Without a
matching gate on the registration, `cargo test -p cratonvm-native-builtins
--lib` fails to compile with E0433. The in-source comment even said so — and
then justified the trade with a claim that is false: *"experimental-
serialization, which every real workspace build gets via the vm crate's
defaults"*. It does not; `vm/Cargo.toml` has never had it in `default`.

So the shape to remember is not "someone forgot a cfg". It is: **a helper in a
feature-gated module dragged an ungateable registration into the gate with
it**, and a plausible-sounding but unverified comment about feature defaults
kept anyone from re-checking.

## The fix
1. `latest_user_defined_loader_class` moved from the gated
   `native-builtins/src/serialization.rs` to the always-compiled
   `native-builtins/src/classloader.rs`, with a comment saying why it lives
   there and not to move it back. `serialization.rs` keeps using it via
   `pub(crate) use crate::classloader::latest_user_defined_loader_class;`, so
   the synthetic `ois_read_object` read-path is unchanged.
2. The `#[cfg(...)]` came off the registration. The handler now calls
   `crate::classloader::latest_user_defined_loader_class`. Behaviour is
   byte-identical; only reachability changed.
3. Per the original report's own caution, nothing else moved: the synthetic
   `ObjectInputStream`/`ObjectOutputStream` read/write path stays opt-in. A
   test asserts that (below).

Compiles clean under default, `experimental-serialization`, and
`synthetic-jdk`. `cargo test -p cratonvm-native-builtins --lib` passes in all
three: 3144 / 3287 / (synthetic) tests, 0 failures.

## Verification
Baseline, `dev` binary without the fix — reproduced exactly as reported:

```
Caused by: java/lang/UnsatisfiedLinkError:
  jdk/internal/misc/VM.latestUserDefinedLoader0()Ljava/lang/ClassLoader;
    at java/io/ObjectInputStream.resolveClass(ObjectInputStream.java:745)
    at java/io/ObjectInputStream.latestUserDefinedLoader(...:2438)
```

After the fix (`--java-home /home/victor/jdk25 --nojit`, H2 at
`/data/data/h2database/h2`), all three classes exit 0 with **no**
`UnsatisfiedLinkError`, and stdout `diff`s clean against real HotSpot JDK 25
running the same classpath:

| test class | cratonvm exit | HotSpot exit | stdout diff |
| --- | --- | --- | --- |
| `org.h2.test.jdbc.TestPreparedStatement` | 0 | 0 | identical |
| `org.h2.test.store.TestObjectDataType` | 0 | 0 | identical |
| `org.h2.test.unit.TestSampleApps` | 0 | 0 | identical |

(`TestSampleApps` still logs two `gen_heap::read_slot: corrupt Value cell`
guard hits from the unrelated HIB-CV-32 family; they do not change the output
and HotSpot's stdout matches regardless.)

## The residual the report asked about — answered, and it is the real story
The report's closing question was whether the "official" product build enables
these features while ad-hoc `-p cratonvm-cli` builds do not. **It does, and
that is exactly why this survived so long.**

`libcratonvm/Cargo.toml` and `cratonvm-embed/Cargo.toml` both carry
`experimental-serialization` in their **default** feature sets. Cargo unifies
features across every workspace member built in one invocation, so:

| invocation | feature on `cratonvm-native-builtins`? |
| --- | --- |
| `cargo build --workspace` / `cargo test --workspace` — CI's Build & Test job | **YES**, via `libcratonvm` |
| `cargo build --release -p cratonvm-cli` — every suite runner | **NO** |

Verified with `cargo tree -e features -i cratonvm-native-builtins` run both
ways. `experimental-aot` and `experimental-debug` leak identically.

CI therefore built and tested a binary that **had** the native, every suite
session built one that did not, and no test in either configuration could see
the difference. Note what this does to a test named "…in a default build":
under `cargo test --workspace` it is not running in a default build at all.

## Blast radius: this was never an H2 bug
`dev` `0c54a91849` (2026-07-31, landed while this fix was in flight) appended
independent confirmation from the Spring Framework suite to the original
report: a full 2847-class 4-shard run on a plain
`cargo build --release -p cratonvm-cli` hit this identical
`UnsatisfiedLinkError` on **61 of 90 FAIL classes — 68% of that run's entire
FAIL count**, across `spring-aop`, `spring-context`, `spring-tx`,
`spring-orm`, `spring-context-support`, `spring-messaging`, `spring-core`,
`spring-web` and `spring-webmvc`. Every one is a test whose method involves
Java serialization (`serializable()`, `canSerializeProxies()`, …). That
addendum is preserved verbatim below.

The three H2 classes in the title of this report were, as it guessed, "the
first three of many".

**Post-fix, interleaved A/B on the same host, same runner, same day.** The 63
classes were extracted from that run's `full-failcauses.log` (the addendum says
61; the log's exact count is 63) and re-run through
`apps/spring-suite-runner/runlist.sh`, 6 shards, `--jdk real --jit on`,
one VM per class:

| binary | tally | `latestUserDefinedLoader0` failcauses |
| --- | --- | --- |
| fix-free control (`7656ad3968`, built same day) | **63 FAIL** | 63 classes |
| this branch | **63 OK** | 0 |

Zero failcauses and zero crashes in the fixed run. The control is a *repeat*
of the baseline rather than the original 03:17 run's numbers, so the delta is
not confounded by the other commits that landed between them.

63 of the suite's 90 FAIL classes — 70% of its entire FAIL count — were this
one `#[cfg]`.

## Guards added
- `native-builtins/src/lib.rs`:
  `essential_registers_latest_user_defined_loader0_in_default_build` — runtime
  registry lookup against the registry `register_essential_natives` actually
  builds. Differentially verified: re-adding the `#[cfg(...)]` makes it fail.
- `native-builtins/src/lib.rs`:
  `default_build_does_not_register_synthetic_object_stream_natives` — asserts
  the synthetic OIS/OOS surface did **not** come along for the ride.
- `vm/tests/t14_system_conformance.rs::t14_vm_natives_not_feature_gated` —
  reads source text, so it is feature-independent, and fails if **any**
  `VM_NATIVES` entry's `register(` call sites are all `#[cfg(...)]`-gated.
  Differentially verified the same way.
  While writing it, `registers()` in that file turned out to match a bare
  `"class","method","desc"` tuple anywhere in `lib.rs` — including inside a
  unit test calling `registry.find(...)` — so `t14_all_vm_natives_registered`
  could be satisfied by a test asserting the registration rather than by the
  registration. It now requires the `register(` call prefix.
- `.github/workflows/ci.yml`: a `-p`-scoped
  "Default-feature native registry surface" step, so the feature set suite
  runners actually get is exercised in CI.

## Follow-up filed, deliberately not fixed here
The same audit found `serialization::register_reflection_factory_serialization`
one screen away in `register_essential_natives_with_shims`, gated on
`experimental-serialization` while its comment claims a real-JDK-mode role.
It is **not** the same bug: those 18 `ReflectionFactory` methods are ordinary
JDK bytecode, not natives, so ungating them replaces working JDK code rather
than fixing a link failure. No failing test asks for it. Written up as
`docs/known-issues/serialization/reflectionfactory-serialization-hooks-gated-out-of-suite-builds.md`
along with the recommendation to stop `libcratonvm`'s defaults from silently
deciding what a `--workspace` build contains.

The two other `#[cfg]`s in the real-JDK essential path — `wildfly_naming`
(synthetic JNDI, must not shadow real provider selection) and
`System.initPhase1/2/3` (synthetic-only by design) — were checked and are
correct as written.

---

# Original report (verbatim, 2026-07-30/31)

### `jdk/internal/misc/VM.latestUserDefinedLoader0()` throws `UnsatisfiedLinkError` in a default build — the fix exists in source but is gated behind an opt-in Cargo feature

### Status
**OPEN — build-configuration bug, not a missing implementation.** Found
while investigating H2 suite regressions in the twelfth-pass `TestUpgrade`
session's follow-up full-suite run (2026-07-30/31), then traced back to its
real cause via `git log -S`.

### Severity
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

### Affected test classes (H2 suite, this run)
- `org.h2.test.jdbc.TestPreparedStatement` (`testSetObject`, deserializing a
  `java.sql.Timestamp` written earlier in the same test)
- `org.h2.test.store.TestObjectDataType` (`testCommonValues`, deserializing
  a `java.sql.Timestamp`/`java.util.Date`)
- `org.h2.test.unit.TestSampleApps` (`org.h2.samples.Function`'s
  `get_x(make_point(10,20))`, deserializing a custom `Point` object stored
  via a Java function call)

All three fail identically and are almost certainly just the first three of
*many* possible triggers.

### Symptom
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

### Root cause — the fix already exists, it's just compiled out
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
##[cfg(any(feature = "experimental-serialization", feature = "synthetic-jdk"))]
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

### Suggested fix
Move the `jdk/internal/misc/VM.latestUserDefinedLoader0` registration out
from under the `experimental-serialization`/`synthetic-jdk` feature gate so
it's always registered (it has no dependency on anything else those
features guard — `latest_user_defined_loader_class` only uses
`NativeContext::frame_class_ids()`/`loader_id_of_class`, both
unconditionally available). Verify this doesn't accidentally pull in the
rest of `serialization.rs`'s synthetic read/write path (`ois_read_object`
et al.) under a default build — those may have their own, legitimate
reasons to stay opt-in; only this one native needs to move.

### Repro
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

### Confirmed independently via the Spring Framework suite (2026-07-31) — much larger blast radius than H2 alone

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

### Related
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
