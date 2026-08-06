# Annotation-metadata-null cluster: pre-fix/post-fix A/B, 2026-08-06

Closes `docs/known-issues/springboot/spring-boot-annotation-metadata-null-cluster-20260805.md`
and its `Related` page `spring-bean-attribute-type-null-flake-20260803.md`.
Verdict and mechanism:
`docs/internal/fixed-suite-bugs/springboot/spring-boot-annotation-metadata-null-cluster-RESOLVED-20260806.md`.

- **Worktrees:** `C:\craton\CratonVM-annmeta-20260806` (branch
  `fix/springboot-annotation-metadata-null-20260806`, off `dev` @ `ce462d315`)
  and `C:\craton\CratonVM-annmeta-anchor-20260806` (detached at `1078f6f05c`,
  the point the 08-05 Azure full suite was cut from).
- **Binaries:** `cratonvm-annmeta0806-dev.exe` and
  `cratonvm-annmeta0806-anchor.exe`, both `cargo build --release`.
- **Fixture:** the already-built checkout in
  `C:\craton\CratonVM-spring-boot-residual-20260728\apps\spring-boot`
  (`run-single-class.ps1 -SpringBootRoot ...`), so both arms read identical
  class files.
- **Host:** Windows, 32 logical cores, one process per run.

## The serial run is not the experiment

Six serial runs of `DataNeo4jReactiveRepositoriesAutoConfigurationTests` on the
**anchor** all passed. So did the single serial run of each class that opened
this session. Serial runs pass on both arms and prove nothing here — the
defect needs enough concurrent compile/tier-up/drop churn to recycle a
`JitInvokeInfo` address while a memo still holds the old site's answer. Every
number below is 4 lanes of the same class running concurrently.

## Result

Blocks run in both orders on one host, same fixture, nothing else changed.

| block | binary | runs | runs with >=1 failed test |
|---|---|---:|---:|
| A | `1078f6f05c` anchor (pre-`383e7f5cf`) | 36 | **3** |
| B | branch `dev` | 36 | 0 |
| C | branch `dev` | 24 | 0 |
| D | `1078f6f05c` anchor (pre-`383e7f5cf`) | 36 | **4** |

Per class:

| class | anchor | dev |
|---|---:|---:|
| `DataNeo4jReactiveRepositoriesAutoConfigurationTests` | 4 / 24 | 0 / 20 |
| `DataCassandraReactiveRepositoriesAutoConfigurationTests` | 2 / 24 | 0 / 20 |
| `DataCouchbaseReactiveRepositoriesAutoConfigurationTests` | 1 / 24 | 0 / 20 |
| **total** | **7 / 72** | **0 / 60** |

At the anchor's ~10% per-run rate, 60 consecutive clean post-fix runs is
p < 0.002.

## Faces seen on the anchor

Seven failures, four distinct faces, all in the same annotation-metadata
machinery — including the page's headline symptom byte-for-byte, though it
landed in the **neo4j** class where the page recorded it under **cassandra**:

1. `Cannot invoke "MergedAnnotations.get(java.lang.Class)" because the return
   value of "MergedAnnotations.from(AnnotatedElement, SearchStrategy,
   RepeatableContainers)" is null`
2. `IllegalArgumentException: RepeatableContainers must not be null`
3. `Cannot invoke "MergedAnnotation.isPresent()" because "annotation" is null`
4. ByteBuddy `AnnotationValue.filter(...)` on null / `Unknown type: null`

Which face appears is an accident of what the recycled memo entry held. None
of the four occurred in 60 post-fix runs.

## The one-run diagnostic

`CRATONVM_DBG_SITE_ALIAS=1` prints each dispatch site key that came to name a
different call site than the one that first used it. All three classes hit the
40-event print cap in a single run, out of ~900-1000 distinct keys, and the
pairs it names are the call sites the pages blame — e.g.
`Class.isPrimitive()Z` aliased with `AnnotationsScanner.hasPlainJavaAnnotationsOnly(Object)Z`
and with `AnnotationTypeMapping.getDistance()I`, and
`TypeMappedAnnotations.from(...)` aliased with `Method.getDeclaringClass()`.
This measures the defect's precondition rather than its rare corruption, which
is why it answers in one run what a hit-rate hunt could not answer in ~1030.

## Reproduce

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\run-single-class.ps1 `
  -Module module/spring-boot-data-neo4j `
  -ClassName org.springframework.boot.data.neo4j.autoconfigure.DataNeo4jReactiveRepositoriesAutoConfigurationTests `
  -SpringBootRoot C:\craton\CratonVM-spring-boot-residual-20260728\apps\spring-boot `
  -Exe <binary>
```

Run four of those concurrently per class per round; a single lane will not
reproduce it on either arm.
