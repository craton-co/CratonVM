# DaCapo luindex / sunflow / fop on CratonVM — root causes (2026-06-01)

All three pass on HotSpot + TornadoVM (JDK 25) but fail on CratonVM. Three
distinct root causes; none is the BigInteger/GC family fixed earlier this round.

## luindex — RandomAccessFile.getFD()==null + writes don't persist

**Symptom.** `InvocationTargetException` at `Luindex.iterate`; the wrapped
cause is `NullPointerException: Cannot invoke sync on null` at
`org.apache.lucene.store.FSDirectory.sync` (Lucene 2.4), then
`FileNotFoundException: no segments* file found`. The index never commits.

**FSDirectory.sync (bytecode pc 41-58):**
`new RandomAccessFile(file,"rw").getFD().sync()` — `getFD()` returns **null**,
so `.sync()` NPEs.

**Minimal repro (no Lucene):** `RafTest2`
```java
RandomAccessFile raf = new RandomAccessFile(f, "rw");
raf.write(new byte[]{1,2,3,4});
// reflect this.fd  -> NULL          (HotSpot: a FileDescriptor)
// raf.getFD()      -> NULL          (HotSpot: present, valid)
// close, reopen, read back -> n=-1  (HotSpot: 4 bytes)  ← WRITE DID NOT PERSIST
```
So on CratonVM a real-JDK `RandomAccessFile` (a) never gets a non-null `fd`
FileDescriptor, and (b) its writes are silently lost.

**Root cause = the documented dispatch-bypass family.** CratonVM ships a
complete fd_table-backed RAF implementation:
`native-builtins/phases_late::register_phase57_random_access_file` (public API:
`<init>`/read/write/seek/getFD/…) and `native-io::random_access_file`
(`open0`/`read0`/`write0`/getFD via `this.fd.fd`). **None of these natives
fire** — confirmed: `CRATONVM_DBG_RAF_INIT=1 CRATONVM_DBG_RAF_GETFD=1` produce
ZERO trace lines. Dispatch runs the real-JDK RAF bytecode instead, whose
platform `open0`/`read0`/`write0` natives CratonVM does not functionally
implement → writes go nowhere and `this.fd` is never populated.

**Force-native does NOT reach it (tried, reverted).** Adding
`java/io/RandomAccessFile` methods to
`interpreter.rs::force_native_over_real_jdk_bytecode` had no effect — getFD
still null, writes still lost, natives still don't trace. So the RAF invoke
path does not consult `intercept_force_registered_native` (the two call sites at
interpreter.rs ~9701/~10990 are bypassed for these calls — likely a
fast/stackless/JIT invoke path, or `getFD`'s 1-line `getfield` body is inlined
before invoke dispatch). This matches the continue_prompt's standing analysis
("registered natives never fire; suspect interpreter inlining / dispatch
keying").

**Fix paths (in order of leverage):**
1. Find why `intercept_force_registered_native` is bypassed for RAF invokes and
   route those invokes through it (would make the existing fd_table natives win
   — they're complete and unit-tested). This also unblocks the Tomcat NIO
   Selector + other dispatch-bypass cases.
2. Implement the real-JDK platform RAF natives (`open0`/`read0`/`write0`/`length0`
   /`seek0`/`close0`) so the real bytecode path works and populates `fd`.

## sunflow — AWT / Java2D native surface missing

**Symptom.** `UnsatisfiedLinkError` swallowed (B6) in `java/awt/Toolkit.<clinit>`
(`initStatic` → `IIORegistry.getDefaultInstance` → `ImageIO.<clinit>`) and
`sun/java2d/Disposer.<clinit>`, reached from `org.sunflow.Benchmark.<init>` via
`javax.imageio.ImageIO`. rc=127.

**Root cause.** sunflow renders and writes images through ImageIO/AWT/Java2D,
whose platform natives (Toolkit, Disposer, the image codecs) CratonVM doesn't
implement. The swallowed UnsatisfiedLinkErrors leave the imaging stack
half-initialized; the benchmark then fails.

**Scope.** Implementing the AWT/Java2D/ImageIO native surface is large and
out of proportion to one benchmark. Lower priority than luindex.

## fop — ClassLoader methods dispatched on java/lang/Object + SEGV

**Symptom.** Repeated `NoSuchMethodError method="java/lang/Object.getParent()
Ljava/lang/ClassLoader;"` / `getResource(...)` / `getResourceAsStream(...)`,
then `<clinit> failed` for `org/apache/batik/dom/svg/SVGDOMImplementation`
(`UnsupportedOperationException`), ending in rc=139 (SEGV).

**Root cause (hypothesis).** A `ClassLoader` is being modelled/resolved as a
bare `java/lang/Object` (so `getParent`/`getResource*` don't resolve on it) —
the same wrong-receiver-type / "flat classloading" family surfaced in the
avrora session (CratonVM attributes custom-loaded classes to the AppClassLoader
and sometimes hands back an `Object`-typed loader). Batik (FOP's SVG layer)
walks the classloader hierarchy + loads resources, hitting it; the SVG DOM
`<clinit>` then throws and the process SEGVs. Related to the
classloader-isolation gap, not the RAF or AWT issues.

## Summary
| bench | root cause | tractability |
|---|---|---|
| luindex | dispatch-bypass: fd_table RAF natives never fire; real-JDK RAF I/O non-functional | medium — needs the dispatch-bypass fix (high leverage, also unblocks Tomcat selector) |
| sunflow | AWT/Java2D/ImageIO native surface missing | low — large native surface |
| fop | ClassLoader-as-Object (flat-classloading family) + SVG clinit + SEGV | medium-hard — classloader-isolation work |
