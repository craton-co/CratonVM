# Single-pass codegen refuses a handler body with an internal branch

**Status:** 🔴 **OPEN**, found 2026-08-02 while validating
[RBC.6's `getfield`/`putfield` admission](../../internal/rbc6-protected-field-ops-FIXED-20260802.md).

Pre-existing, not caused by that change — RBC.6 was refusing these methods
before codegen ever ran, so the hole had nothing to be seen through. Effect is
benign (the method is bail-listed and stays interpreted), but it is a real
backend limitation and it now costs compiles that RBC.6 no longer blocks.

## Repro

`probes/Rbc6FieldProbe.java`, method `getfieldRefHandlerLocal`:

```
CRATONVM_DBG_JITC=1 cratonvm --java-home <jdk25> -cp <probes-out> Rbc6FieldProbe
```

```
[cratonvm-jitc] compile-bail Rbc6FieldProbe.getfieldRefHandlerLocal(LRbc6FieldProbe$Holder;I)I
                backend_attempted=true reason=singlepass-codegen(pc=36,op=0xac)
```

Its four siblings in the same class — including the `long` and `putfield`
variants — compile. The one structural difference is that this method's
HANDLER body contains a branch:

```
 23: astore        4          // handler entry
 25: iload_2
 26: aload_3
 27: ifnonnull     34
 30: iconst_0
 31: goto          35
 34: iconst_1
 35: iadd
 36: ireturn                  <-- refused here
```

so pc 35 is a merge point reached by two edges INSIDE a region that, in a
compiled frame, is unreachable (exceptions leave through the sentinel; handler
dispatch happens in the interpreter).

## What the reason string does and does not say

`reason=singlepass-codegen(pc=36,op=0xac)` reports `dbg_last_pc`/`dbg_last_op`,
i.e. the last bytecode the emitter touched. The `0xac..=0xb0` arm has no
`return false` of its own — it flushes, pops to RAX and emits the epilogue —
so the refusal is a `self.failed` flag raised elsewhere (the spill-range
checks in `checked_spill_range_end` are the likeliest source) and merely
*attributed* to the last opcode. Treat the pc/op as a locator, not a verdict;
that ambiguity is itself worth fixing, by having whatever sets `failed` record
its own site the way `note_jit_bail_site` does everywhere else.

## Why it matters now

The dead-code merge reconstruction that runs at pc 35 is the machinery whose
history is documented at `x64.rs`'s `branch_target_stack_oop_marks` — it
rebuilds an operand stack for a target that becomes live again. Handler bodies
are the population where that path is most exercised and least tested, and
RBC.6's `getfield`/`putfield` exclusion was keeping a large share of them out
of the backend entirely.

## Suggested first step

Reproduce with a minimal method: an exception handler whose body contains a
ternary (`return e == null ? 0 : 1;`) and nothing else, with a protected range
containing only admitted opcodes. Then instrument `self.failed = true` sites to
name themselves, and read which one fires.
