# Hibernate bytecode enhancement residuals after loader-faithful lazy/lazytoone closure

| | |
|---|---|
| **Status** | OPEN, split from the retired loader-faithful enhancement/lazy/lazytoone note on 2026-07-08. |
| **Area** | Hibernate ORM bytecode enhancement, non-lazy residuals in basic dirty tracking, composite merge/null, and versioned entity identity checks. |
| **Supersedes** | Residual tracking formerly bundled into `hib-bytecode-enhancement-loader-faithful-linking.md`; that loader-linking/lazy/lazytoone family is now fixed and retired. |

## Current evidence

After `codex/hib-enhancement-loader-retire-20260708-121047`, the lazy/lazytoone loader-linking family is closed:

- `lazy_lazytoone_sample.txt`: `SUMMARY total=18 pass=18 fail=0 hang=0`
- `lazy_lazytoone_subset.txt`: `SUMMARY total=69 pass=69 fail=0 hang=0`
- `LoadAndFetchGraphAssociationNotExplicitlySpecifiedTest`: `found=14 started=14 ok=14 failed=0` after the final same-name type-test fallback

The broader `gated_subset.txt` still has distinct residuals, confirmed with `/data/data/bin/cratonvm-hib-enhancement-loader-retire-20260708-121047-fix8`:

| Index | Class | Current result | Signature |
|---:|---|---|---|
| 1 | `org.hibernate.orm.test.bytecode.enhance.internal.bytebuddy.DirtyCheckingWithEmbeddableAndNonVisibleGenericMappedSuperclassTest` | `found=8 started=8 ok=5 failed=3` | `ClassCastException: Object of type '...$MyEmbeddable' can't be cast to CompositeTracker` |
| 18 | `org.hibernate.orm.test.bytecode.enhancement.basic.ExtendedEnhancementNonStandardAccessTest` | `found=28 started=28 ok=21 failed=7` | `AssertionError: [Loaded value after update]` |
| 20 | `org.hibernate.orm.test.bytecode.enhancement.basic.FinalFieldEnhancementTest` | `found=5 started=5 ok=4 failed=1` | `IllegalArgumentException: Supplied id had wrong type` for same-named `EmbeddableId` |
| 113 | `org.hibernate.orm.test.bytecode.enhancement.merge.CompositeMergeTest` | `found=1 started=1 ok=0 failed=1` | `NullPointerException: ... ServiceRegistryScopeImpl.releaseRegistry() because "scope" is null` |
| 114 | `org.hibernate.orm.test.bytecode.enhancement.merge.CompositeNullTest` | `found=1 started=1 ok=0 failed=1` | same `ServiceRegistryScopeImpl.releaseRegistry()` null-scope NPE |
| 117 | `org.hibernate.orm.test.bytecode.enhancement.version.VersionedEntityTest` | `found=1 started=1 ok=0 failed=1` | `IllegalArgumentException: Passed entity instance ... is not of expected type` |

`InheritedTest` and `MappedSuperclassTest` still report `ok=3 failed=0 aborted=1`; this matches the historical assumption-abort behavior noted in the retired doc and is not counted here as a CratonVM failure.

## Initial read

These residuals are downstream of the loader-faithful class-resolution work, not the same no-arg helper/default-method/lazy-field/enum-array surfaces fixed on 2026-07-08. The remaining signatures look like separate enhancement integration gaps:

- `CompositeTracker` false negative: likely enhanced embeddable tracking interface visibility or object instantiation path, not a same-name `checkcast` (the final type-test fallback fixes the graph same-name CCE but does not change this).
- Basic dirty tracking assertion: loaded state after update is wrong even after loader identity and field-slot fixes.
- Final-field embedded id and versioned entity checks: Hibernate still sees same-named ids/entities as the wrong runtime type in non-lazy paths.
- Composite merge/null scope NPE: may be a test-extension cleanup symptom after an earlier setup failure; root cause not yet isolated.

## Reproduction

Use the Azure harness:

```bash
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
env CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  /data/data/bin/cratonvm-hib-enhancement-loader-retire-20260708-121047-fix8 \
  --java-home /data/data/.gradle/jdks/eclipse_adoptium-22-amd64-linux.2 \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner \
  /data/data/wt-hib-enh-classvalue-20260706/apps/hib-suite-runner/gated_subset.txt <index>
```
