# `untriaged-oddities.md` closed — both classes were the shared Hashtable size-doubling bug / a fixture typo (FIXED)

**Status:** Both classes tracked in
`../../../known-issues/tomcat/untriaged-oddities.md` are closed.
**Test:** `org.apache.catalina.startup.TestTomcat` — now **26/26 PASS**
clean under CratonVM (was 3 failures / an occasional 300s HANG depending on
host load). Matches HotSpot.

## `org.apache.catalina.startup.TestTomcat` — "Deliberately Broken"

The `LifecycleException: Deliberately Broken` text that gave this bucket its
name was always a red herring, never the actual failure cause:
`testBrokenWarOne`/`testBrokenWarTwo` deliberately trigger and catch that
exact exception (confirmed by reading `TestTomcat.java`) — both always pass.
The real, separate failure was `testJsps` (and, once fixture-completion work
landed a real `webapps/examples`, `testSingleWebapp`/`testGetResource` too):

```
org.apache.jasper.JasperException: Unable to compile class for JSP
Caused by: java.lang.NullPointerException: Cannot assign field
  "referenceBinding" because "classFile" is null
    at org.eclipse.jdt.internal.compiler.ast.CompilationUnitDeclaration.cleanUp
    at org.eclipse.jdt.internal.compiler.Compiler.processCompiledUnits
    at org.apache.jasper.compiler.JDTCompiler.generateClass
```

This is the **same bug** already root-caused and fixed on `dev` by a
concurrent session investigating a *different* pair of failing classes
(`TestDefaultServlet.testBug57601`, `TestMapperWebapps.testWelcomeFileStrict`)
— see commits `744401af4` (`fix(vm): force native Hashtable/HashMap
put/get/size on ALL dispatch paths`), `b854dc01f` (`fix(collections):
resolve map size field against receiver's own class, not hardcoded
HashMap`), merged via `a517d31b4` (`Merge Hashtable/HashMap dispatch +
size-field-collision fixes`). Short version: `java.util.Hashtable`'s
`put`/`get`/`size` weren't consistently native-dispatched on every call
path, and where the native size-tracking helpers (`map_state`/
`set_map_size` in `native-collections/src/lib.rs`) *were* used, they
resolved the size field via a hardcoded `resolve_field_index("java/util/
HashMap", "size")` regardless of the actual receiver's class — for a
`Hashtable` (no `size` field at all; its own field is `count`) this landed
on whatever field occupies that slot number in `Hashtable`'s own layout,
which turned out to be its real `modCount` field. Every `put()` then
double-bumped that slot, doubling `Hashtable.size()`.

Jasper's embedded ECJ Java compiler keys its `CompilationResult
.compiledTypes` (the generated JSP servlet class it's about to write out) in
a plain `new Hashtable(11)`. The inflated `size()` over-allocated
`getClassFiles()`'s destination array (`new ClassFile[compiledTypes.size()]`
then `.values().toArray(classFiles)`), which — per the standard JDK
`AbstractCollection.toArray(T[])` contract — null-pads the trailing slots
once the live collection runs out, and `CompilationUnitDeclaration
.cleanUp()` crashed on the first null it hit. That broke **JSP compilation
under CratonVM entirely**, not just these specific test classes.

This session found and confirmed the exact same root cause independently
(before discovering the fix had already landed on `dev`), via `TestTomcat`
rather than `TestDefaultServlet`/`TestMapperWebapps`: confirmed
byte-identical generated `<jsp>_jsp.java` between CratonVM and HotSpot,
reproduced with `--nojit` (rules out JIT) and `--Xmx 8g` with zero logged GC
events (rules out GC-move/identity-hash instability), reproduced down to a
minimal standalone `Hashtable.put()`/`.size()` probe (independent of ECJ/
Jasper entirely — plain `HashMap` unaffected), and traced the doubling to
the same `map_state`/`set_map_size` field-slot collision described above.
No further code change was needed — `dev`'s existing fix (merged the same
day, shortly before this investigation reached the same conclusion) already
resolves it. Verified against a freshly built binary at `dev`'s tip:

* `Hashtable.put()`/`.size()` probes (String/`Object`/`char[]` keys) now
  match HotSpot exactly.
* `org.apache.jasper.JspC ... -compile` on both a trivial static JSP and
  `webapps/examples/jsp/jsp2/el/basic-arithmetic.jsp` — `Generation
  completed with [0] errors`, real `.class` produced.
* `org.apache.catalina.startup.TestTomcat` — 26/26 PASS.

See `744401af4`'s and `b854dc01f`'s commit messages for the full fix
rationale; `a517d31b4`'s merge message notes the same root cause is shared
by 8 of the 9 classes in
[hang-classification-unconfirmed-host-contention.md](../../../known-issues/tomcat/hang-classification-unconfirmed-host-contention.md)
(still open as of this writing — that doc's own status wasn't updated by
this session, out of scope here).

## `org.apache.jasper.compiler.TestNonstandardTagPerformance` — self-referential `ClassNotFoundException`

Unrelated, not a CratonVM bug — a fixture-data typo, independent of the
above. `.suite/all-tests.txt` (fixture-local, not git-tracked, per this
folder's `README.md`) had a typo at line 452:
`org.apache.jasper.compiler.TestNonstandardTagPerformance`, but the real
class is `TesterNonstandardTagPerformance` (note the "er"). Its own source
comment explains the `Tester` prefix is deliberate: "This test requires
additional setup and cannot be run as part of a standard test run so it is
excluded due to the name starting Tester..." — it's a 100,000,000-iteration
manual EL-arithmetic benchmark, not a functional test, and was never meant
to be collected into a standard run. Confirmed this is the *only* bogus
entry in the fixture's 646-class list (every other line has a matching
compiled `.class`). Fixed directly on the Azure host's fixture
(`/data/data/tomcat-dohead-fixture-20260717/.suite/all-tests.txt`, bogus
line removed) — no git action needed, nothing to fix in the repo itself.
