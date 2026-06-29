# HIB-CV-35 — Long-tail correctness divergences (inventory)

**Run:** full Hibernate ORM suite, 2026-06-22/23 (`--nojit`, the un-JIT-contaminated run)
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Status:** Inventory of the remaining CratonVM-only, non-timeout, HotSpot-PASS failures
not already captured as their own root-cause report (HIB-CV-22…34).

Full machine list: [`INVENTORY-cvonly-real-fails.tsv`](INVENTORY-cvonly-real-fails.tsv)
(50 classes total; the ones already attributed to HIB-CV-22…34 are excluded below).

---

## Sub-groups (each likely its own smaller bug)

### a. Sorted-set ordering
- `sorted.set.SortComparatorTest` — `AssertionFailedError`
- `sorted.set.SortNaturalTest` — `AssertionFailedError`
Likely a `TreeSet`/`Comparator`/natural-ordering divergence (iteration order or
comparison result differs from HotSpot).

### b. `UnsupportedOperationException` thrown where HotSpot succeeds
- `mapping.type.format.XmlFormatterTest`
- `util.PropertiesHelperTest`
A platform API path is unimplemented/over-restrictive on CratonVM (XML formatting;
`Properties` helper). Check the exact call sites — small, likely-cheap fixes.

### c. Deserialization / classloading can't resolve an application class
- `util.SerializationHelperTest` — `ClassNotFoundException: ...SerializableThing`
- `proxy.ProxyClassReuseTest` — `ClassNotFoundException: ...ProxyClassReuseTest$ProxyGetter`
`ObjectInputStream`/proxy resolution fails to find an app/inner class that exists
— resolution likely not using the right (caller/thread-context) classloader.
Related to [HIB-CV-24](HIB-CV-24-classloader-isolation-delegation.md) (classloader)
and [HIB-CV-29](HIB-CV-29-deserialize-list-not-in-base-module.md) (serialization).

### d. Statistics / session-state value divergences
- `stats.ExplicitQueryStatsMaxSizeTest` — `expected:<0> but was:<1000>`
- `stateless.StatelessSessionPersistentContextTest` — "PersistenceContext has not been cleared" `expected:<true> but was:<false>`
- assorted bare `AssertionError` / `expected:<N> but was:<N>` (see inventory)
Wrong computed values / state not reset — individual Hibernate-behavior divergences.

### e. SQL grammar (stored-proc family)
- `sql.storedproc.ResultMappingTest` — `Function "FINDONEUSER" not found` →
  same in-process-javac root as [HIB-CV-27](HIB-CV-27-inprocess-javac-message-bundle-broken.md).

### f. `.par` archive URL
- `mapping.fetch.depth.NoDepthTests` — `Could not create URL for archive: fetch-depth.par`
URL construction for a persistence-archive (`.par`) resource fails; may be a
URL/resource-handling gap (verify it is not a missing test resource).

### g. Non-SIGSEGV crash variants (CV-only, HS PASS)
- `bootstrap.scanning.JarVisitorTest` — process exited rc=0 mid-class (no result)
- `dynamicmap.DynamicMapOneToOneTest` — process died rc=127 mid-class
Distinct from the SIGSEGV crashes ([HIB-CV-32](HIB-CV-32-sigsegv-blob-bytearray-bind.md),
[HIB-CV-33](HIB-CV-33-sigsegv-execute-fault-joined-inheritance-sf-build.md)); the
VM terminates without a normal result and without a SIGSEGV dump — investigate
for an unexpected `System.exit`/abort path.

---

## Note on the 35 `--nojit` HANGs

Of the 35 CratonVM-only HANGs (HotSpot PASS), most are **interpreter slowness**,
not deadlocks: the largest are big query test classes (`HQLTest` 169 tests,
`FunctionTests` 123, `ASTParserLoadingTest` 106, `BulkManipulationTest` 51, …) that
exceed the 300 s class cap purely from per-test cost under the interpreter. The
genuine non-slowness hang is the `Ejb3Xml*` / orm.xml mapping family —
[HIB-CV-23](HIB-CV-23-nojit-hang-orm-xml-mapping-processing.md) — which hangs on
its *first* test. (With a working JIT the slowness HANGs would shrink dramatically;
see [HIB-CV-21](HIB-CV-21-jit-hangs-hibernate-orm-bootstrap-UMBRELLA.md).)
