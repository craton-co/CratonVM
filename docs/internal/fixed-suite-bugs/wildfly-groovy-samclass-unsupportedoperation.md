# Groovy CachedSAMClass.getSAMMethodImpl() threw UnsupportedOperationException under CratonVM reflection

Status: FIXED 2026-07-08 - moved from docs/known-issues after adding a LinkedList.removeIf bridge and regression coverage.
Severity: Low-Medium
First confirmed: 2026-07-08, Azure worktree `test/wildfly-full-suite-20260707`, dev@6881f7e9

## Symptom

WildFly `testsuite/integration/vdx` tests that apply Groovy-scripted XML transformations failed while
instantiating generated Groovy script objects:

```text
java.lang.ExceptionInInitializerError
  at org.codehaus.groovy.runtime.InvokerHelper.<clinit>(InvokerHelper.java:66)
  ...
Caused by: java.lang.UnsupportedOperationException
  at org.codehaus.groovy.reflection.stdclasses.CachedSAMClass.getSAMMethodImpl(CachedSAMClass.java:199)
  at org.codehaus.groovy.reflection.stdclasses.CachedSAMClass.getSAMMethod(CachedSAMClass.java:161)
  at org.codehaus.groovy.reflection.ClassInfo.isSAM(ClassInfo.java:376)
  ...
```

Groovy's `ClassInfo` machinery determines whether classes are SAM types while bootstrapping ordinary
script classes. The failure surfaced as a misleading Groovy script construction error, but the failing
operation was inside `CachedSAMClass.getSAMMethod`.

## Root cause

Groovy 4.0.32's non-interface SAM detection path builds a `java.util.LinkedList` of abstract methods and
then calls `LinkedList.removeIf(Predicate)` to prune methods implemented by concrete inherited methods.

CratonVM already had `LinkedList$Itr.remove()` support, but did not have a direct
`java/util/LinkedList.removeIf(Ljava/util/function/Predicate;)Z` native for overlay-backed linked lists.
That let this path fall through to the unsupported/default iterator mutation route in synthetic and
reflection-heavy execution, surfacing as `UnsupportedOperationException` from Groovy's SAM detection.

## Fix

Added a native `LinkedList.removeIf(Predicate)` implementation in `native-collections/src/lib.rs`.

The implementation walks CratonVM's overlay linked-list nodes, invokes the supplied Java predicate for
each element, and unlinks matching nodes directly. It pins the list, predicate, current node, next node,
and object element values across the Java predicate call so GC relocation cannot invalidate native-side
references.

Added `TckUtil.linkedlist_remove_if_iterator_remove()` and wired it through the interpreter, JCK
conformance, and runtime TCK catalogs. The regression checks both `LinkedList.removeIf` and ordinary
iterator `remove()` after the remove-if pass.

## Verification

```powershell
$env:CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS='1'
cargo test -p cratonvm-vm --test interpreter_tests test_s47_linkedlist_remove_if_iterator_remove -- --nocapture
# 1 passed

cargo test -p cratonvm-native-collections
# all native-collections tests passed

javac -cp 'C:\Users\Victor\.m2\repository\org\apache\groovy\groovy\4.0.32\groovy-4.0.32.jar' scratch\GroovySamCratonRepro.java
java -cp 'scratch;C:\Users\Victor\.m2\repository\org\apache\groovy\groovy\4.0.32\groovy-4.0.32.jar' GroovySamCratonRepro
# OK

target\release\cratonvm.exe --classpath 'scratch;C:\Users\Victor\.m2\repository\org\apache\groovy\groovy\4.0.32\groovy-4.0.32.jar' GroovySamCratonRepro
# OK

cargo run -p cratonvm-cli --bin cratonvm -- --nojit --classpath 'scratch;C:\Users\Victor\.m2\repository\org\apache\groovy\groovy\4.0.32\groovy-4.0.32.jar' GroovySamCratonRepro
# OK
```

Source-built debug JIT hit an unrelated `EXCEPTION_ILLEGAL_INSTRUCTION (SIGILL)` during the scratch
Groovy probe. The release JIT binary passed the same probe, and the source-built no-JIT path passed.

## Original Evidence

```text
/data/data/wt-wildfly-bugbash-20260707-runner/out/rerun4-s1of2-jit-real-all-20260708-143042/surefire-reports/00695-org.wildfly.test.integration.vdx.standalone.NoSchemaTestCase/
/data/data/wt-wildfly-bugbash-20260707-runner/out/hscheck-vdx-hotspot-all-20260708-201951/
```

## Residual, NOT covered by this fix — separate GroovyBugError during compilation

`org.wildfly.test.integration.vdx.domain.HostXmlSmokeTestCase` (1 instance, same 2026-07-08 run) threw a
bare `org.codehaus.groovy.GroovyBugError` (no message captured) from
`CompilationUnit.applyToPrimaryClassNodes` -- i.e. during Groovy *script compilation*, not instantiation
of an already-compiled class (the SAM-detection path fixed above). Different Groovy subsystem, different
stack, but same underlying app (`creaper`'s `Subtree.SubtreeCreator`) and same module. Not confirmed
whether this fix also resolved it (only 1 low-confidence instance observed, not re-verified against the
fixed binary) -- whoever next runs the `vdx.domain.HostXmlSmokeTestCase` class should check, and re-open a
known-issues doc if it still reproduces.
