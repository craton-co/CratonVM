# Proposal: the native `Lookup.find*` gate asks the lookup class, not only the mode bits

**Status: proposal, filed 2026-10-04 by interpreter round i1 wave 40, lane L4.
Not implemented.**

## What exists

The `Lookup` finders (`findGetter`, `findVirtual`, `findStatic`, ...) are
`Bridge`s that shadow the JDK's `Lookup` bytecode in both modes
(`register_p63_method_handles_lookup`, `native-builtins/src/lang_invoke.rs`).
Their access gate, `lk_enforce_find_access`, is **mode bits only**, by its own
documentation: a Lookup with `PRIVATE` in its modes (every
`MethodHandles.lookup()`, `0x5F`) is admitted to every member of every class
before the member is looked at, "deliberately, because our `lookupClass` comes
from a stack walk and a wrong answer there must never turn into a refusal".

So `MethodHandles.lookup().findGetter(Other.class, "secret", String.class)`
from a class that is not `Other`'s nestmate answers a working getter of
`Other`'s private field on CratonVM, where HotSpot throws
`IllegalAccessException: member is private: Other.secret/java.lang.String/getField,
from class Caller (...)`. The same admission is why `ldc` of a
`CONSTANT_MethodHandle` naming another class's private member was admitted
until wave 40 (fixed there in the `ldc` path only, by asking JVMS §5.4.4 from
the class that holds the constant, which is known exactly:
`interpreter::constants::method_handle_constant_access_refusal`).

## Proposal

1. **Know when the lookup class is exact.** The `MethodHandles.lookup()`
   native already walks the stack for the caller. Record, per minted
   `Lookup`, whether that walk was certain (the innermost Java frame is the
   caller, no reflective or MH frame in between) — or mint the `Lookup` from
   the interpreter door that executes the `invokestatic` of
   `MethodHandles.lookup()`, where the calling frame's class IS known. A
   `Lookup` from `privateLookupIn`, `in`, `dropLookupMode` carries its own
   class explicitly.
2. **Under `--jdk-only`, judge private members by the JVMS §5.4.4 nest rule**
   for a Lookup whose class is exact: reuse
   `interpreter::field_access::field_handle_access_refusal` /
   `method_member_access_refusal` (the predicates the `ldc` path and the
   `getfield` / `invoke*` checks already share), PRIVATE members only first
   (protected members need the JDK's receiver restriction, not a refusal).
3. **Census before enforcement.** A counter of "would refuse" under the
   existing `CRATONVM_DBG=access` census over the suite, the jdk-only corpus
   and a Spring Boot fat jar; enforce only when it reads zero on real code
   (frameworks use `privateLookupIn` for foreign privates, which carries the
   target class and stays admitted).

## Why it is worth doing

It removes the last place where a full-power Lookup is a universal key, and
it makes `Lookup.find*` and `ldc` of the same member answer alike (today
`ldc` refuses under `--jdk-only` and `findGetter` admits). The cost is one
predicate call per `find*` of a non-public member of another class, off the
invoke path.

## Risk

A wrong lookup class turns into a refusal of working code, which is exactly
the fear the gate's comment records; step 1 exists to make that impossible
rather than unlikely, and step 3 to measure it.
