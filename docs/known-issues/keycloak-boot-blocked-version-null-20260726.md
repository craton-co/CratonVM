# KC26-PIC.1 / KC26-RX.1 — blocked by a classloading bug, NOT a missing fixture

**Status: still banned, blocked by a separate boot-time bug found this session, not by "no fixture" (a prior doc's claim that Keycloak has no fixture on this host was wrong — see below).**

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
