# `ClassLoader.defineClass` served the existing mirror where HotSpot raises `LinkageError` — FIXED 2026-08-11

**Status:** FIXED. Found while executing the native-precedence re-audit's
`defineClass` snippet (`RJdkDefineClass`), filed separately because the remedy
is a decision about duplicate-definition semantics rather than about decode
fidelity, and it can reach every workload that stacks class loaders.

## The defect

JVMS §5.3.5: a class loader that has already defined a class of a given name
must not define another. Measured on OpenJDK 25.0.4:

```
java.lang.LinkageError: loader DupProbe$L @1dbd16a6 attempted duplicate class
definition for Dp1. (Dp1 is in unnamed module of loader DupProbe$L @1dbd16a6,
parent loader 'bootstrap')
```

CratonVM raised nothing. A `ClassLoader` subclass calling
`defineClass(name, bytes, 0, len)` twice got the FIRST class back, silently, in
both `--real-jdk` and `--jdk-only`, and across the `byte[]`/`ByteBuffer`
boundary (`defineClass1` then `defineClass2`).

Every "already defined" error from the class-manager backend was routed through
`lang_system.rs::same_loader_already_defined_mirror`, which looked for an
existing class of that name and returned it. Every one of that helper's arms is
keyed on THIS loader — so it served the mirror in precisely the case HotSpot
refuses, and there was no arm for the case it was named after.

## Why the tolerance could not simply be deleted

It was added 2026-07-10 (`38cfa98b5`) for a measured Tomcat failure, and the
incident report is the thing that identifies the right discriminator:

> Every `TestEncodingDetector` sub-test starts and stops its own embedded
> `Tomcat` instance; after **~14 stop/start cycles** in one process,
> `defineClass1(org/apache/catalina/loader/JdbcLeakPrevention)` started throwing
> `IncompatibleClassChangeError` ("already defined by user-defined(N) loader"),
> cascading into `LifecycleException: A child container failed during stop` for
> every subsequent parameter.

Each cycle has its own `WebappClassLoader`. On HotSpot each of those loaders
defines its own `JdbcLeakPrevention` and none of them conflicts — the oracle
above says so directly ("other loader, same name → NO THROW", and the two
classes are distinct). What collided was **CratonVM's namespace numbering**, not
the loaders: `define_class_shared_with_options` probes `(loader_id, name)` where
`loader_id` is a synthetic NAMESPACE number two distinct loader objects can
share. So the tolerance was covering a VM artefact, and deleting it would have
re-broken the container lifecycle.

## The fix

Discriminate on **defining-loader OBJECT identity**, which is the thing JVMS
§5.3.5 actually turns on:

| shape | HotSpot | now |
|---|---|---|
| the same loader object defines a name twice | `LinkageError` | `LinkageError` |
| two distinct loaders, one CratonVM namespace | both succeed | serve the existing mirror (unchanged) |

`same_loader_already_defined_mirror` becomes `classify_duplicate_define`
returning `NotDuplicate` / `SameLoaderObject` / `ServeExisting`, and all three
`defineClass0/1/2` call sites act on it. `hidden` classes keep their existing
exclusion: a hidden class has no binary name in any namespace, so the rule does
not reach it.

Two supporting pieces:

* `classloader::class_defined_by_this_loader_object` — the strongest identity
  statement available, factored out of the identical inline scan
  `find_loaded_class_for_loader_inner` already ran, so the two readers of "did
  THIS object define it" cannot drift.
* `LinkageError::DuplicateClassDefinition`, mapping to **`java.lang.LinkageError`
  itself**, not a subclass. Code that catches this catches the base type.

**The bias is stated and deliberate.** `SameLoaderObject` is returned only on a
POSITIVE identification. The record is an `ObjectRef` and a moving collection
can leave a stale pointer, so "not recorded" and "recorded elsewhere" both fall
to `ServeExisting`. A missed `LinkageError` is what this VM did yesterday; a
spurious one is a new way to break a workload that was working.

## What the measurement says about the tolerance itself

`CRATONVM_DBG_DUPDEF=1` (declared, so `CRATONVM_DBG=dupdef` reaches it) names
every "already defined" error and the verdict it got. Both arms print, so a
probe can show which one it exercised instead of assuming.

Across everything run for this change — the regression corpus in both modes, 45
Spring aop/aspectj/aot/instrument/beans/cglib classes, 9 loader-stacking classes,
a 40-loader churn probe, and a probe built specifically to collide namespaces
via shared parents — the `ServeExisting` arm **never fired once**. The only
verdicts observed were 41 `SameLoaderObject` (the deliberate duplicate checks in
the probes) and one `NotDuplicate`.

That is consistent with the namespace collision having been fixed at its root
since: `loader_namespace_id_store` is now keyed by the loader OBJECT with
post-GC reconciliation, and its own comment records that the previous
identity-hash keying "never pruned dead loaders — a fresh per-compile loader
could inherit a dead sibling's namespace id". The tolerance appears to be legacy
cover for a defect that no longer occurs.

**It is kept anyway.** "Not reachable in any configuration this lane could
construct" is not "unreachable", the Tomcat fixture is not on this host so the
originating workload could not be re-run, and removing it is a separate decision
that this change does not need. The diagnostic is the instrument that will show
it if it ever fires again.

## Validation

One binary per arm, same host (Azure linux, JDK 25.0.4+7), same workloads.

| gate | base | fixed |
|---|---|---|
| `DupProbe` (6 duplicate/distinct shapes) vs HotSpot | 3 wrong | **all 6 match**, both modes |
| `SUITE=all` regression corpus | 64/0 | **65/0** (+`RLoaderChurnDefine`) |
| `SUITE=jdk-only`, `--jdk-only` | 23/4 | **23/4**, identical failing set |
| 9 Spring loader-stacking classes (194 tests) | all OK | **identical** |
| 45 Spring aop/aspectj/aot/instrument/cglib classes | 44 OK / 1 FAIL | **byte-identical verdicts** |
| `cratonvm-classloading --lib` | — | 791/0 |
| `cratonvm-native-builtins --lib classloader` | — | 135/0 |

The single Spring FAIL (`AspectJAutoProxyCreatorTests`, 19/22) is identical on
both binaries and pre-existing.

**Not run: the Tomcat suite.** `TestEncodingDetector` / `TestVirtualContext` are
the workloads the tolerance was added for, and the fixture
(`/data/data/apps/tomcat`) is not on this host. `RLoaderChurnDefine` reproduces
the incident's SHAPE — 40 short-lived loaders defining the same name, plus a
survivor that outlives the churn — and holds it down permanently, but it is a
model of the workload and not the workload. Anyone with the Tomcat fixture
should run those two classes against this change.

## Vectors

* `regression-suite/src/RJdkDefineClass.java` — the refusal. The check was
  landed commented `NOT ASSERTED` when the divergence was measured; it is now
  enabled, asserts the exception TYPE is `LinkageError` itself, asserts the
  stable part of the message (the tail carries an identity hash), and asserts
  the refusal crosses the `defineClass1`/`defineClass2` boundary.
* `regression-suite/src/RLoaderChurnDefine.java` — the PERMISSION, which is the
  half a naive fix breaks: 40 loaders × one name all distinct, a multi-name
  churn that rotates definition order, and a survivor that keeps its class and
  still refuses its own duplicate afterwards. 1,183 checks, green on HotSpot and
  on CratonVM in both modes.
