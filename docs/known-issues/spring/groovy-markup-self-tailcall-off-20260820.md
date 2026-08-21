# Groovy markup templates fail to compile when self-tail-call elimination is OFF — 2 Spring classes, one env var away from green

## Status
**OPEN, CratonVM-specific, reproduced on pristine `dev` tip** — found
2026-08-20 while verifying an unrelated Spring Framework fix against the merged
tree. `dev` commit `9fdc0a3f7` ("default the self-tail-call elimination OFF, and
retire the last two jit known-issues") flipped
`CRATONVM_JIT_SELF_TAILCALL`'s default from on to off. Two Spring Framework
classes fail with that default and pass without it, on `dev` tip and on any
branch merged with it alike:

```text
                                                     default   SELF_TAILCALL=1
  dev tip eadd845c4
    GroovyMarkupViewTests                             7/10       10/10
    ViewResolutionIntegrationTests                    6/7         7/7
  fix/spring-chm-clusters-20260820 merged with it
    GroovyMarkupViewTests                             7/10       10/10
    ViewResolutionIntegrationTests                    6/7         7/7
```

Four runs per cell region, deterministic — not a flake. Both classes are green
on HotSpot. `--nojit` also makes both pass, so this is a JIT-compiled path.

This is NOT a regression introduced by the branch that found it: the same
binary-for-binary comparison above shows pristine `dev` tip failing identically.
It is recorded here because the flag flip that exposed it landed with a commit
message about closing two OTHER JIT known-issues, so the trade is easy to miss.

## Symptom

```text
GroovyMarkupViewTests :: renderI18nTemplate/renderLayoutTemplate/renderMarkupTemplate
ViewResolutionIntegrationTests :: groovyMarkup

org.codehaus.groovy.control.MultipleCompilationErrorsException: startup failed:
General error during canonicalization: cannot explicitly cast
  MethodHandle(Object,Object,String,Object[])Object to (Object,Object)Object

java.lang.invoke.WrongMethodTypeException: cannot explicitly cast
  MethodHandle(Object,Object,String,Object[])Object to (Object,Object)Object
    at org.codehaus.groovy.vmplugin.v8.Selector$MethodSelector
       .setCallSiteTarget(Selector.java:1068)
    at org.codehaus.groovy.vmplugin.v8.IndyInterface.fallback(IndyInterface.java:401)
    ...
    at groovy.text.markup.MarkupTemplateTypeCheckingExtension
       $_run_closure6$_closure9.doCall(MarkupTemplateTypeCheckingExtension.groovy:168)
```

`Selector.setCallSiteTarget`'s last step is
`MethodHandles.explicitCastArguments(handle, callSite.type())`, which refuses on
an ARITY mismatch alone. The handle it is holding still has the target's four
parameters where the call site has two, so the adapter chain Groovy built ahead
of it — `insertArguments` for the bound name, a collector for the trailing
`Object[]` — did not reduce the arity it was supposed to reduce.

## What is known, and what is not

Known:

* The failing operation is an adapter chain whose composed `type()` is wrong,
  in the same family as the `MethodHandles.collectArguments` defect fixed the
  same day (`collect_args_adapter_descriptor`, native-builtins) — an adapter
  that dispatches correctly and reports the wrong type is invisible until
  something reads the type back, and Groovy's `Selector`, like invokebinder's
  `Binder`, is built entirely on reading it back.
* It is JIT-path-dependent (`--nojit` passes) and gated by
  `CRATONVM_JIT_SELF_TAILCALL` alone. Neither
  `CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS=0`,
  `CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS=0`, nor
  `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=0` changes the result, so the
  other two levers `9fdc0a3f7`'s neighbours added are not involved.

Not known, and the next step:

* **Whether self-tail-call elimination CURES the defect or merely HIDES it.**
  A tail-call-eliminated frame is a different compiled shape, not a different
  MethodHandle, so the more likely reading is that it changes which of Groovy's
  `Selector` paths is reached or how far the fallback recurses. Nothing here
  distinguishes the two, and the distinction decides whether the fix belongs in
  the JIT or in `lang_invoke.rs`.
* A minimal repro. `probes/MhCombinatorProbe.java` covers the arity and purity
  of every combinator Groovy's chain uses and passes all its arity rows on the
  current tree, so whatever this is, that probe does not yet reach it — start by
  extending it with the exact `(Object,Object,String,Object[]) -> (Object,Object)`
  reduction rather than by reading `Selector.java`.
* One strong candidate to rule in or out first: `asType` and
  `explicitCastArguments` on this VM adapt the RECEIVER IN PLACE and hand it
  back, where every JDK returns a new handle and leaves the original alone (the
  G31-1 nomination recorded on the `asType` registration;
  `probes/MhCombinatorProbe.java`'s purity section fails four rows on it). A
  chain builder that holds a handle across several adaptations — which
  `Selector` does — is exactly the shape that aliasing corrupts, and the
  corruption is arity-shaped.

## Repro

```bash
cd apps/spring-suite-runner
export SPRING=/data/cratonvm/apps/spring-framework JDK25="$JAVA_HOME"
export CRATONVM_BIN=<cratonvm-bin>

./one.sh org.springframework.web.servlet.view.groovy.GroovyMarkupViewTests
#   -> succ=7 fail=3
CRATONVM_JIT_SELF_TAILCALL=1 \
./one.sh org.springframework.web.servlet.view.groovy.GroovyMarkupViewTests
#   -> succ=10 fail=0
```

`KRUN_STACK=1` prints the full `MultipleCompilationErrorsException` chain quoted
above; without it the failcause truncates at `startup failed:`, which is how the
2026-08-19 sweep recorded this class and why it was filed then as a possible
Groovy/classpath version mismatch rather than a VM defect.
