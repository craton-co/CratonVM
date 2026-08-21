# FIXED — a self-recursive activation could not catch its own callee's throw, and it cost two Groovy classes

## Status
**FIXED 2026-08-20** on `fix/groovy-selftailcall-20260820`. Filed the same day
against `dev` tip `eadd845c4`, where `GroovyMarkupViewTests` (7/10) and
`ViewResolutionIntegrationTests` (6/7) failed with the default flags and passed
with `CRATONVM_JIT_SELF_TAILCALL=1` — on pristine `dev` and on any branch merged
with it, binary for binary.

```text
                                    dev tip eadd845c4   with this fix
  GroovyMarkupViewTests                   7/10              10/10
  ViewResolutionIntegrationTests           6/7                7/7
  the 90 all-GC-variant Spring classes   85/90              87/90
```

The two still non-OK at 87/90 are unrelated and pre-existing:
`FileNativeConfigurationWriterTests` (HotSpot fails it identically) and
`BeanRegistrationsAotContributionTests` (the AOT/Mockito throughput wall).

Regression test: `test_jit_self_recursive_activation_catches_its_own_callee_throw`
in `vm/tests/jit_local_exception_handler_tests.rs`, over the new
`vm/tests/resources/cratonvm/JitSelfRecursiveHandler.java` fixture — it THROWS
before the fix rather than returning a wrong number. Standalone oracle:
`probes/SelfRecCatchProbe.java` + `.expected.txt`.

## The bug

A compiled frame never dispatches to its own exception handler. It returns the
`i64::MIN` sentinel and the interpreter's post-return drain resolves the
handler, using the bytecode pc that `jit_set_throw_bci` stamped — a compiled
frame stamps its own CALL SITE bci over whatever its callee left behind, so the
drain can range-check that pc against the frame's own exception table.

**That stamp is one `Cell<i64>` per thread and carries no activation identity.**
`jit_local_athrow_pc_kind`'s two guards — a real instruction boundary, inside
this method's code — are what keep a foreign method's bci from being honoured,
and they are satisfied *by construction* when the foreign frame is another
activation of the SAME method. In a self-recursive chain every activation writes
that one slot, the outermost writer wins, and the drain reads the outermost
call site while the throw happened somewhere deeper.

Measured on `probes/SelfRecCatchProbe.java` with `CRATONVM_DBG_RBC6=1`:

```text
classify hasUsableImplementation athrow_bci=19 ranges=[(10, 51)]   x5  -> caught
classify hasUsableImplementation athrow_bci=69 ranges=[(10, 51)]        -> PROPAGATE
```

bci 19 is the `Class.getMethod` call inside the `try`; bci 69 is the method's own
recursive call, outside it. The drain reads 69, concludes "this method cannot
catch this throw", and propagates — past a `catch (NoSuchMethodException)` that
covers the actual throw site.

`--nojit` and `CRATONVM_JIT_SELF_TAILCALL=1` both hide it for the same reason:
neither produces two compiled activations of the method. The elimination turns
the recursion into a `JMP` back to the method's own entry, so there is one frame
and one stamp. It never cured anything; `9fdc0a3f7` defaulting it off is what
made the defect reachable, and that commit is not at fault — the trade was
invisible.

## Why two Groovy classes, from that

`org.codehaus.groovy.reflection.stdclasses.CachedSAMClass.hasUsableImplementation`
is exactly this shape: it walks a superclass chain with a tail self-call, and
each activation wraps `Class.getMethod` in `catch (NoSuchMethodException)`. At
`java.lang.Object` the reflective lookup throws, the `catch` is skipped, and the
exception escapes `getSAMMethod` — so Groovy picks the wrong SAM method for a
Closure coercion. Two hops later that is a NULL receiver at a Groovy call site:

```text
invocation of method 'visitMethod'
  sender: MarkupTemplateTypeCheckingExtension$_run_closure6$_closure9
  argument[0] = null
INFO ... receiver is null
INFO ... binding null object receiver and dropping old receiver
INFO ... casting explicit from (Object,Object,String,Object[])Object to (Object,Object)Object
```

and the last line throws `WrongMethodTypeException` out of
`Selector.setCallSiteTarget`, surfacing as
`MultipleCompilationErrorsException: startup failed: General error during
canonicalization`. Groovy's own `-Dgroovy.indy.logging=true` is what made that
chain visible; the failing run has three `binding null object receiver` blocks
and the passing run has none.

**The 2026-08-19 filing's guess was wrong, and worth recording as such.** It read
the `WrongMethodTypeException` as an adapter-arity defect in the same family as
the `MethodHandles.collectArguments` bug fixed that day, and nominated the
`asType` in-place aliasing (G31-1) as the first thing to rule out. Neither is
involved. The arity message is a SECOND CratonVM weakness — Groovy's
null-receiver path really does build a handle it cannot cast — reached only
because a JIT miscompile put a null there. It stays unfixed and unreachable
again now; if a future null receiver reaches that path it will need its own
look.

## The fix

Two routes read that stamp, and both needed the same guard
(`vm/src/runtime/interpreter/exception_dispatch.rs`):

* `route_jit_signal_exception` — the drain after a compiled frame returns.
* `run_jit_callee_handler` — the JIT-to-JIT sibling, reached from
  `route_implicit_exc_through_callee`, which captures the stamp before clearing
  it and hands it in as "the callee's own throw site".

`stamp_is_an_ambiguous_self_call_site` reads the opcode at the stamped pc,
resolves its constant-pool method reference, and answers whether it names a call
to the method being drained. When it does, the verdict is downgraded from
"outside every protected range" (a definite *cannot catch*) to the `usize::MAX`
pc-unknown search the caller already runs for any stamp it cannot read.

That downgrade is deliberately weaker than a fix that would name the right
activation. It says "cannot tell", not "caught here" — and the pc-unknown search
is itself conservative (it skips a catch-all whose region does not span the whole
method, i.e. every javac `finally`), so it cannot swallow anything the unknown-pc
path would not already have swallowed. The predicate fails closed: an unreadable
opcode, a malformed constant-pool entry or a missing class all leave the
propagate exactly as it was.

Blast radius is confined to methods that call themselves. Every witness the
surrounding machinery was built against — `FinallyBalanceProbe`,
`AthrowCountBisect.twoThrowsSequential`, bc-java's `CipherInputStream.nextChunk`
and `SymmetricConstraintsTest` — is a non-self-recursive shape and is untouched
by construction; `vm/tests/jit_local_exception_handler_tests.rs` (20 tests) is
green.

## What this does NOT fix

The stamp still cannot name WHICH activation threw, so the handler is resumed in
the outermost activation's frame, rebuilt from its `this` + declared params —
the documented approximation `route_jit_exception_through_method` has always
made. For `hasUsableImplementation` that re-runs the walk from an outer class and
converges on the right answer; for a method whose prefix is not idempotent it
would not. Naming the activation needs the stamp to carry a frame identity, which
is an ABI change to `jit_set_throw_bci` and its call sites and wants its own task.

## Repro

```bash
cd probes && javac -d /tmp/srp SelfRecCatchProbe.java
<cratonvm-bin> --java-home "$JAVA_HOME" -cp /tmp/srp SelfRecCatchProbe
$JAVA_HOME/bin/java -cp /tmp/srp SelfRecCatchProbe    # the oracle
```

Before the fix CratonVM exits 1 with
`NoSuchMethodException: java.lang.Object.run(java.lang.String)` escaping a
`catch` that covers it; after, it prints the oracle's six rows and
`TRUECOUNT=4`. Add `CRATONVM_DBG_RBC6=1` for the routing decisions.

Suite level:

```bash
cd apps/spring-suite-runner
SPRING=/data/cratonvm/apps/spring-framework JDK25="$JAVA_HOME" \
CRATONVM_BIN=<cratonvm-bin> ./one.sh \
  org.springframework.web.servlet.view.groovy.GroovyMarkupViewTests
```
