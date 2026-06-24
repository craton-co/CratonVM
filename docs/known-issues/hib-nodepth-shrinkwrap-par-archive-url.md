# Hibernate `NoDepthTests` (JPA variants) — ShrinkWrap in-memory `.par` archive has no URL handler

| | |
|---|---|
| **Status** | ✅ FIXED (branch `fix/hib-nodepth-shrinkwrap-url`). `NoDepthTests` **4/4** on CratonVM (`--nojit`), matching HotSpot. |
| **Area** | VM — `URLClassLoader.addURL` / custom `URLStreamHandler` resource resolution / `URL.openStream` for ShrinkWrap's in-memory `JavaArchive` |
| **Symptom** | `org.hibernate.orm.test.mapping.fetch.depth.NoDepthTests` JPA variants fail: `RuntimeException: Could not create URL for archive: fetch-depth.par`. |
| **Severity** | low (CratonVM-only; pre-existing — fails identically at baseline `b0aab8f9`; only the 2 JPA variants of 4 tests are affected). |
| **Discovered** | 2026-06-24, triaging the Hibernate suite residuals after the collection-delegation stack-overflow fix (`7b224d8a`). |

## Symptom

`NoDepthTests` has 4 methods. The two non-JPA variants (`testWithMax`,
`testNoMax`) build a `SessionFactory` directly and **pass**. The two JPA variants
(`testWithMaxJpa`, `testNoMaxJpa`) construct an **in-memory** ShrinkWrap archive
and load a persistence unit out of it:

```java
final JavaArchive par = ShrinkWrap.create( JavaArchive.class, "fetch-depth.par" );
par.addClasses( SysModule.class );
par.addAsResource( "units/many2many/fetch-depth.xml", "META-INF/persistence.xml" );
try ( ShrinkWrapClassLoader classLoader = new ShrinkWrapClassLoader( par ) ) {
    …
    createEntityManagerFactory( "fetch-depth", settings );   // → fails here
}
```

These fail with:

```
java.lang.RuntimeException: Could not create URL for archive: fetch-depth.par
```

## Root cause

The original hypothesis ("CratonVM cannot construct a `URL` against ShrinkWrap's
in-memory handler") is **wrong** — `new URL(null, "archive:fetch-depth.par/",
handler)` (the 3-arg `URL(URL,String,URLStreamHandler)` ctor ShrinkWrap 1.2.6's
`ShrinkWrapClassLoader.addArchive` uses) succeeds on CratonVM exactly as on
HotSpot. The `RuntimeException: Could not create URL for archive` is ShrinkWrap
wrapping a *different* exception caught over the whole `try { new URL(...);
addURL(url); }` block (the 1.2.6 catch is `java.lang.Exception`, covering both).

The actual failure is a chain of three CratonVM gaps:

1. **`URLClassLoader.addURL` NPEs.** `ShrinkWrapClassLoader` extends
   `URLClassLoader`, calls `super(new URL[0], parent)`, then `addURL(archiveUrl)`.
   The real body is `ucp.addURL(url)`, whose bytecode does `synchronized
   (unopenedUrls)`. CratonVM's `ucp` is a bare synthetic `jdk.internal.loader
   .URLClassPath` (all instance fields null — it deliberately shims URLClassPath
   out and serves resources from the global dynamic classpath), so
   `unopenedUrls` is null →
   `NullPointerException: Cannot enter synchronized block`. This breaks
   construction of *every* real-mode `URLClassLoader` subclass that calls the
   protected `addURL`, not just ShrinkWrap.

2. **`getResource(s)` can't see the in-memory archive.** The persistence.xml lives
   only inside the heap-resident `JavaArchive`, reachable solely through
   ShrinkWrap's custom `archive:` `URLStreamHandler`. CratonVM's
   `getResource(s)`/`findResource(s)` walk the global dynamic classpath, which has
   no knowledge of it.

3. **`URL.openStream`/`openConnection` reject `archive:`.** Both natives
   string-parse the scheme (`file:`/`jar:`/`classpath:`/`http:`…) and throw
   "unsupported scheme: archive:" instead of delegating to the URL's
   app-supplied handler.

## Fix

- **`jdk.internal.loader.URLClassPath.addURL(URL)`** — register a native
  (`ucp_add_url`, `native-builtins/src/classloader.rs`) that records the URL on
  `ucp.path` (a real `ArrayList<URL>` created on demand) and extends the global
  dynamic classpath for ordinary `file:`/`jar:` URLs. Shimmed on `URLClassPath`
  (not `URLClassLoader`) because `addURL` is invoked via a subclass
  `this.addURL(url)` whose CP methodref names the subclass and escapes the
  static-class force-native gates; `ucp.addURL` always names `URLClassPath`.
- **`URLClassLoader.findResource`/`findResources`** (`ucl_find_resource(s)`) —
  after the global-classpath walk, probe the loader's recorded custom-handler
  base URLs: build `new URL(base, name)` (inherits the handler), confirm it
  resolves via `handler.openConnection(url).getInputStream()`, and return /
  enumerate the hits.
- **`URL.openStream`/`openConnection`** — when the URL carries a non-null,
  non-`sun.net.*` (application) handler, delegate to `handler.openConnection(url)`
  (mirroring the real `URL` body) via `url_custom_handler_connection`
  (`native-builtins/src/net_phase_e.rs`).
- **Force-native gates** — `URLClassPath.addURL` and `URLClassLoader
  .findResource(s)` are added to both `check_override`
  (`vm_exec.rs::invoke_on_class_shared_inner`) and
  `force_native_over_real_jdk_bytecode` (`interpreter.rs`) so the registered
  natives actually shadow the real JDK bytecode.

Archive-root scanning (`archive:fetch-depth.par/` openStream → null for the
directory node) degrades gracefully exactly as on HotSpot; the PU's explicit
`<class>` entries + the test's second classloader resolve the entities.

## Impact

- `NoDepthTests.testWithMaxJpa` / `testNoMaxJpa` (2 of 4; the other 2 pass).
- More broadly: `URLClassLoader.addURL` now works in real-JDK mode (was an
  unconditional NPE), and any in-memory / custom-`URLStreamHandler` archive is
  resolvable through `getResource(s)` + `openStream`.
