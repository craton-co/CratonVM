# A compiled `instanceof` answers false where the helper it would have called answers true

**Status: OPEN (2026-08-11).** Localised to one compiled method, with three
hypotheses refuted by measurement. Not root-caused; the remaining suspect is a
type-check fast path in generated code that never calls the runtime helper.

## Symptom

`org.springframework.context.aot.ApplicationContextAotGeneratorTests` — 7 of 40
fail (at least 5 of them this; KRun displays only the first 5 failures per
class):

```
java.lang.RuntimeException: java.lang.IllegalArgumentException:
    org.springframework.core.test.tools.DynamicClassFileObject
  at com.sun.tools.javac.file.JavacFileManager.inferBinaryName(JavacFileManager.java:809)
  at javax.tools.ForwardingJavaFileManager.inferBinaryName(ForwardingJavaFileManager.java)
  at com.sun.tools.javac.api.ClientCodeWrapper$WrappedJavaFileManager.inferBinaryName(...)
  at com.sun.tools.javac.code.ClassFinder.includeClassFile(ClassFinder.java)
```

`JavacFileManager.inferBinaryName` throws `IllegalArgumentException(file.getClass().getName())`
for anything that is not a `PathFileObject`. **The JDK's own message is the
receiver's class name, and it says `DynamicClassFileObject`** — so the object
that reached the `super.` call really was one.

It reached `super.` because Spring's override took its else branch
(`spring-core-test/src/main/java/org/springframework/core/test/tools/DynamicJavaFileManager.java:118`):

```java
public String inferBinaryName(Location location, JavaFileObject file) {
    if (file instanceof DynamicClassFileObject dynamicClassFileObject) {
        return dynamicClassFileObject.getClassName();
    }
    return super.inferBinaryName(location, file);   // <- taken, wrongly
}
```

So a compiled `instanceof DynamicClassFileObject` answered **false for an object
whose `getClass().getName()` is exactly that**.

## Established

| # | Probe | Result |
|---|---|---|
| 1 | `--jit off` | **passes 40/40** — it is a JIT defect |
| 2 | `CRATONVM_JIT_DENY=DynamicJavaFileManager.inferBinaryName` | **passes 40/40** — the miscompile is in THAT compiled body, not in a caller or callee |

Probe 2 is the sharp one: denying just that one method is enough, and the only
decision in it is the `instanceof`.

## Refuted — do not repeat these

**It is not a loader split.** `@CompileWithForkedClassLoader` is in the stack and
this looks exactly like the family in
`fixed-suite-bugs/spring/gc-variant-fullsuite-classpath-gap-and-fails-20260810-FIXED.md` (§3, in the internal docs tree) and the `invokestatic`-bound-by-NAME
fix of 2026-08-10, so "two copies of `DynamicClassFileObject`, `instanceof`
against the wrong one" was the first hypothesis. It is wrong:

```
CRATONVM_DBG=dupclass CRATONVM_DBG_DUPCLASS_FILTER=DynamicClassFileObject
  -> 0 events
```

**It is not the runtime type-check helper.** Added
`CRATONVM_DBG=typecheck-filter=<substring>` (this branch) to name the branch that
decides every compiled `checkcast`/`instanceof` against a matching target. Over
637,004 traced decisions in one run:

```
97220x  REFUSED-by-recorded-site  recv=PathFileObject$SimpleFileObject      target=DynamicClassFileObject(3247)
61172x  REFUSED-by-recorded-site  recv=PathFileObject$DirectoryFileObject   target=DynamicClassFileObject(3247)
53933x  REFUSED-by-recorded-site  recv=PathFileObject$JarFileObject         target=DynamicClassFileObject(3247)
    2x  site-recorded             recv=DynamicClassFileObject(3247)         target=DynamicClassFileObject(3247)  -> true
```

Every refusal is a receiver that genuinely is not a `DynamicClassFileObject`, and
**every time the receiver IS one, the helper answers true**. One class id (3247),
one name — corroborating the dupclass result independently. The helper is right
every time it is asked.

That is the whole finding: *the predicate is correct wherever it is consulted, so
the wrong answer is produced where it is NOT consulted.* Same shape as
`a_warmed_dispatch_memo_must_re_ask_a_decision_whose_inputs_move`.

**It is not the branchy IR arm.** `CRATONVM_NO_IR_BRANCHY=1` still fails 7/40.

## Where to look next

`jit/src/lib.rs` builds per-PC inline type-check maps and installs them with
`builder.set_checkcast_info(cc_im)` / `set_instanceof_info(io_im)` (~line 15259).
That is generated code deciding a type check inline, and it is the only path that
can answer without reaching `jit_typecheck_resolve` — which is exactly what the
trace above requires of the culprit. `-ir-branchy` did not disable it, so either
the fast path lives on the other lowering path too, or that flag does not gate
what its name suggests; establish which before assuming either.

A useful next probe is to make the inline fast path *also* emit a trace (or to
force it off entirely for one build) and re-run probe 2's method — the deny lever
already gives a known-good/known-bad pair to compare against.

## Levers

```bash
CRATONVM_JIT_DENY=DynamicJavaFileManager.inferBinaryName   # known-good
CRATONVM_DBG_TYPECHECK_FILTER=DynamicClassFileObject       # names the deciding branch
CRATONVM_DBG=dupclass CRATONVM_DBG_DUPCLASS_FILTER=...     # two-copies check
```

Note `CRATONVM_DBG_TYPECHECK_FILTER` is extremely loud — 637k lines and a 161 MB
stderr for a single class. Filter narrowly and delete the `.err` afterwards.
