# `ResolvableType[]` array-cast `ClassCastException` — narrowed to `equals(Object)`, root cause not yet found (2026-07-27)

Continuation of `docs/known-issues/resolvabletype-array-cast-aggressive-jit-20260727.md`
(the original finding, from the SPB.4/4b/4c removal session). This entry
records substantial further narrowing via precise bisection; the exact
x86 miscompile mechanism inside the JIT-compiled `equals(Object)` was
**not** found — this is a handoff, not a fix.

## Recap of the symptom

```
CRATONVM_JIT_THRESHOLD=1 cratonvm --java-home <jdk25> -cp "$CP" S01_Context
# org/springframework/boot/context/config/Profiles.<clinit> throws:
# ClassCastException: class [Lorg.springframework.core.ResolvableType;
#   cannot be cast to class org.springframework.core.ResolvableType
```

Only reproduces under `CRATONVM_JIT_THRESHOLD=1` (forces eager/aggressive
compilation from the first invocation) — never at default JIT tiering,
because the affected code runs from a `<clinit>` (executes once) and
never naturally accumulates enough invocations to be JIT-tiered.
`Profiles.<clinit>` itself and `ResolvableType.forClassWithGenerics`/
`forClass` (the methods whose bytecode literally builds/consumes the
`ResolvableType[]` array) are confirmed via `CRATONVM_DBG_JIT_DISASM=1`
to **never get JIT-compiled at all** during this repro — they run purely
interpreted. So this is NOT a miscompile of the array-building bytecode
itself; it's corruption caused by a *different*, JIT-compiled method,
manifesting later when the interpreter reads the (by-then-corrupted)
object.

## Bisection (via `CRATONVM_JIT_DENY=<substring>`, matches `Class.method`
without descriptor — force-interpret matching methods)

`CRATONVM_JIT_THRESHOLD=1` + `CRATONVM_DBG_JIT_DISASM=1` shows exactly 10
`org/springframework/core/ResolvableType*` methods get JIT-compiled
during this repro:
`hashCode`, `calculateHashCode`, `resolve`, `equals`, `equalsType`,
`DefaultVariableResolver.getSource`, `forType` (2 overloads),
`SyntheticParameterizedType.equals`, `TypeVariablesVariableResolver.getSource`.

Bisection results (each row: env additions, result):

| `CRATONVM_JIT_DENY` value | Result |
|---|---|
| (none — baseline) | **FAILS** (10/11) |
| `org/springframework/core/ResolvableType` (whole class) | PASSES (15/15) |
| `ResolvableType.hashCode,...calculateHashCode,...resolve,...equals,...equalsType` (5 of 10) | PASSES (15/15) |
| `ResolvableType.resolve` (alone) | **FAILS** |
| `ResolvableType.hashCode,ResolvableType.calculateHashCode` (alone) | **FAILS** |
| `ResolvableType.equals` (alone — substring-matches BOTH `equals` and `equalsType`, since "equals" is a string-prefix of "equalsType" and the filter has no descriptor to disambiguate) | PASSES (15/15) |
| `ResolvableType.equalsType` (exact, longer string — does NOT match the shorter `equals`) | **FAILS** |

Conclusion: the outer, public **`ResolvableType.equals(Object)`** method
specifically is the culprit. Not `equalsType`, not `hashCode`/
`calculateHashCode`, not `resolve`, not either `forType` overload, not the
two tiny nested-class `getSource()`/`equals()` methods.

## `equals(Object)`'s own bytecode (real spring-core-7.0.7.jar, via javap)

```java
public boolean equals(Object other) {
    if (this == other) return true;
    if (other == null || other.getClass() != getClass()) return false;
    ResolvableType otherType = (ResolvableType) other;     // checkcast here
    if (!equalsType(otherType)) return false;
    ... typeProvider / variableResolver comparisons ...
    return true;
}
```

39 bytecode instructions, one `checkcast org/springframework/core/ResolvableType`
right after the `getClass()` identity check. Nothing unusual — a
completely standard `equals()` shape used all over the JVM; if a generic
"self-checkcast in equals()" bug existed it would be triggering constantly
elsewhere in the whole test suite, which it isn't. The bug must be
specific to *this* compilation, not the general pattern.

## The compiled x86 is suspiciously large (7436 bytes for ~40 bytecode
instructions)

`CRATONVM_DBG_JIT_DISASM='ResolvableType.equals'` dumps the real machine
code (see `docs/known-issues/repros/resolvabletype-equals-jit-20260727/equals_disasm.txt`
for the full dump). The first ~150 bytes are unremarkable
prologue/spill-save code, but starting around offset `0x1a8` the compiled
code loads a table pointer (`mov r10, 0x200123E4800`) and does a chain of
`cmp eax,[r10]` / `cmp eax,[r10+4]` / `cmp eax,[r10+8]` ... against
successive 4-byte slots, each followed by a conditional `call` through a
function pointer at `[r10+0x10]`/`[r10+0x18]`/etc. — this is the shape of
an **inlined polymorphic/megamorphic dispatch table** (a manually
unrolled inline-cache lookup), most likely for the `other.getClass()`
virtual call inside `equals()`. The method is JIT-compiled at
`CRATONVM_JIT_THRESHOLD=1` (eager, first-invocation compile) — at that
point the JIT has seen exactly one call so far, so it is unclear why a
multi-entry polymorphic table would already exist unless this reuses a
**pre-existing, cross-call-site polymorphic inline cache keyed by
(declaring class, method name) rather than by the individual call site**,
inheriting entries from *other* `equals()` calls elsewhere in the process
(other `ResolvableType.equals` invocations during Spring's own bootstrap,
e.g. via the static `cache: ConcurrentReferenceHashMap<ResolvableType,
ResolvableType>` field, which calls `.equals()`/`.hashCode()` on
`ResolvableType` keys constantly). If that polymorphic-table lookup ever
mis-selects the WRONG entry (or a stale one whose "known class" no longer
matches, e.g. after some other object got GC'd/reused at the same class
slot), the compiled code could branch into the wrong receiver-comparison
path and mis-cast an array reference it holds for an entirely unrelated,
previously-seen receiver — which would explain the specific "array cast
to element type" shape without there being any array-typed field
anywhere in `equals()`'s own bytecode.

**Update: the dispatch table IS identified** — it is exactly the
documented 4-way Polymorphic Inline Cache (PIC) codegen at
`jit/src/x64.rs` (the `CRIT-8`/`HIGH-7` comment block, ~line 26860-26940,
right before the `jit_invoke_virtual_mic` slow-path call). The layout
matches byte-for-byte:

```
CLASS_ID_OFFSETS      = [0, 4, 8, 12]     // matches cmp eax,[r10]/[r10+4]/[r10+8]
ENTRY_PTR_OFFSETS     = [16, 24, 32, 40]  // matches call qword [r10+0x10]/[r10+0x18]/[r10+0x20]
NEEDS_CONTEXT_OFFSETS = [48, 49, 50, 51]  // matches cmp byte [r10+0x30]/[r10+0x31]/...
```

This is the compiled cascade for one of `equals()`'s two `invokevirtual`-
shaped calls (`other.getClass()`, or the `invokevirtual equalsType`
at bytecode offset 31 — not disambiguated yet, both are plausible; the
receiver register (`rsi`/`[rbp-38h]` in the dump) was not traced back to
a specific bytecode-level local). This is well-established, carefully
documented, heavily-used machinery (present at essentially every
polymorphic virtual call site in the VM) — a naive bug in the mechanism
itself would very likely already manifest constantly elsewhere in this
huge test suite, so the defect is probably not in the PIC codegen
template itself but in something specific to THIS call site's
compilation (e.g., stale/cross-contaminated slot state from an unrelated
call site sharing a `pic_ptr`/`mic_ptr` allocation, or a `needs_context`/
ABI mismatch for this particular receiver shape). **Not confirmed** — the
full x86 was not manually traced instruction-by-instruction to the actual
faulting `checkcast`'s implementation, and the `mic_ptr`/`pic_ptr`
allocation-and-slot-population code (upstream of this consumption site)
was not inspected at all this session.

Next steps for whoever picks this up:

1. Read the `mic_ptr`/`pic_ptr` allocation and slot-population logic
   (search `jit/src/x64.rs` for `mic_slots`, `pic_ptr`,
   `JitMICSlot`/`JitPICSlot`, `jit_invoke_virtual_mic`) to see whether
   slots are keyed strictly per-(bytecode-offset, compiled-method) or
   could be shared/reused across call sites in a way that lets one call
   site's PIC state leak into another's.
2. Add an assertion/trace at the actual `checkcast` failure site (native
   runtime error path for `ClassCastException`) that dumps the actual
   object's class-id and the expected class-id, to see exactly which PIC
   slot/entry was consulted when this specific call failed.
3. Consider whether this is the SAME family as the already-fixed
   `jit-virtual-dispatch-bail-static-class-bug` (receiver-class-based
   dispatch defect in the bail-to-interpreter path specifically) but in
   the PIC hot-path itself rather than its miss/bail path.
4. Disassemble `ResolvableType.equalsType` too (also JIT-compiled in this
   repro, 1125 bytes) for comparison — if IT has a similar PIC shape but
   denying it alone does NOT reproduce the bug (confirmed this session),
   that may help distinguish "PIC mechanism bug" from "something specific
   to equals()'s particular call shape/inlining decisions".

## Reachability caveat (unchanged from the original finding)

Only reachable via `CRATONVM_JIT_THRESHOLD=1` (a debug/testing knob that
forces eager compilation from the first invocation) — a real workload's
default JIT tiering would essentially never compile `equals()` on a
`ResolvableType` cache key this early/aggressively in a way that
reproduces this exact interaction, and `Profiles.<clinit>` itself (the
class that actually crashes) never gets JIT-compiled at all. Low priority
relative to default-threshold-reachable bugs, but a genuine, precisely
localized, reproducible correctness defect worth fixing eventually.

## Repro commands (for the next session)

```bash
CP=/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite/classes:$(ls /data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite/lib/*.jar | tr '\n' ':')

# Reproduce:
CRATONVM_JIT_THRESHOLD=1 cratonvm --java-home <jdk25> -cp "$CP" S01_Context

# Confirm it's specifically equals(), not equalsType/hashCode/etc:
CRATONVM_JIT_THRESHOLD=1 CRATONVM_JIT_DENY='ResolvableType.equals' cratonvm --java-home <jdk25> -cp "$CP" S01_Context   # PASSES (15/15)
CRATONVM_JIT_THRESHOLD=1 CRATONVM_JIT_DENY='ResolvableType.equalsType' cratonvm --java-home <jdk25> -cp "$CP" S01_Context  # still FAILS

# Get the real compiled machine code:
CRATONVM_JIT_THRESHOLD=1 CRATONVM_DBG_JIT_DISASM='ResolvableType.equals' cratonvm --java-home <jdk25> -cp "$CP" S01_Context
```
