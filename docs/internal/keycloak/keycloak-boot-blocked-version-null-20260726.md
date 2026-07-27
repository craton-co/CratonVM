# KC26-PIC.1 / KC26-RX.1 — Keycloak boot blocked by classloader stub fabrication

**Status: ✅ RESOLVED 2026-07-27.** The Keycloak 26.6.1 server boots under
CratonVM, and both JIT bans this doc existed to unblock (KC26-PIC.1 and
KC26-RX.1) were re-measured against it and **lifted**. Full closure, root
cause and the measurement table are in the final section, "CLOSED 2026-07-27".
The sections below are the original investigation, kept in chronological order
— including a recommendation that turned out to be wrong about where the fix
belonged (see the closure section).

## Correction to a prior finding

`docs/known-issues/full-ban-inventory-status-20260726.md` (this same
session, written before this investigation) listed KC26-PIC.1/KC26-RX.1 as
blocked because "needs a real Keycloak 26.2.4 checkout; not present on this
host." **That was incorrect** — a full real Keycloak fixture exists on this
host at `/home/victor/.m2/repository/org/keycloak/` (Maven repo with
Keycloak 26.6.1 and a `999.0.0-SNAPSHOT` master build, including a
bootable `keycloak-quarkus-dist-26.6.1.tar.gz` server distribution and
several `keycloak-tests-*` test jars). It was found this session only
after being told to search harder than the initial `find -iname
'*keycloak*'` sweep, which missed it because the previous searches only
checked shallow depth / obvious paths and the repo-scripts directory
(`apps/keycloak-suite-runner`, which is just the *runner*, not the
checkout it expects at `apps/keycloak`).

## What was tried

Extracted the real distribution
(`/data/tmp/kc-dist/keycloak-26.6.1/`), wrapped the CratonVM binary as
`bin/java` (matching this host's established WildFly-boot pattern —
`JAVA_HOME=<fake-dir-with-cratonvm-as-bin/java> CRATONVM_JAVA_HOME=<real
JDK25>`), and ran:

```bash
cd /data/tmp/kc-dist/keycloak-26.6.1
JAVA_HOME=/data/tmp/kc-javahome CRATONVM_JAVA_HOME=/home/victor/jdk25 \
  timeout 90 bash bin/kc.sh start-dev --http-enabled=true --hostname-strict=false
```

## Result: boot fails immediately, before reaching KC26-PIC.1/RX.1's code paths at all

```
Exception in thread "main" java/lang/ExceptionInInitializerError
	at io/quarkus/bootstrap/runner/QuarkusEntryPoint.main(QuarkusEntryPoint.java:37)
	at io/quarkus/bootstrap/runner/QuarkusEntryPoint.doRun(QuarkusEntryPoint.java:86)
	at org/keycloak/quarkus/runtime/KeycloakMain.main(KeycloakMain.java:68)
Caused by: java/lang/NullPointerException: Cannot invoke "String.toLowerCase()"
because "org.keycloak.common.Version.VERSION" is null
	at org/keycloak/common/Version.<clinit>(Version.java:43)
```

`Version.<clinit>` (decompiled from
`keycloak-common-26.6.1.jar!/org/keycloak/common/Version.class`) does:

```java
InputStream is = Version.class.getResourceAsStream("/keycloak-version.properties");
Properties p = new Properties();
p.load(is);
VERSION = p.getProperty("version");   // null if the resource wasn't found or lacks "version"
```

`VERSION` ends up `null`, and the next line calls `.toLowerCase()` on it,
NPEing. This means `getResourceAsStream("/keycloak-version.properties")`
either returned `null` (resource not found) or returned a stream that
didn't parse to a Properties object containing a `version` key.

**Not yet root-caused whether this is:**
1. A genuine CratonVM classloader resource-lookup gap specific to
   Quarkus's "fast-jar" runner layout (`QuarkusEntryPoint` uses a custom
   classloader over a `lib/main/` + `app/` + `quarkus/` directory
   structure, not a single flat jar — a layout this host's WildFly-boot
   precedent didn't need to handle), or
2. Something specific to how `keycloak-version.properties` is packaged
   (it may live in a different jar than `Version.class` itself, requiring
   correct classpath-wide resource search across the runner's multiple
   `lib/main/*.jar` entries).

This is **not a JIT bug** — it happens during class initialization before
any JIT-relevant code executes, and blocks reaching the actual KC26-PIC.1
(Picocli command reflection / SmallRye config timeout) and KC26-RX.1
(RxJava3 Infinispan stream hang) code paths entirely, so neither ban could
be re-tested this session.

## Recommendation

A future session should either (a) root-cause and fix the
`getResourceAsStream` gap for Quarkus's fast-jar runner layout (which would
also unblock testing potentially many other Quarkus-based JIT bans in this
codebase, not just these two), or (b) find/build a Keycloak test harness
that runs individual test classes directly via a JUnit launcher (like the
`hib-suite-runner`/`CratonRunner` pattern already established for
Hibernate on this host) rather than booting the full Quarkus server, which
would sidestep this specific boot blocker and let KC26-PIC.1/RX.1 be
tested against their actual named test classes (`PicocliTest`,
`RealmModelTest`) directly. The `keycloak-tests-*` jars in the m2 repo
(Keycloak 26+'s newer test-framework module) may already contain compiled
test classes suitable for this — not yet explored this session due to time.

## Update 2026-07-26 (later same day): root-caused and FIXED the Version.<clinit> NPE; boot now hits a second, deeper, separate blocker

**The original `Version.<clinit>` NullPointerException is FIXED.** Root
cause: `Class.getResourceAsStream`/`Class.getResource`'s native
implementations (`native_class_get_resource_as_stream` /
`native_class_get_resource` in `native-builtins/src/lang_class.rs`)
resolved every lookup via `ctx.find_resource`/`ctx.find_all_resource_urls`
-- CratonVM's own static, process-wide `-cp` scan -- with no regard for
which `ClassLoader` actually defined the class. `Version.class`'s real
defining loader is `io.quarkus.bootstrap.runner.RunnerClassLoader`, a
from-scratch `ClassLoader` subclass (not `URLClassLoader`) that overrides
only `findResource` and indexes Quarkus's `app`/`lib` split-jar layout
itself. That override was silently bypassed whenever the resource's
backing jar wasn't on the CratonVM process's own `-cp` -- which it never
is for Quarkus's fast-jar runner layout -- so `getResourceAsStream`
returned `null`, `Properties.load` did nothing, `VERSION` stayed `null`,
and `.toLowerCase()` NPE'd.

**Isolated with a standalone, Keycloak/Quarkus-independent reproducer**
(`CustomLoaderResourceProbe.java`, committed at
`docs/known-issues/repros/jitban-remaining-20260726/`): a minimal custom
`ClassLoader` overriding only `findClass`/`findResource` over a real jar.
Before the fix: `getResourceAsStream` returns `null` under CratonVM with
only the loader's own jar path passed to it (no `-cp` entry), while the
identical call succeeds under real HotSpot. Adding the same jar directly
to CratonVM's own `-cp` also makes it succeed -- confirming the
"loader-blind, `-cp`-only" theory precisely.

**The fix** (commit `ceea4eb05` on `fix/jit-ban-remaining-20260726`):
mirrors the delegation `cl_get_resource_as_stream`/`cl_get_resource`
already perform correctly in `native-builtins/src/classloader.rs` when
`ClassLoader.getResource*` is called directly on a loader instance.
`native_class_get_resource_as_stream`/`native_class_get_resource` now
resolve the class's defining loader via the existing
`native_class_get_class_loader`, and when that loader is a
`URLClassLoader` subclass or not one of the recognized builtin loader
classes (`java/lang/ClassLoader`, `java/net/URLClassLoader`,
`java/security/SecureClassLoader`, `jdk/internal/loader/*`,
`sun/misc/Launcher$*`), invoke the loader's own
`getResourceAsStream`/`getResource` virtually instead of the static
`-cp` scan. The common/builtin-loader case -- the overwhelming majority
of calls -- falls through to the existing fast path completely unchanged,
so there's no performance impact there. `cargo test --release -p
cratonvm-vm --lib skip_list`: 63 passed, 0 failed (this file isn't part
of that suite, included as a standard regression gate).

**Confirmed against the real Keycloak 26.6.1 distribution**: rebuilt
(`target/release/cratonvm-t24-classloader-delegation-20260726`),
re-wrapped as `/data/tmp/kc-javahome/bin/java`, re-ran:

```bash
cd /data/tmp/kc-dist/keycloak-26.6.1
JAVA_HOME=/data/tmp/kc-javahome CRATONVM_JAVA_HOME=/home/victor/jdk25 \
  timeout 90 bash bin/kc.sh start-dev --http-enabled=true --hostname-strict=false
```

`Version.<clinit>` no longer throws -- boot gets measurably further.

## New, separate, deeper blocker found after the fix: class RESOLUTION (not resource lookup) also appears loader-blind

Boot now fails later with:

```
[cratonvm] main-vm run() Err (debug): Error in thread "main" class file error: class not found: org/keycloak/quarkus/runtime/configuration/mappers/PropertyMappers
```

Note this is CratonVM's own internal `class file error: class not
found` -- not a Java-level `ClassNotFoundException` bubbling up through
user bytecode (the stack trace format differs; compare the original
`Version` bug which showed a normal `java/lang/ExceptionInInitializerError`
Java stack trace). This looks like a resolver-level failure during
constant-pool / symbolic-reference class resolution, a different code
path from the explicit `Class.forName`/`ClassLoader.loadClass` delegation
machinery (which already has substantial existing `findClass`-override
support in `classloader.rs` -- `cl_load_class`, `cl_load_class_resolve`,
`cl_load_class_base_delegation`, etc. -- so this is NOT the same "missing
delegation infrastructure" gap the resource bug was).

**The class genuinely exists** -- confirmed via `python3 zipfile` (NOT
`unzip`, which is missing from this host's non-interactive SSH `PATH`;
use `python3 -c "import zipfile; ..."` or `/usr/bin/jar tf <jar>`
instead) at:

```
/data/tmp/kc-dist/keycloak-26.6.1/lib/lib/main/org.keycloak.keycloak-quarkus-server-26.6.1.jar
  -> org/keycloak/quarkus/runtime/configuration/mappers/PropertyMappers.class
  -> (plus ~30 sibling *PropertyMappers*.class files in the same package)
```

This jar sits under `lib/lib/main/`, one of Quarkus's fast-jar runner's
own indexed directories (distinct from `lib/quarkus/` and `lib/app/`,
which is what `RunnerClassLoader` normally serves classes from post
Quarkus's dev-mode "augmentation" step) -- so the class is real and
present on disk, just apparently unreachable through whatever resolution
path threw this specific error.

**Hypothesis, not yet confirmed:** the same underlying pattern as the
just-fixed resource bug -- a global/`-cp`-only view instead of genuine
per-`ClassLoader` delegation -- but in the class-resolution path rather
than the resource-lookup path. If true, this is a second, likely more
consequential instance of the same root issue (class loading is far more
central than resource loading), and worth checking whether other
already-"fixed"/"working" JIT bans that depend on custom classloaders
were only ever exercised through paths that happen to route through the
`-cp`-aware slow path.

**Root-caused to the exact code site (not yet fixed).** The hypothesis
above is confirmed: `ClassManager::find_class_bytes_delegated` in
`classloading/src/class_manager.rs` (the function `load_class` falls
through to for parent delegation, right above `define_class`) resolves a
name through exactly three FIXED loader levels only --
`self.bootstrap.find_class_bytes`, `self.extension.find_class_bytes`,
`self.application.find_class_bytes` -- in that order, with no path
whatsoever to consult an actual runtime-registered custom `ClassLoader`
instance's own `findClass` override. If none of those three fixed levels
has the bytes, it unconditionally returns
`ClassFileError::ClassNotFound`, then `load_class`'s caller falls back to
a synthetic stub only for recognized JDK/enterprise-stub name prefixes
(see the large `is_jdk_class(name)` branch just above) -- a name like
`org/keycloak/quarkus/runtime/configuration/mappers/PropertyMappers`
matches none of those prefixes, so it surfaces as a hard
`ClassFileError::ClassNotFound` instead.

This is architecturally the same defect class as the (now-fixed)
resource bug, but one level more fundamental: `self.application` is
populated from the process's own `-cp`, so any class whose *only*
defining path is a genuinely custom `ClassLoader.findClass` override
(not also reachable via bootstrap/extension/the flat application
classpath) cannot be constant-pool-resolved at all, regardless of
whether the class is later reachable via an explicit
`Class.forName`/`ClassLoader.loadClass` call (which DOES have real
per-loader `findClass`-override delegation already, per
`cl_load_class`/`cl_load_class_resolve`/`cl_load_class_base_delegation`
in `classloader.rs` -- those two code paths have simply diverged).

**Recommendation for whoever picks this up:** `find_class_bytes_delegated`
needs a fourth path -- when linking a constant-pool reference from a
class whose own defining loader is a real custom `ClassLoader` instance
(not bootstrap/extension/application), the resolution should also try
that loader's `findClass` (mirroring the delegation already implemented
for explicit `Class.forName`/`loadClass` in `classloader.rs`) before
giving up. This is a materially bigger and riskier change than the
resource fix (constant-pool resolution is extremely hot-path and this
function has no `ObjectRef`/loader-instance context readily available at
its current call sites -- it takes only `name: &str` -- so plumbing the
right loader identity through likely touches the callers of `load_class`
too). Recommend a dedicated session/investigation rather than folding it
into a JIT-ban sweep. `KC26-PIC.1`/`KC26-RX.1` remain banned/blocked, now
specifically by this new class-resolution gap rather than the (now
fixed) resource bug.

## CLOSED 2026-07-27 — the whole loader-blindness family is fixed; Keycloak 26.6.1 boots

**Status: RESOLVED.** The real `keycloak-quarkus-dist-26.6.1` server now boots
under CratonVM all the way to `io.quarkus.runtime.Quarkus.waitForExit()` —
through Picocli CLI parsing, SmallRye config mapping, the Quarkus augmentation
step, BouncyCastle/JCA provider registration, Hibernate ORM + Liquibase
bootstrap, Arc (CDI) container init and RESTEasy Reactive deployment. Verified
by the watchdog stack dump (`CRATONVM_DEFAULT_WATCHDOG_SEC=420`), whose main
thread sits in `ApplicationLifecycleManager.waitForExit → awaitUninterruptibly`
— the normal "started, waiting for shutdown" state.

### The recommendation in the section above was wrong about *where* the fix goes

It proposed teaching `ClassManager::find_class_bytes_delegated` a fourth,
custom-`ClassLoader` path and called that "a materially bigger and riskier
change ... 149 call sites". None of that was needed. `find_class_bytes_delegated`
is untouched. The real defect was **ordering**, in four much smaller places:

CratonVM fabricates a *synthetic stub* (a class with no `Code` on any method)
for any name under a recognized enterprise prefix — `io/quarkus/`, `org/jboss/`,
`io/smallrye/`, `org/infinispan/`, ... — that none of the three built-in loaders
can find (`is_enterprise_stub_prefix` / `create_synthetic_stub`). That stub is
registered **globally, under `Application`**. So the stub does not merely give
one bad answer: it *poisons the binary name*, and the custom loader that owns
the real class can never define its own copy afterwards. Every symptom in this
doc's history is that one mechanism firing at a different call site.

### The four fixes (all in this session's commit)

1. **`ClassManager::would_fabricate_synthetic_stub(name)`** (new, non-destructive)
   — mirrors `load_class`'s own fallback branches and answers "this name would
   only produce a stub" *without* creating one. Every fix below is gated on it,
   which is what keeps them strictly additive: they can only change an answer
   that was going to be fabricated.

2. **`NativeContextImpl::{load_class, ensure_class_initialized,
   new_object_initialized}`** (`vm/src/vm/vm_exec.rs`) — the name-based entry
   points natives use now consult the *calling class's* own `ClassLoader` (JNI
   `FindClass` semantics) when the global answer would be a stub, and again
   after a genuine miss. This fixed two boot blockers:
   - `Class.getGenericInterfaces()` on a SmallRye `@ConfigMapping` interface
     resolved the type argument `io.quarkus.runtime.configuration.MemorySize`
     (only in `lib/main/io.quarkus.quarkus-core-3.33.1.jar`) to a stub →
     `VerifyError: non-abstract non-native method must have Code attribute` out
     of `io.quarkus.runtime.generated.SharedConfig.<clinit>`.
   - the JCA provider chain instantiates SPI classes by name
     (`build_jca_impl` → `new_object_initialized`), so BouncyCastle's
     `PKCS12KeyStoreSpi$BCPKCS12KeyStore` — registered by a provider loaded from
     a `RunnerClassLoader` jar — surfaced as `ClassNotFoundException` in
     `KeyStore.getInstance` during `JavaKeystoreKeyProviderFactory.init`.

3. **`resolve_class_loader_aware`** (`vm/src/runtime/interpreter.rs`) — the same
   rule for constant-pool resolution: a `new`/`checkcast`/`ldc` naming a
   would-be-stub is offered to the referencing class's defining loader first.

4. **`cl_real_load_class_base`** (`native-builtins/src/classloader_real.rs`) —
   **`ClassLoader.loadClass` must never answer with a fabricated stub.** This
   was the last and most interesting one. Quarkus's `RunnerClassLoader` lists
   `io.quarkus.value.registry` among 40 **parent-first** packages: it calls
   `getParent().loadClass(name)` inside `try { } catch (ClassNotFoundException)`
   and, on the expected refusal, falls through to its own index — which has the
   class, in `lib/quarkus/generated-bytecode.jar`. CratonVM's app loader instead
   returned a stub, so Arc's generated `ValueRegistry_…_Synthetic_Bean` became a
   method-less, interface-less class and `ArcContainerImpl.<init>` died with
   `ClassCastException: … cannot be cast to io.quarkus.arc.InjectableBean`.
   HotSpot's `loadClass` has no notion of stubs; ours now doesn't either.
   `CRATONVM_CL_STUB_DELEGATION=1` restores the legacy behaviour.
   The stub is still minted by constant-pool / `Class.forName` fallbacks when
   nothing else can supply the name — only the explicit `loadClass` API stopped
   manufacturing one mid-delegation.

### Isolated reproducer

`docs/known-issues/repros/keycloak-runner-loader-20260727/RunnerLoaderProbe.java`
builds the real `RunnerClassLoader` from `quarkus-application.dat` and asks it
for five classes. Before the fix, two of them came back defined by
`jdk.internal.loader.ClassLoaders$AppClassLoader` (the stub) instead of
`RunnerClassLoader`; after it, all five match HotSpot exactly. It runs in ~2
seconds, versus ~7 minutes for a full boot — the reason the last two blockers
were found quickly. Sibling probes (`DirMapProbe`, `GenSetProbe`,
`ResourceDataProbe`, `JdkPkgProbe`) narrowed it from "the loader can't find the
class" to "the loader never asks its own index, because the parent answered" by
ruling out the `HashMap`/`HashSet`/jar-read layers one at a time.

### New diagnostics added (all env-gated, all cached — no hot-path cost)

- `CRATONVM_DBG_STUB_BT=<substring>` — Rust backtrace where a synthetic stub is
  fabricated for a matching name. A stub is silent until something *runs* it, by
  which point the resolver that asked for it is long gone from the stack.
- `CRATONVM_DBG_RTERR=<substring>` — VM-raised runtime errors (`Debug` form
  matched against the substring) with the Java stack at the raise point. The
  VM-side raise path never goes through `athrow`, so `CRATONVM_DBG_ATHROW`
  cannot see these.
- `CRATONVM_DBG_LINKAGE_BT=1` — Rust backtrace at every linkage-error raise.
- `CRATONVM_DBG_STUBLOADER=1` — traces the "would-stub → ask the caller's own
  loader" fallback.

### Verification

- `RunnerLoaderProbe`: CratonVM output now byte-identical to HotSpot.
- Full `kc.sh start-dev` boot reaches `waitForExit` (see above).
- `cargo test --release -p cratonvm-vm --lib skip_list`: 68 passed, 0 failed.
- `regression-suite/run.sh`: 13 passed, 0 failed.

### Known residual (NOT a boot blocker, out of this doc's scope)

Arc logs `No matching bean found for type class …` for the RESTEasy Reactive
handler/exception-mapper beans, and the process ends up with only 3 live threads
(no Vert.x event loop), so the HTTP listener never opens and the
"Keycloak … started in Ns / Listening on http://…" banner is not printed. The
lifecycle itself completes normally. That is a separate CDI/Arc-registration
gap, tracked on its own; it does not block the JIT-ban testing this doc existed
to unblock.
