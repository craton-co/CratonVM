# Groovy `CompilationUnit$PhaseOperation.doPhaseOperation` AbstractMethodError - FIXED

| | |
|---|---|
| **Status** | FIXED 2026-07-08 on `codex/groovy-phaseop-abstractmethod-20260708-1` / `dev`. |
| **Found** | 2026-07-07 during a Spring suite non-passed rerun on Azure (dev, real-JDK, JIT on). |
| **Area** | Groovy scripting support - lambda proxy dispatch through interface default methods. |

## Symptom

```text
java.lang.AbstractMethodError: method org/codehaus/groovy/control/CompilationUnit$PhaseOperation.doPhaseOperation(Lorg/codehaus/groovy/control/CompilationUnit;)V has no Code attribute
```

`org.springframework.scripting.groovy.GroovyScriptFactoryTests` failed
identically on 4 methods: `staticScriptWithInlineDefinedInstance()`,
`prototypeScriptFromTag()`, `staticScriptWithInstance()`, `factoryBean()` -
one shared root cause.

`AbstractMethodError: ... has no Code attribute` is CratonVM's standard
symptom for dispatch resolving to an interface/abstract method slot instead
of the real implementation. Here the receivers were synthetic lambda proxy
class ids. The proxy metadata was present, but dispatch arrived through
Groovy's parent `CompilationUnit$PhaseOperation` interface. That parent
declares `doPhaseOperation(CompilationUnit)` abstractly; child functional
interfaces such as `ISourceUnitOperation` and `IPrimaryClassNodeOperation`
provide concrete default implementations that call their SAMs.

## Root cause

The no-code AbstractMethodError fallback already knew how to rescue lambda
receivers by calling `try_lambda_dispatch`, but that helper intentionally
handles SAM calls plus a few native default-method patterns. For this Groovy
shape the invoked method is the inherited default `doPhaseOperation`, while
the lambda SAM is `call(...)`, so SAM dispatch declined and the VM threw from
the abstract parent interface slot.

The missing edge was: synthetic lambda proxy receiver, resolved method is an
abstract superinterface declaration, but the proxy's functional interface has
a concrete default body for the exact same name and descriptor.

## Fix

When a lambda proxy reaches the no-code fallback and SAM dispatch returns
`None`, CratonVM now resolves the exact method name and descriptor starting
from the proxy's functional interface. If `find_method_recursive` finds a
concrete non-abstract method body there, the VM invokes that declaring
interface with `invoke_on_class_shared_no_retarget`, preserving the lambda
proxy as `this`.

This leaves normal SAM routing unchanged and fills only the missing
superinterface-to-functional-interface default-method path.

## Reproduction

Azure host suite runner (path depths on this host shift between `/data/data/`
and `/data/data/data/` - verify with `ls -d` at both before trusting either):

```bash
WT=/data/data/wt-osr-nonpassed-20260706-1945   # prebuilt Spring suite + frozen binary
cd $WT/apps/spring-suite-runner
echo org.springframework.scripting.groovy.GroovyScriptFactoryTests > /tmp/list.txt
SF=$WT/apps/spring-framework RUNNER=$WT/apps/spring-suite-runner \
  CRATONVM_BIN=$WT/cratonvm-osr-nonpassed-20260706.bin JH=/data/data/jdk25-real \
  BATCH=1 BATCH_TO=120 ONE_TO=120 LIST=/tmp/list.txt OUT=/tmp/out SHARD_N=1 SHARD_ID=0 \
  bash suite-run.sh
# see /tmp/out/failcauses.log and /tmp/out/raw.log
```

HotSpot passes all 4 methods.

## Validation

- Baseline current-dev binary reproduced the reported
  `PhaseOperation.doPhaseOperation` AME in `GroovyScriptFactoryTests`.
- Fixed binary: `/data/data/bin/cratonvm-groovy-phaseop-20260708-1-fix1`.
- Re-ran `org.springframework.scripting.groovy.GroovyScriptFactoryTests` with
  the Spring suite harness on Azure. The reported `PhaseOperation.doPhaseOperation`
  `AbstractMethodError` no longer appears in `raw.log`, `failcauses.log`, or
  `.err`.
- `cargo test -p cratonvm-classloading m2_find_method_recursive --lib`: 6/6
  passed, covering the default-method lookup helper this fix relies on.
- The class still exposes later Groovy/Spring functional residuals after it
  gets past compilation; those are separate from this AME dispatch bug.
