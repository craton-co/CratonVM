# FIXED: a MIC entry compiled BY NAME can be another loader's copy of the method

**Status: FIXED 2026-08-11.** `vm/src/jit/helpers.rs` — gate the by-name callee
compile on `globally_named`, in both the entryless-hit and cache-miss arms.

## The defect

`jit_invoke_virtual_mic` resolves its callee with

```rust
try_jit_compile_callee(vm, &class_name, info.method_name, info.descriptor, true)
```

where `class_name` is the **receiver's** class name — correct as far as it goes,
and there is a long-standing "VIRTUAL DISPATCH FIX" comment explaining why it
must not be the static call-site type. But the resolution is still **by name**.
When the receiver's class is not the class that name globally resolves to, this
hands back **another loader's copy** of the method, and the resulting entry is
published into the MIC keyed on **this** receiver's class id. Every later
monomorphic hit then machine-CALLs a body compiled for a different copy.

`globally_named` (`cm.get_loaded_class_id(&recv_name) == Some(cid)`) already
detects exactly this, and already gates `publish_mic_rust_cached_entry` two arms
below. The by-name compile feeding the machine-code MIC/PIC was left ungated.

## How it surfaced, and why it looked like something else

`ApplicationContextAotGeneratorTests`, 7 of 40:

```
IllegalArgumentException: org.springframework.core.test.tools.DynamicClassFileObject
  at com.sun.tools.javac.file.JavacFileManager.inferBinaryName(JavacFileManager.java:809)
  at javax.tools.ForwardingJavaFileManager.inferBinaryName(...)
  at ClientCodeWrapper$WrappedJavaFileManager.inferBinaryName(...)
```

Spring's `DynamicJavaFileManager.inferBinaryName` guards its fast path with
`file instanceof DynamicClassFileObject` and otherwise calls `super.`. Each
`@CompileWithForkedClassLoader` test defines its **own** copy of these classes,
so one run resolved `DynamicJavaFileManager` to **eight-plus distinct class ids**
(2690, 8510, 10377, 14030, 15877, 17724, 19569, 21414, …). The compiled body the
MIC bound belonged to a different copy, and its `instanceof` site is interned
against **that** copy's `DynamicClassFileObject` id — so the check *correctly*
answered false for this copy's file object, `super.` ran, and `JavacFileManager`
threw on a file object it had not created.

The JDK's message is `file.getClass().getName()`, so it prints
`DynamicClassFileObject` and reads like a broken type check. It is not one.

## The measurement that found it

`CRATONVM_DBG_MIC_METHOD=inferBinaryName` (added with this fix — the `[cv-mic-*]`
trace was previously hardcoded to one investigation's `get(Class)Object` site):

```
106054  [cv-mic-miss] resolved_class=ClientCodeWrapper$WrappedJavaFileManager recv_cid=2698
     1  [cv-mic-miss] resolved_class=DynamicJavaFileManager recv_cid=8510
     1  [cv-mic-miss] resolved_class=DynamicJavaFileManager recv_cid=2690
     1  [cv-mic-miss] resolved_class=DynamicJavaFileManager recv_cid=21414
     ...
```

One name, many ids, one call site. That single histogram is the whole diagnosis.

## Four refutations first — do not repeat them

The failure text points at `instanceof`, and four probes died on that framing
before the MIC histogram was taken:

* **Not a loader split *of `DynamicClassFileObject`*** —
  `CRATONVM_DBG_DUPCLASS_FILTER=DynamicClassFileObject` → 0 events. (The split is
  real, but it is of the *file manager*, and this instrument did not see it;
  treat 0 dupclass events as "this instrument saw nothing", not "one copy".)
* **Not the type-check helper** — `CRATONVM_DBG=typecheck-filter` over 637,004
  decisions: refuses only receivers that genuinely are not the target, accepts
  every one that is. It is right wherever it is asked. In hindsight the clue was
  that it saw a `DynamicClassFileObject` receiver only **twice**.
* **Not a compiled check with a null target name** — `bytecode_walk` falls back
  to `(pc, ptr::null(), 0)` and `jit_instanceof` answers 0 for that before
  reaching `jit_typecheck_resolve`. Instrumented that exact branch: 0 hits.
* **Not an inline type-check fast path** — there is none. Both lowerings call the
  helper.

The probe that actually localised it was the **pair** of deny runs:
`CRATONVM_JIT_DENY` on the callee AND on the caller each fix it, which is the
signature of a bad binding *between* two compiled bodies rather than a bad body
in either.

## Verification

| | before | after |
|---|---|---|
| `ApplicationContextAotGeneratorTests` | 7/40 fail | **40/40 OK** |
| `RestTemplateIntegrationTests` | 1/125 fail | **125/125 OK** |
| behavioural regression, 47 classes (the XML cluster, all `org.springframework.jmx.*`, and the residual set) | — | **42 OK**, 5 FAIL — exactly the pre-existing unrelated ones |
| `cargo test -p cratonvm-jit --lib` | — | **1972 passed, 0 failed** |

The fix is conservative in the failing direction: when the receiver's class is
not globally named, the site simply stays on the dispatch helper, which resolves
on the real receiver. It gives up a fast path in the multi-copy case; it does not
change single-copy behaviour, which is every ordinary process.

## Levers

```bash
CRATONVM_DBG_MIC_METHOD=<substring>     # retarget the [cv-mic-*] trace
CRATONVM_JIT_DENY=<Class>.<method>      # force-interpret one method
CRATONVM_DBG_TYPECHECK_FILTER=<substr>  # name the deciding typecheck branch (very loud)
CRATONVM_DBG_IR_COMPILES=1              # which pipeline compiled what
```
