# A compiled `invokeinterface` runs the superclass method instead of the override

**Status: OPEN (2026-08-11).** Localised to the dispatch between two compiled
methods, with five hypotheses refuted by measurement. Not root-caused.

> **Read the title change.** This page began as "a compiled `instanceof` answers
> false", because the failure text points there and the failing `else` branch is
> guarded by one. That was wrong, and every hypothesis built on it was refuted.
> The `instanceof` is never executed at all — the method containing it never
> runs. Kept visible because the wrong framing cost four probes.

## Symptom

`org.springframework.context.aot.ApplicationContextAotGeneratorTests` — 7 of 40
fail (at least 5 of them this; KRun displays only the first 5 per class):

```
java.lang.RuntimeException: java.lang.IllegalArgumentException:
    org.springframework.core.test.tools.DynamicClassFileObject
  at com.sun.tools.javac.file.JavacFileManager.inferBinaryName(JavacFileManager.java:809)
  at javax.tools.ForwardingJavaFileManager.inferBinaryName(ForwardingJavaFileManager.java)
  at com.sun.tools.javac.api.ClientCodeWrapper$WrappedJavaFileManager.inferBinaryName(...)
  at com.sun.tools.javac.code.ClassFinder.includeClassFile(ClassFinder.java)
```

`ClientCodeWrapper$WrappedJavaFileManager.inferBinaryName` calls
`clientJavaFileManager.inferBinaryName(...)`, where the field is declared as the
INTERFACE `javax.tools.JavaFileManager` — an **`invokeinterface`**. The receiver
is Spring's `DynamicJavaFileManager`, which extends `ForwardingJavaFileManager`
and **overrides** `inferBinaryName`
(`spring-core-test/.../DynamicJavaFileManager.java:118`):

```java
public String inferBinaryName(Location location, JavaFileObject file) {
    if (file instanceof DynamicClassFileObject dynamicClassFileObject) {
        return dynamicClassFileObject.getClassName();
    }
    return super.inferBinaryName(location, file);
}
```

The stack goes straight from the wrapper to `ForwardingJavaFileManager` — the
SUPERCLASS body — with no `DynamicJavaFileManager` frame between them. That is
not a lost frame. **The override never ran**, so `super.inferBinaryName` was
reached without the `instanceof` ever being evaluated, and `JavacFileManager`
threw on a file object it did not create.

## Established

| # | Probe | Result |
|---|---|---|
| 1 | `--jit off` | **passes 40/40** — a JIT defect |
| 2 | `CRATONVM_JIT_DENY=DynamicJavaFileManager.inferBinaryName` (the CALLEE) | **passes 40/40** |
| 3 | `CRATONVM_JIT_DENY=WrappedJavaFileManager.inferBinaryName` (the CALLER) | **passes 40/40** |

**Probes 2 and 3 together are the finding.** Denying *either* side fixes it,
which is the signature of a bad binding BETWEEN them, not a bad body in either:

* deny the caller → the call is dispatched interpreted → correct target;
* deny the callee → there is no compiled body for the caller's dispatch to bind
  to → correct target.

Both compiled bodies are produced by the **optimizing (IR) pipeline**
(`CRATONVM_DBG_IR_COMPILES=1`: "admitted to the optimizing pipeline" /
"optimizing backend produced a body" for `DynamicJavaFileManager`,
`ForwardingJavaFileManager` AND `ClientCodeWrapper$WrappedJavaFileManager`), and
the caller's plan reports `gates(static=true special=true virtual=true)
call_eligible=true`.

So: a compiled `invokeinterface` resolved to `ForwardingJavaFileManager.inferBinaryName`
— a *valid* implementation of the interface method, and the one the receiver
would inherit if it did not override — instead of the receiver's own override.

## Refuted — do not repeat these

**Not a loader split.** `@CompileWithForkedClassLoader` is in the stack, so "two
copies of `DynamicClassFileObject`" was the first hypothesis.
`CRATONVM_DBG=dupclass CRATONVM_DBG_DUPCLASS_FILTER=DynamicClassFileObject` → **0
events**, and the typecheck trace shows a single class id (3247) for the name.

**Not the runtime type-check helper.** `CRATONVM_DBG=typecheck-filter` (added on
this branch) over **637,004** traced decisions:

```
97220x  REFUSED  recv=PathFileObject$SimpleFileObject     target=DynamicClassFileObject(3247)
61172x  REFUSED  recv=PathFileObject$DirectoryFileObject  target=DynamicClassFileObject(3247)
53933x  REFUSED  recv=PathFileObject$JarFileObject        target=DynamicClassFileObject(3247)
    2x  true     recv=DynamicClassFileObject(3247)        target=DynamicClassFileObject(3247)
```

Every refusal is a receiver that genuinely is not the target, and every receiver
that IS one answers true. In hindsight this was the clue: only **2** such
receivers were ever seen, because in the failing runs the method holding the
check never executed.

**Not a compiled check with a missing target name.** `bytecode_walk`'s
`0xc0`/`0xc1` emission falls back to `(pc, ptr::null(), 0)` when the pc is absent
from `typecheck_info_idx`, and `jit_instanceof` answers `0` for a null name
*before* reaching `jit_typecheck_resolve` — a real "compiles to constant false"
shape. Instrumented that exact branch: **0 hits**.

**Not an inline type-check fast path.** There isn't one. Both lowerings call the
helper: the IR path emits `Op::InstanceOf` (and *bails the method* when the pc is
absent from `instanceof_info`), and single-pass emits an unconditional
`emit_call_absolute(self.helpers.instanceof_check)`.

**Not LICM.** `CRATONVM_JIT_LICM` is opt-in and unset; `licm.rs`'s `0xc1` arm is
a stack-depth model, not a hoist.

## Where to look next

The caller's compiled `invokeinterface` binding. `[ir] invoke-plan` shows the
caller lowering its calls through the IR call path with `virtual=true`, so start
at the IR invoke lowering and its dispatch cache / direct-entry selection, and
ask whether the target is chosen from the RECEIVER's class or from the
*resolved declaring* class of the interface method. `CRATONVM_DBG=jit-stale-ic`
and `jit-mic` are the relevant existing instruments, and
`reference_inline_cache_never_publishes_so_jit_taxes_call_dense_code` is the
neighbourhood.

The shape to check first: an interface method implemented by a superclass and
overridden by the receiver's class. Probes 2 and 3 give a known-good/known-bad
pair to A/B any candidate fix against, and this class is a 6-minute run.

## Levers

```bash
CRATONVM_JIT_DENY=DynamicJavaFileManager.inferBinaryName    # known-good (callee)
CRATONVM_JIT_DENY=WrappedJavaFileManager.inferBinaryName    # known-good (caller)
CRATONVM_DBG_IR_COMPILES=1 CRATONVM_DBG_JIT_COMPILED=1      # which path compiled what
CRATONVM_DBG_TYPECHECK_FILTER=<substr>                      # names the deciding typecheck branch
```

`CRATONVM_DBG_TYPECHECK_FILTER` is extremely loud — 637k lines and a 161 MB
stderr for one class. Filter narrowly and delete the run's `.err` afterwards.
