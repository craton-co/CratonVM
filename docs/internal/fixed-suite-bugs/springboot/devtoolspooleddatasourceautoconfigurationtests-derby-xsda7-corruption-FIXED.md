# `DevToolsPooledDataSourceAutoConfigurationTests.inMemoryDerbyIsShutdown` — Derby `XSDA7` boot corruption

**Status: FIXED — verified 2026-07-26.** The fix landed as a side effect of an
unrelated closure session before this doc was picked back up: commit
`2a6a57bf8` (`fix(spring-jdbc): close loader, Derby, UCP, and charset
residuals`, 2026-07-24, see
[`spring-boot-jdbc-closure-20260724-FIXED.md`](spring-boot-jdbc-closure-20260724-FIXED.md)
in this directory). That session was closing out `module/spring-boot-jdbc`
residuals and didn't know this `module/spring-boot-devtools` doc existed, so
it never updated/retired it — this doc was reproducing a symptom of a bug
already fixed under a different name.

## Original symptom (for context)

The only test in the class that touches Apache Derby's real embedded engine
(`inMemoryDerbyIsShutdown`, using `jdbc:derby:memory:test;create=true`) failed
during database boot with a chain ending in:

```
Caused by: ERROR XSDA7: Restore of a serializable or SQLData object of class , attempted to read more data than was originally stored
Caused by: java.io.EOFException: Unexpected EOF
	at org.apache.derby.iapi.services.io.FormatIdUtil.readFormatIdInteger(FormatIdUtil.java:66)
	at org.apache.derby.iapi.services.io.FormatIdInputStream.readObject(FormatIdInputStream.java:75)
	at org.apache.derby.iapi.services.io.FormatableHashtable.readExternal(FormatableHashtable.java:168)
	at org.apache.derby.catalog.types.IndexDescriptorImpl.readExternal(IndexDescriptorImpl.java:301)
	...
	at org.apache.derby.impl.sql.catalog.DataDictionaryImpl.create_SYSIBM_procedures(DataDictionaryImpl.java:11526)
	at org.apache.derby.impl.sql.catalog.DataDictionaryImpl.boot(DataDictionaryImpl.java:781)
```

## Actual root cause (this doc's original hypothesis was wrong)

The 2026-07-23 investigation (see git history of this file) suspected the
Derby `jdbc:derby:memory:` synchronous-bypass native (`native-builtins/src/jdbc.rs`,
`register_derby_embedded_connection_native`) exposed a thread-context/`synchronized`-
dispatch gap by running Derby's boot on the calling thread instead of Derby's
own executor thread. **That hypothesis was never confirmed and was, in fact,
wrong** — see [[known-issue-doc-hypothesis-can-be-wrong-not-just-stale]].

The real defect (fixed by `2a6a57bf8`, `native-collections/src/lib.rs`):
Derby's `FormatableHashtable` extends `java.util.Hashtable`, but CratonVM's
native Map/Hashtable storage helpers (`map_state`, `set_map_size`,
`map_resize_inner`, `native_map_init`) assumed every Hashtable-family object
used `HashMap`'s field layout/`size` bookkeeping. That size-field mismatch
made Derby serialize a `FormatableHashtable` (used to persist
`IndexDescriptorImpl` and other catalog metadata) with a truncated byte
stream — exactly the "attempted to read more data than was originally
stored" `XSDA7`/`EOFException` signature above. This has nothing to do with
which thread runs the boot; it reproduces (or doesn't) identically regardless
of calling thread, which is why background-thread vs. main-thread testing
during the original investigation didn't correlate with the failure.

The fix added `uses_native_hashtable_layout()` (walks the class hierarchy,
true for `Hashtable` subclasses other than `Properties`) and made
`map_state`/`set_map_size`/`map_resize_inner`/`native_map_init` honor
`Hashtable`'s own `count`/`threshold` fields and bucket layout for any class
in that family — not just `java.util.Hashtable` itself — which is exactly
what `FormatableHashtable` needed.

## Verification (2026-07-26)

Confirmed fixed by re-running the exact defect on the Azure Linux build host
(`/data/data/wt/wt-derby-xsda7-20260726`, branch `fix/derby-xsda7-boot-corruption`
off `origin/dev` @ `28485dcdd`, binary `cratonvm-derbyfix-base`):

1. **Minimal repro** (`DerbyBootProbe.java`): bare
   `new EmbeddedDriver().connect("jdbc:derby:memory:test;create=true", ...)`
   + `SELECT 1 FROM SYSIBM.SYSDUMMY1`, both on the main thread and on a
   spawned background thread (to directly test the original thread-context
   hypothesis) — both pass (`PROBE_OK`).
2. **Faithful repro** (`DerbyHikariSpyProbe.java`): mirrors the actual test
   body — `HikariDataSource` pointed at `jdbc:derby:memory:test;create=true`
   built on a background thread (matching `AbstractDevToolsDataSourceAutoConfigurationTests.getContext`'s
   thread-spawning helper), wrapped in `Mockito.spy()` (matching
   `DataSourceSpyBeanPostProcessor`), then `getConnection()` +
   `SELECT 1 FROM SYSIBM.SYSDUMMY1` + the pool-idle wait + Derby shutdown
   sequence. Ran 4× against `cratonvm --java-home <jdk25>` with no
   `synthetic-jdk` — all 4 runs: `PROBE_OK idle=1`, and the post-shutdown
   `connect("jdbc:derby:;shutdown=true")` SQLSTATE (`XJ015`) matches real
   HotSpot's (Eclipse Temurin 25.0.3.9) output on the same classpath exactly.
   No `XSDA7`/`EOFException` in any run.

No code change was needed this session — the fix was already on `dev`. This
doc is retired (moved from `docs/known-issues/springboot/` to
`docs/internal/fixed-suite-bugs/springboot/`) per
[[docs-known-issues-convention]].

## Residuals / related open docs (NOT covered by this fix, left open)

One other `module/spring-boot-devtools` doc shares a directory with this one
but is an **independent bug, unaffected by the `2a6a57bf8` fix** — checked
and confirmed still open, out of scope for this closure:

- [`devtools-2class-host-load-confound.md`](../../../known-issues/springboot/devtools-2class-host-load-confound.md) —
  a genuine hang in `DevToolPropertiesIntegrationTests` and a Mockito
  self-attach `NoClassDefFoundError` in `DevToolsEmbeddedDataSourceAutoConfigurationTests`
  (shared root cause with the Tomcat suite's `ForkedClassPath`/`MockMethodAdvice`
  bug, per that doc).
