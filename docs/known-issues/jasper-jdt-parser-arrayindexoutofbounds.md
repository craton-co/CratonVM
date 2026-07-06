# JSP compilation fails: Eclipse JDT parser `ArrayIndexOutOfBoundsException`

**Status:** OPEN. **Severity:** medium (breaks JSP compilation for specific
source shapes; HotSpot compiles the same JSP/generated-Java successfully with
the same bundled JDT compiler).

## Summary

`org.apache.jasper.compiler.TestCompiler` (`testBug53257f`) fails compiling a
generated JSP servlet. The failure is inside **Eclipse JDT's own bundled Java
parser** (`org.eclipse.jdt.internal.compiler.parser.Parser`, the compiler
Jasper uses to compile generated `_jsp.java` sources to bytecode) — not
Jasper's JSP-to-Java translation itself. Since HotSpot compiles the identical
generated Java source through the identical bundled JDT parser without error,
the defect is in how CratonVM executes JDT's parser code (an interpreter/JIT
correctness bug exposed by JDT's specific bytecode patterns), not a JDT bug
per se.

Found via a full 651-class Apache Tomcat suite rerun (`osr600verify`, real
JDK, JIT on, `CRATONVM_JIT_OSR=1`, 600s timeout, dev `8cfd53bf`+). HotSpot
passes this class.

## Symptom

```
HTTP Status 500 — Internal Server Error
Message: org.apache.jasper.JasperException: Unable to compile class for JSP
Root Cause: java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 50
	at org.eclipse.jdt.internal.compiler.parser.Parser.consumeRule(Parser.java:7045)
	at org.eclipse.jdt.internal.compiler.parser.Parser.parse(Parser.java:11701)
	at org.eclipse.jdt.internal.compiler.parser.Parser.parse(Parser.java:12076)
	at org.eclipse.jdt.internal.compiler.ast.MethodDeclaration.parseStatements(MethodDeclaration.java:239)
	at org.eclipse.jdt.internal.compiler.ast.TypeDeclaration.parseMethods(TypeDeclaration.java:1093)
```
A second failure in the same class run shows a related but distinct site:
```
java.lang.ArrayIndexOutOfBoundsException (no message)
	at org.eclipse.jdt.internal.compiler.parser.Parser.consumeTypeImportOnDemandDeclarationName(Parser.java:9708)
	at org.eclipse.jdt.internal.compiler.parser.Parser.consumeRule(Parser.java:6721)
	at org.eclipse.jdt.internal.compiler.parser.Parser.parse(Parser.java:11701)
```

`Parser.consumeRule` is JDT's LALR-parser reduce-action dispatcher — it reads
back off an internal stack (`identifierStack`/`intStack`/similar arrays sized
50 by default) using an index computed from parser state. An `Index -1` means
that computed index went negative — either the stack pointer/counter field
that feeds the index is wrong at the point of use, or an array read/write
earlier in the same method (or a caller) desynchronized the counter from the
array's actual contents.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName jdtparser `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.jasper.compiler.TestCompiler — testBug53257f
```
Baseline confirming HotSpot passes: `apps/tomcat/.suite/results/overnight0629c/hotspot-jit/results.csv`.
Full run: `apps/tomcat/.suite/results/osr600verify/real-jit/results.csv`.

## Recommendation

Try `--nojit` / `CRATONVM_DISABLE_JIT=1` first to see whether this is
JIT-codegen-specific (a miscompiled array-index computation or corrupted
counter field in `Parser`) or reproduces in the interpreter too (pointing at a
more general field-read/array-store bug). If JIT-specific, `CRATONVM_DBG_JITC`
or bisecting `CRATONVM_JIT_BISECT_SKIP` against `Parser.consumeRule` /
`Parser.consumeTypeImportOnDemandDeclarationName` would isolate which compiled
method produces the bad index, following the same methodology already used
for the OSR regression cluster
([[jit-osr-backedge-value-corruption-cluster]]).
