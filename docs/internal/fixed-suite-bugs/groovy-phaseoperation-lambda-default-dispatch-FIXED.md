# Groovy PhaseOperation lambda default dispatch - FIXED

| | |
|---|---|
| **Status** | FIXED on `codex/groovy-phaseop-abstractmethod-20260708-1` |
| **Found** | 2026-07-07 Spring suite non-passed rerun on Azure |
| **Area** | Lambda proxy dispatch through interface default methods |

## Symptom

`org.springframework.scripting.groovy.GroovyScriptFactoryTests` failed during
Groovy compilation with:

```text
java.lang.AbstractMethodError: method org/codehaus/groovy/control/CompilationUnit$PhaseOperation.doPhaseOperation(Lorg/codehaus/groovy/control/CompilationUnit;)V has no Code attribute
```

The failing receiver class ids were synthetic lambda proxy ids. The proxy
metadata was present, but dispatch was invoked through Groovy's parent
`CompilationUnit$PhaseOperation` interface. That parent declares
`doPhaseOperation(CompilationUnit)` abstractly, while child functional
interfaces such as `ISourceUnitOperation` and `IPrimaryClassNodeOperation`
provide concrete default implementations that call their SAMs.

## Root cause

The no-code AbstractMethodError fallback knew how to rescue a lambda receiver
by calling `try_lambda_dispatch`, but that helper intentionally handles only
SAM calls and a few native default-method patterns. For this Groovy shape the
method name was the inherited default `doPhaseOperation`, not the SAM `call`,
so the rescue returned `None` and the VM threw AME from the abstract parent
interface slot.

## Fix

When a lambda proxy reaches the no-code fallback and SAM dispatch declines the
call, CratonVM now resolves the exact method name and descriptor starting from
the proxy's functional interface. If `find_method_recursive` finds a concrete
default method body there, the VM invokes that declaring interface with
`invoke_on_class_shared_no_retarget`, preserving the lambda receiver as
`this`.

This keeps ordinary SAM routing unchanged and only fills the missing
superinterface-to-functional-interface default-method path.

## Validation

- Built `/data/data/bin/cratonvm-groovy-phaseop-20260708-1-fix1` from the fix
  worktree.
- Re-ran `org.springframework.scripting.groovy.GroovyScriptFactoryTests` with
  the Spring suite harness on Azure. The reported `PhaseOperation.doPhaseOperation`
  `AbstractMethodError` no longer appears in `raw.log`, `failcauses.log`, or
  `.err`.
- The class still exposes later Groovy/Spring functional residuals after it
  gets past compilation; those are not this AME dispatch bug.
