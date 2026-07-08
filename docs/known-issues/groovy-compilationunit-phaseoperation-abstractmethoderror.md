# Groovy `CompilationUnit$PhaseOperation.doPhaseOperation` AbstractMethodError

| | |
|---|---|
| **Status** | OPEN, found 2026-07-07 during a Spring suite non-passed rerun on Azure (dev, real-JDK, JIT on). |
| **Area** | Groovy scripting support — dispatch through anonymous/lambda subclasses of an abstract inner class. |

## Symptom

```
java.lang.AbstractMethodError: method org/codehaus/groovy/control/CompilationUnit$PhaseOperation.doPhaseOperation(Lorg/codehaus/groovy/control/CompilationUnit;)V has no Code attribute
```

`org.springframework.scripting.groovy.GroovyScriptFactoryTests` fails
identically on 4 methods: `staticScriptWithInlineDefinedInstance()`,
`prototypeScriptFromTag()`, `staticScriptWithInstance()`, `factoryBean()` —
one shared root cause.

`AbstractMethodError: ... has no Code attribute` is CratonVM's standard
symptom for dispatch resolving to an interface/abstract method slot instead
of the real overriding implementation. `CompilationUnit$PhaseOperation` is an
abstract inner class in Groovy's own compiler; Groovy's compilation pipeline
invokes concrete phase-operation instances (commonly created as anonymous
inner classes or lambdas) through this abstract type. CratonVM is resolving
the call back to the abstract declaration rather than the concrete override.

## Initial read

Likely the same general family as other anonymous-inner-class /
lambda-as-abstract-class dispatch gaps previously found in Groovy support
(see the `spring-bug-11` Groovy `invokedynamic`/dispatch fix history) — check
whether this is a residual of that work or a new instance. Since Groovy
compiles its own phase pipeline as an object graph of `PhaseOperation`
overrides, the fix likely needs invoke-cache/vtable population to correctly
resolve to the concrete subclass instead of caching the abstract declaration.

## Reproduction

Azure host suite runner (path depths on this host shift between `/data/data/`
and `/data/data/data/` — verify with `ls -d` at both before trusting either):

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
