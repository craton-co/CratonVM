# HIB-DEV-04 — `Throwable.getStackTrace()` / `printStackTrace()` return frames in **reversed** order

**Severity:** Medium — every exception stack trace is upside-down (`main` at index 0, throw site last). Breaks any test/code that inspects `e.getStackTrace()[0]` or reads `printStackTrace()` output, and impedes all exception diagnosis.
**Status:** ✅ **FIXED** (this run, `../../../../native-builtins/src/lang_misc.rs`).
**Mode:** Interpreter (JIT-off); order-only, JIT-independent.
**HotSpot (JDK 25):** correct (index 0 = throw site).

## Symptom

```java
static int len(){ return s.length(); }   // NPE thrown here (s == null)
public static void main(String[] a){ try { len(); } catch (NullPointerException e) {
    for (StackTraceElement f : e.getStackTrace()) System.out.println(f); } }
```

| | `getStackTrace()` order |
|---|---|
| HotSpot | `NpeStack.len(NpeStack.java:3)` then `NpeStack.main(NpeStack.java:5)` |
| CratonVM (before) | `NpeStack.main(...)` then `NpeStack.len(...)` ← **reversed** |

Per `java.lang.Throwable.getStackTrace()`: "the zeroth element … represents the top of the stack, which is the **last method invocation** in the sequence" — i.e. the throw site. CratonVM returned it bottom-first.

## Root cause

CratonVM's internal `capture_stack_trace` deliberately stores frames **outermost-first** (`main` first) — and that order is correct for, and relied upon by, the `StackWalker` / `Reflection.getCallerClass` / `lang_invoke` consumers (documented in those call sites). The bug was that the **user-facing `Throwable` → `StackTraceElement[]` conversion** copied that order verbatim instead of reversing it. Four sites in `lang_misc.rs` were affected:

- `native_init_stack_trace_elements` (fills the `StackTraceElement[]` for `getOurStackTrace`)
- the `getStackTrace()` array builder
- `native_throwable_get_stack_trace_element(index)` (indexed access)
- `throwable_frame_lines` (the `printStackTrace()` `\tat …` lines)

## Fix

Reverse **only** at the four user-facing throwable conversion sites (`.iter().rev()` / reversed index), leaving `capture_stack_trace` outermost-first so the StackWalker/caller-class consumers are unaffected. After the fix `getStackTrace()[0]` is the throw site, matching HotSpot.

## Impact

Corrects all exception diagnostics (a prerequisite for debugging the other FAILs — e.g. the serialization-NPE cluster's stack was unreadable upside-down), and any test asserting on stack-trace order/origin. Standalone repro: `jsonrepro/NpeStack.java`.
