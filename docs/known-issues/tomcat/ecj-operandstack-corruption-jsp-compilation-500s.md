# ECJ's own `OperandStack.pop()` throws `AssertionError: Unexpected operand at stack top` while compiling JSPs — 7+ classes

| | |
|---|---|
| **Status** | OPEN — high priority, broad blast radius |
| **Severity** | high — any test that compiles a JSP is at risk; hits both correctness (500s) and throughput (classes that retry/recompile run far longer) |
| **HotSpot** | PASS on every class checked |
| **CratonVM** | FAIL/HANG, reproduces consistently |
| **Discovered** | 2026-08-06, complete 651-class Tomcat suite rerun (1-shard + 4-shard remainder) after merging `dev` (~1044 commits, including the `interpreter.rs` big-file split) |

## Symptom

Jasper's JSP-to-`.java`-to-`.class` compilation pipeline uses Eclipse's ECJ
(`org.eclipse.jdt.internal.compiler`) as its Java compiler backend. ECJ
maintains its own operand-stack simulation while generating bytecode
(`OperandStack`, a plain Java class tracking JVM stack depth during codegen).
That internal bookkeeping is now inconsistent on CratonVM and trips ECJ's own
assertion:

```
06-Aug-2026 11:13:21.760 SEVERE [http-nio-...] org.apache.catalina.core.StandardWrapperValve.invoke
Servlet.service() for servlet [jsp] in context with path [/test] threw exception
[java.lang.AssertionError: Unexpected operand at stack top] with root cause
java.lang.AssertionError: Unexpected operand at stack top
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.eclipse.jdt.internal.compiler.codegen.OperandStack.pop(OperandStack.java)
	at org.eclipse.jdt.internal.compiler.codegen.CodeStream.fieldAccess(CodeStream.java:1368)
	...
```

The exact call site inside `CodeStream` varies by run (`fieldAccess`,
`areturn`/`StackMapFrameCodeStream.areturn`, `pop`/`MessageSend.generateCode`
inside a `<clinit>`) — it is not one fixed bytecode shape, it fires from
whatever codegen path a given JSP's generated `.java` happens to exercise.
Each occurrence surfaces to the HTTP client as a 500 (`expected:<200> but
was:<500>`) from the servlet container's own exception-to-status-code
handling.

## Confirmed affected classes

All produce this exact `AssertionError` on CratonVM and pass cleanly on
HotSpot 25.0.3:

- `org.apache.jasper.compiler.TestEncodingDetector` — 5 method failures, all `expected:<200> but was:<500>`
- `org.apache.jasper.compiler.TestJspDocumentParser` — 6 method failures, same shape
- `org.apache.jasper.compiler.TestParser` — 7 method failures, same shape (`testBug56265`'s failure body is literally Tomcat's rendered 500 page quoting this `AssertionError`)
- `org.apache.jasper.TestJspCompilationContext` — 1 method failure (`testTagFileInJarIncludesValid`)
- `org.apache.catalina.core.TestStandardContextResources` — `testResourcesWebInfClasses`, same shape; this class is **not** in `org.apache.jasper` at all, confirming the bug is reachable from any JSP-serving test, not specific to the jasper-compiler package
- `org.apache.jasper.compiler.TestGenerator` — hit **10 times** in one run (grep count on the class's `.log.err`); this is very likely why the class times out under a 300s budget rather than a genuine multi-hundred-second-per-method slowdown
- `org.apache.jasper.compiler.TestJspConfig` — hit **4 times**

Checked and **not** hitting this signature (their slowness looks like
ordinary long runtime, not corruption-induced retries):
`org.apache.jasper.compiler.TestValidator`, `org.apache.jasper.optimizations.TestELInterpreterTagSetters`.

HotSpot control, e.g. `TestEncodingDetector`: `OK (22 tests)` in 6.1s.

## Suspected connection to the GC moving-young fallback regression (not proven)

Every occurrence observed so far is preceded in the log by
`cratonvm_gc::gc_quiescence` `[moving-young] fallback #N` warnings (see
[gc-moving-young-persistent-nonmoving-fallback-regression.md](gc-moving-young-persistent-nonmoving-fallback-regression.md)
for the broader slowdown this same fallback correlates with). ECJ's bytecode
generator is exactly the kind of allocation-heavy, long-lived-object-graph
workload that would be most exposed by a moving-young collector silently
running non-moving sweeps instead — a stale/aliased reference into ECJ's own
`OperandStack` internal array would produce precisely this "impossible"
assertion. This is a lead, not a confirmed root cause; it has not been
verified by disabling the fallback and re-running.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.jasper.compiler.TestEncodingDetector
```

## Suggested next step

Bisect the ~1044 commits merged into `dev` on 2026-08-06 for anything
touching the moving-young GC path or `interpreter.rs`'s new split
(`vm/src/runtime/interpreter/{dispatch_virtual,gc_and_alloc,...}.rs`), since
this class of "third-party library's own internal invariant breaks" bug has
historically traced back to CratonVM heap/root-tracking defects rather than
anything in the library itself.
