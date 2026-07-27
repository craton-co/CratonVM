# LICM pre-header vs. exception-handler entry — dormant witness

These files witness one edge of the JIT's speculative pre-header placement
contract: `pc_to_native[loop_header]` deliberately points PAST the pre-header
(LICM hoists, speculative-BCE range guards, SIMD batch pre-headers), so anything
that enters the loop body without falling through the header runs it against an
uninitialised hoist slot or an unrun guard.

Explicit branch edges into a loop header are handled — see
`find_bypassable_loop_headers` in `jit/src/x64.rs`. Exception-handler edges are
not bytecode branches, so the same function handles them from the method's
exception table, using the same rule: an entry whose source lies outside the
loop. A `try`/`catch` wholly inside the loop keeps its hoist, because the throw
can only have happened after the header was entered.

**Status: the handler clause cannot fire today.** A method with a non-empty
exception table is not admitted to this backend, so the exception ranges handed
to that function are always empty in production. Measured on dev `d134f7104`
with `CRATONVM_DBG_DUMP_JIT=LIST`:

| probe | exception table | compiled |
|---|---|---|
| `LicmEntryProbe.shapeA/shapeB` | none | yes |
| `TryCatchHot.f` — try/catch in a hot loop, 200 000 calls | yes | **no** |
| `HandlerLoopProbe.shape` — 400 000 calls | yes | **no** |

Re-run these the day that admission gate is relaxed; the guard is already in
place, and this is the direct check that it works.

## Files

| file | what it is |
|---|---|
| `GenHandlerLoop.java` | ASM generator emitting `HandlerLoopProbe.class` — a shape javac cannot express: handler PC inside the loop body, protected range entirely before the loop header, and the normal path *falls through* into the header so no branch edge exists for the explicit-edge rule to catch. Uses an implicit divide-by-zero because an explicit `athrow` is separately barred from compilation. |
| `HandlerLoopDriver.java` | Alternates the two entries 200 000 times. Both set `max = 25` before the loop, so both must return 50. |
| `TryCatchHot.java` | Plain-Java control showing an ordinary try/catch-in-a-loop method is not compiled either. |

## Running

```bash
ASM=<asm-9.x.jar>
JH=<jdk25>
$JH/bin/javac -cp $ASM -d . GenHandlerLoop.java
$JH/bin/java  -cp $ASM:. GenHandlerLoop
$JH/bin/javac -cp classes -d classes HandlerLoopDriver.java
$JH/bin/java  -cp classes HandlerLoopDriver 200000
```

The generator writes `classes/HandlerLoopProbe.class`. Then run the same driver
under CratonVM:

```bash
<cratonvm> --java-home <jdk25> -cp classes HandlerLoopDriver 200000
```

Expected on both: `DONE iterations=200000 badNormal=0 badHandler=0`. A
`badHandler > 0` line means the pre-header was bypassed on the handler entry.
