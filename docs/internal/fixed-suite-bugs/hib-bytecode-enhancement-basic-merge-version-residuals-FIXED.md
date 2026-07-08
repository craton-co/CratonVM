# Hibernate bytecode enhancement basic/merge/version residuals - fixed 2026-07-08

| | |
|---|---|
| **Status** | FIXED / RETIRED on dev, 2026-07-08. |
| **Area** | Hibernate ORM bytecode enhancement residuals in basic dirty tracking, final-field embedded IDs, composite merge/null, and versioned entity type checks. |
| **Supersedes** | The residual tracking split from `hib-bytecode-enhancement-loader-faithful-linking.md` after the lazy/lazytoone loader-linking family was retired. |

## Closure evidence

Validated on Azure host `20.83.144.174`, worktree `/data/data/cratonvm-worktrees/20260708-183000-hib-enhancement-basic-residuals`, binary `/data/data/bin/cratonvm-hib-enhancement-basic-residuals-20260708-183000-fix6`, log `/tmp/hib-enhancement-basic-residuals-20260708-183000-fix6-six.log`:

| Index | Class | Result |
|---:|---|---|
| 1 | `org.hibernate.orm.test.bytecode.enhance.internal.bytebuddy.DirtyCheckingWithEmbeddableAndNonVisibleGenericMappedSuperclassTest` | `found=8 started=8 ok=8 failed=0` |
| 18 | `org.hibernate.orm.test.bytecode.enhancement.basic.ExtendedEnhancementNonStandardAccessTest` | `found=28 started=28 ok=28 failed=0` |
| 20 | `org.hibernate.orm.test.bytecode.enhancement.basic.FinalFieldEnhancementTest` | `found=5 started=5 ok=5 failed=0` |
| 113 | `org.hibernate.orm.test.bytecode.enhancement.merge.CompositeMergeTest` | `found=1 started=1 ok=1 failed=0` |
| 114 | `org.hibernate.orm.test.bytecode.enhancement.merge.CompositeNullTest` | `found=1 started=1 ok=1 failed=0` |
| 117 | `org.hibernate.orm.test.bytecode.enhancement.version.VersionedEntityTest` | `found=1 started=1 ok=1 failed=0` |

Focused probes also pass with the same binary:

- JUnit `Namespace.create(extensionName, testInstance)` keeps the same hash before and after Hibernate mutates `entityId`, and both `map.get(n1)` / `map.get(n2)` return `scope`.
- `List.of(extensionName, testInstance).hashCode()` is stable before and after the `entityId` mutation through direct, `Object`, and `List` call sites.
- `System.identityHashCode()` remains stable across primitive and reference field writes.

## Root cause

The residual set was not one bug:

1. Loader-private enhanced classes still leaked through several non-lazy paths. `Class.isInstance` / `Class.isAssignableFrom` now perform loader-aware same-name type checks for generated enhancement interfaces and local class copies, and `invokestatic` / promoted invoke caches now resolve the owner in the caller's loader namespace before dispatching. That closes the same-name `CompositeTracker`, embedded-id, and versioned-entity type failures.
2. `native-collections` misclassified any one-field object containing an `int`, `long`, `float`, or `double` slot as a boxed primitive. Hibernate's composite test instance has a single `long entityId`; after `@BeforeEach` stored the generated ID, JUnit's `Namespace` key hash changed because `List.of(extensionName, testInstance).hashCode()` used `entityId` instead of the test instance identity hash. The store lookup then missed and the visible failure was the cleanup `ServiceRegistryScopeImpl.releaseRegistry()` null-scope NPE. `unbox_wrapper` now requires the receiver class to be one of the eight JDK primitive wrapper classes before unboxing.

## Reproduction command

```bash
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
for idx in 1 18 20 113 114 117; do
  env CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
    /data/data/bin/cratonvm-hib-enhancement-basic-residuals-20260708-183000-fix6 \
    --java-home /data/data/.gradle/jdks/eclipse_adoptium-22-amd64-linux.2 \
    --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner \
    /data/data/wt-hib-enh-classvalue-20260706/apps/hib-suite-runner/gated_subset.txt "$idx"
done
```

## Original evidence

After `codex/hib-enhancement-loader-retire-20260708-121047`, the lazy/lazytoone loader-linking family was closed:

- `lazy_lazytoone_sample.txt`: `SUMMARY total=18 pass=18 fail=0 hang=0`
- `lazy_lazytoone_subset.txt`: `SUMMARY total=69 pass=69 fail=0 hang=0`
- `LoadAndFetchGraphAssociationNotExplicitlySpecifiedTest`: `found=14 started=14 ok=14 failed=0` after the final same-name type-test fallback

The broader `gated_subset.txt` still had the distinct residuals retired above when run with `/data/data/bin/cratonvm-hib-enhancement-loader-retire-20260708-121047-fix8`.
