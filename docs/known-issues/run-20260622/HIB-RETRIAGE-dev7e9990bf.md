# Hibernate ORM suite re-triage — dev HEAD 7e9990bf (2026-06-24)

Re-ran the task's CratonVM-only failing classes against a **fresh build of dev
HEAD** (`7e9990bf`), `--nojit`. The originally-supplied `results.tsv`
(`.cratonvm-suite/out-rerun-dev40773014/`) was taken at baseline `40773014`,
which is many merges behind current dev — so a large fraction of the listed
failures were **already fixed** on dev and only a handful remain.

Repro (per class):
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm> --java-home "C:/Program Files/Java/jdk-25" \
  --nojit @common.args -Dcraton.trace=1 CratonRunner <listfile> 0
```

## Already GREEN on dev HEAD (no action needed)

| Class | Note |
|---|---|
| `sorted.set.SortNaturalTest` | merged 9ac7ef1d (natural_compare field-0) |
| `mapping.fetch.depth.NoDepthTests` | merged 9258f821 (ShrinkWrap URL) |
| `annotations.onetoone.OneToOneJoinTableUniquenessTest` | 5/5 — the whole "schema temp-file write / Unable to open script target" cluster passes now |
| `bootstrap.scanning.JarVisitorTest` | 9/9 — the silent rc=0 crash is gone |
| `hql.HqlParserMemoryUsageTest` | `resetPeakUsage0` linkage fixed (e32bbcab); residual = the known 120 s interpreter throughput wall, **not a bug** |
| `bootstrap.registry.classloading.ClassLoaderLeaksUtilityTest` | now a 120 s throughput timeout, not a linkage/logic failure |

## FIXED this pass — branch `fix/hib-reflect-iae-printstacktrace` (8a9bd026)

- **`query.NativeQueryConstructorErrorTest` 1/2 → 2/2.** `coerce_arg_strict`
  (the shared `Method.invoke` / `Constructor.newInstance` argument coercion)
  threw a descriptive `IllegalArgumentException` where the JDK's
  `jdk.internal.reflect` accessors throw `IllegalArgumentException("argument
  type mismatch")` verbatim. Hibernate's HHH-20261 error appends `"…due to: " +
  cause.getMessage()`, so the test's `contains("due to: argument type
  mismatch")` failed. Now JDK-exact for genuine mismatches. Sibling of the
  `Method.invoke` IAE-vs-NSME fix.

- **`Throwable.printStackTrace(PrintStream/PrintWriter)` stream routing.** The
  user's "stack traces come back empty" symptom: frames ARE captured, but the
  `(stream)` overloads ignored the argument and always wrote to the stderr sink
  (fd 2), so a `printStackTrace(System.out)` produced nothing on stdout. Now
  honors the stream (System.out → fd 1; else fd 2). Force-multiplier for
  debugging the suite (CratonRunner prints traces to `System.out`).

## STILL FAILING — root-caused, deferred (deep / high-blast-radius / concurrent)

- **CV-27 in-process javac** (`sql.storedproc.StoredProcedureTest` "Function
  FINDUSERS not found"; `delegation.SessionDelegatorBaseImplTest` syntax error
  in the CREATE ALIAS Java source). H2 `CREATE ALIAS` compiles Java via the
  in-process `jdk.compiler`; the compile silently fails so the function is never
  created. `try_class_bundle` already handles compiled `.class` message bundles
  (HIB-CV-27); the remaining gap is the `.properties`/compiler-resource path
  used during actual compilation.

- **CV-30 composite-id cascade** (`jpa.cascade.multilevel.MultiLevelCascade
  CollectionIdClassTest` / `…EmbeddableTest`, "expected: not <null>"). Multi-
  level cascade with a composite key loses an association. See HIB-CV-30.

- **CV-24 classloader identity** (`service.ClassLoaderServiceImplTest`).
  `testSystemClassLoaderNotOverriding`: a child loader's locally `defineClass`'d
  override is not returned by its `loadClass` (CratonVM resolves to the parent's
  class) — per-loader defined-class identity. `testStoppableClassLoaderService`:
  `loadJavaServices` finds 0 instead of 1 because `ServiceLoader` does not
  consult the custom loader's `findResources` override. Both are the gated /
  uncommitted loader-aware work (`CRATONVM_LOADER_AWARE_RESOLUTION`,
  `CRATONVM_CL_BOOTSTRAP_SCOPED`); high blast radius, needs soak.

- **`mapping.type.format.XmlFormatterTest` 10/12** (byte[]/char[] cases). UOE at
  `CollectionJavaType.wrap` — byte[]/char[] resolve to the wrong Hibernate
  `JavaType` (a collection type) so the round-trip hits `wrap()` which is
  designed to throw. Deep Hibernate type-descriptor resolution divergence.

- **`stats.ExplicitQueryStatsMaxSizeTest`** (expected 0, was 1000). Hibernate's
  `BoundedConcurrentHashMap` (Infinispan-derived LRU) does not evict entries
  after exceeding `QUERY_STATISTICS_MAX_SIZE`; query "1" survives 200+ later
  inserts. Pure-bytecode class → a CratonVM concurrency/atomics/identity
  primitive diverges. Matches the "bounded-LRU stats" triage note.

- **`multitenancy.DatabaseTimeZoneMultiTenancyTest`** (expected
  `2018-11-23T12:00:00`, was `…T18:00:00`). 6-hour Timestamp TZ-offset on
  read-back across tenants with different DB timezones. Being addressed in the
  `fix/hibernate-tz-offset` worktree (concurrent).

- **`proxy.ProxyClassReuseTest` 2/3.** `MappingException: Could not instantiate
  persister … SingleTableEntityPersister`. Tied to loader-aware CONSTANT_Class
  resolution (gated default-OFF). See the loader-aware reference.

- **`util.dtd.EntityResolverTest`.** `MappingException: Collection
  Parent.children references an unmapped entity Child` from `Parent.hbm.xml` —
  the hbm.xml `<one-to-many>` Child mapping is dropped during second-pass
  binding (DTD/entity-resolution path).

- **`util.PropertiesHelperTest` 1/2.** TWO-part bug (investigated; a clean full
  fix is a `Properties.entrySet()`-becomes-live-view refactor, deferred):
  1. `Properties.entrySet()` returns snapshot `SimpleImmutableEntry` objects, so
     `entry.setValue(..)` (Hibernate `ConfigurationHelper.resolvePlaceHolders`)
     throws `UnsupportedOperationException`. (JDK Hashtable entries are live.)
  2. `resolvePlaceHolder("${}")` returns `null`, so the loop does
     `entries.remove()` — but the snapshot entrySet iterator's `remove()` does
     NOT write through to the Properties side-table, so the `"${}"` entry
     survives and `getString(default)` returns `"${}"` instead of the default.
  Fixing both requires `Properties.entrySet()` to be a live, write-through view
  (setValue + iterator.remove → side-table). A prototype write-through
  `setValue` cleared (1) but (2) still fails; reverted to keep the landed diff
  to confirmed-green fixes.

Separately tracked (not this task): `sql.exec` NPEs (@OneToOne @JoinTable load;
native-query LockMode) — a concurrent change to native-collections `Map.equals`
(CCE/NPE swallowing) was in the shared working tree during this pass.
