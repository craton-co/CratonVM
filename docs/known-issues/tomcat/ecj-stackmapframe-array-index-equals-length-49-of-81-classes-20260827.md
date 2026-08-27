# Eclipse JDT's `StackMapFrameCodeStream.getFramePositions` throws `ArrayIndexOutOfBoundsException` on CratonVM — accounts for 49 of 81 (60%) of tomcat's non-passing classes

## Status
**OPEN, not root-caused to the specific CratonVM defect.** Confirmed real via
direct HotSpot A/B (identical class, identical setup, identical Tomcat/Jasper
version — HotSpot passes, CratonVM throws). This is very likely the single
largest known contributor to tomcat's non-passing count right now.

## How this was found

2026-08-27: retested the complete tomcat suite's existing 81-class
fail/hang/crash set (per `-RefCsv` against the 2026-08-25 ZGC baseline
results.csv) in isolation, 1 shard, quiet host, fresh `dev` HEAD `<latest
merged this session>`. 74 of 81 still FAIL. Grepping every failing class's
stderr for the exception:

```
grep -l "StackMapFrameCodeStream" *.log.err | wc -l
=> 49
```

**49 of the 81 classes (60%) — and 49 of 74 FAILs (66%)** — hit the exact same
exception, at the exact same frame.

## The exception

```
27-Aug-2026 07:43:08.778 SEVERE [http-nio-...] org.apache.catalina.core.StandardWrapperValve.invoke
  Servlet.service() for servlet [jsp] threw exception [org.apache.jasper.JasperException: Unable to compile class for JSP] with root cause
java.lang.ArrayIndexOutOfBoundsException: Index 3 out of bounds for length 3
	at org.eclipse.jdt.internal.compiler.codegen.StackMapFrameCodeStream.getFramePositions(StackMapFrameCodeStream.java:193)
	at org.eclipse.jdt.internal.compiler.ClassFile.traverse(ClassFile.java:6379)
	at org.eclipse.jdt.internal.compiler.ClassFile.generateStackMapTableAttribute(ClassFile.java:5115)
	at org.eclipse.jdt.internal.compiler.ClassFile.completeCodeAttribute(ClassFile.java:1590)
	at org.eclipse.jdt.internal.compiler.ast.AbstractMethodDeclaration.generateCode(AbstractMethodDeclaration.java:426)
	...
	at org.apache.jasper.compiler.JDTCompiler.generateClass(JDTCompiler.java:528)
	at org.apache.jasper.compiler.Compiler.compile(Compiler.java:408)
	at org.apache.jasper.JspCompilationContext.compile(JspCompilationContext.java:746)
	at org.apache.jasper.servlet.JspServletWrapper.service(JspServletWrapper.java:441)
```

Jasper (Tomcat's JSP engine) compiles every JSP page to a `.java` servlet
source at request time, then compiles THAT to bytecode using Eclipse's own
compiler (`ecj`, bundled as `org.eclipse.jdt...JDTCompiler`) rather than
`javac`. `StackMapFrameCodeStream` is ecj's own internal machinery for
emitting the `StackMapTable` class-file attribute (required for bytecode
verification on any generated class targeting Java 6+). The AIOOBE happens
inside ecj's own code, on data ecj itself builds — this is not JSP-specific,
Tomcat-specific, or Jasper-specific; it is triggered by whatever bytecode
shape a JSP-generated servlet class happens to produce, on the JVM ecj itself
is running on.

## Always `index == length`

```
grep -h "ArrayIndexOutOfBoundsException: Index" *.log.err | sort | uniq -c
    348 Index 3 out of bounds for length 3
      2 Index 2 out of bounds for length 2
      2 Index 1 out of bounds for length 1
      1 Index 9 out of bounds for length 9
```

Every single occurrence, across all 49 classes, has the requested index
exactly equal to the array's current length — never `index > length+1`, never
negative. That shape is the signature of an off-by-one in an array-growth
check: something should have grown the backing array by one slot before this
write/read and did not. Since HotSpot never hits this on the identical class,
identical bytecode-under-compilation, identical ecj version — the growth-check
computation itself must be evaluating differently under CratonVM.

## Confirmed via HotSpot A/B

```powershell
.\run-tomcat-suite.ps1 -Vm hotspot -Category all -Start 6 -Count 1 -RunName hs-spot1 -TimeoutSec 60
=> PASS  jakarta.el.TestCompositeELResolver   (2s)
```
Same class, same classpath, same host, same JDK — CratonVM: `AssertionError:
expected:<200> but was:<500>` (the 500 being Tomcat surfacing the
`JasperException`/AIOOBE above); HotSpot: clean pass.

## Not yet done — this is where the real investigation starts

- **Not isolated to a standalone ecj repro.** Every occurrence so far is
  inside a live Tomcat JSP-compile request; a minimal repro (feed
  `JDTCompiler`/ecj some fixed `.java` source directly, outside Tomcat) would
  isolate this from Jasper/Tomcat entirely and make it a much faster,
  cheaper reproduction loop.
- **Not traced into `StackMapFrameCodeStream.java:193`** — haven't pulled the
  ecj source to see what array is being indexed there or what's supposed to
  grow it. That's the next step to turn "off-by-one in a growth check" from an
  inference into a diagnosis.
- **Not narrowed to which CratonVM subsystem is responsible.** Candidates not
  yet ruled in or out: a JIT-compiled method computing a wrong bound/length
  for `StackMapFrameCodeStream`'s internal array, an interpreter bytecode
  producing a stale/cached length read, or a GC/heap-shape difference
  (array resize typically means allocate-bigger + `System.arraycopy` +
  reassign — a stale reference to the pre-grow array anywhere in that
  sequence would produce exactly this).
- **Blast radius beyond these 49 classes not established.** These are 49 of
  the 81 classes that were ALREADY non-passing before this session (the
  `-RefCsv` selection is "everything not PASS in the 2026-08-25 baseline") —
  not checked whether this same AIOOBE also silently affects any class
  currently counted PASS (e.g. a JSP compile that happens to produce bytecode
  shaped just under the AIOOBE's trigger threshold today, but wouldn't after
  an unrelated code change nudges array sizes by one).

## Repro

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Category all -Start 6 -Count 1 -GcFlag '-XX:+UseZGC' -RunName repro -TimeoutSec 60
# jakarta.el.TestCompositeELResolver is index 6 in .suite\all-tests.txt; any of the
# 49 classes below reproduces the same underlying exception
```

Full list of the 49 (2026-08-27, `grep -l StackMapFrameCodeStream *.log.err` in
`apps/tomcat/.suite/results/fhc-zgc1-20260827-074302/real-jit/`):
`jakarta.el.TestCompositeELResolver`, `jakarta.el.TestOptionalELResolverInJsp`,
`jakarta.servlet.TestSessionCookieConfig`, `jakarta.servlet.jsp.TestPageContext`,
`jakarta.servlet.jsp.el.TestImportELResolver`,
`jakarta.servlet.jsp.el.TestScopedAttributeELResolver`,
`org.apache.catalina.authenticator.TestFormAuthenticatorA/B/C`,
`org.apache.catalina.core.TestApplicationContext`,
`org.apache.catalina.core.TestApplicationDispatcher`,
`org.apache.catalina.core.TestStandardContextResources`,
`org.apache.catalina.core.TestStandardWrapper`,
`org.apache.catalina.loader.TestVirtualContext`,
`org.apache.catalina.mapper.TestMapperWebapps`,
`org.apache.catalina.servlets.TestDefaultServlet`,
`org.apache.catalina.startup.TestContextConfig`,
`org.apache.catalina.startup.TestTomcat`,
`org.apache.coyote.ajp.TestAbstractAjpProcessor`,
`org.apache.coyote.http11.TestHttp11Processor`, and 30 more jasper.*/catalina.*
classes in the same results directory — run the grep above against that
directory for the exact current list, since it will shift as classes are
added/removed from the non-passing set.
