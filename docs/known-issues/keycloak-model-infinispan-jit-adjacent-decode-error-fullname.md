# JIT-adjacent bytecode-decode error in `FileDescriptor.fullName` during Infinispan protostream bootstrap

Status: open

Date observed: 2026-07-07 — surfaced as a new, distinct residual once
[keycloak-model-infinispan-cache-config-null-after-real-start-FIXED](../internal/fixed-suite-bugs/keycloak-model-infinispan-cache-config-null-after-real-start-FIXED.md)
was fixed and `RealmModelTest` got much further into `DefaultCacheManager.internalStart()`.

## Summary

With JIT on (the default), `RealmModelTest` (and presumably any test that
exercises Infinispan's built-in ProtoStream schema registration — this
happens automatically during `GlobalComponentRegistry` module bootstrap, not
from anything Keycloak's own code explicitly triggers) deterministically
crashes partway through registering the ~30+ builtin `.proto` files:

```text
[cratonvm] main-vm run() returned Err: Error in thread "main" internal error: decode error at pc=51 in fullName.(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;: unexpected end of data at position 51
```

`org.infinispan.protostream.descriptors.FileDescriptor.fullName(String, String)`
(real method, from `infinispan-protostream-6.0.6.jar`) is a genuinely tiny
16-byte method (`javap -c` confirms: `aload_0; ifnonnull 8; aload_1; goto 15;
aload_0; aload_1; invokedynamic #422 (StringConcatFactory.makeConcatWithConstants,
recipe "."); areturn` — bytecode offsets 0-15, method ends at
pc=15). A "decode error at pc=51" for a method whose Code array is only 16
bytes long is impossible unless the interpreter's `pc` (or its `code` slice
reference) for this frame is corrupted before the decode is attempted — this
is NOT a bug in string-concat/invokedynamic recipe parsing itself (verified
with a minimal standalone `ConcatProbe.java` doing an equivalent
`a == null ? b : a + "." + b` — runs correctly under the same binary).

## Root cause hypothesis (not yet confirmed)

`fullName` itself contains an `invokedynamic`, and
`jit/src/ir.rs:2607`'s comment states "Invokedynamic is rejected upstream by
`jit_scan`" — so the JIT should never attempt to compile `fullName` directly.
This points at a **JIT ↔ interpreter call-boundary bug**: some OTHER method
that repeatedly calls `fullName` in a loop (during proto descriptor
parsing/resolution — plausible given how many times `Resolving dependencies
of ...proto` / `File resolved successfully` cycle through the log before the
crash) gets hot enough to JIT-compile, and something about how the
JIT-compiled caller sets up the call into the (JIT-ineligible, interpreted)
`fullName` callee corrupts that callee's fresh interpreter frame — most
likely its `pc` field or its `code` slice reference — rather than the
`fullName` method's own bytecode being at fault.

**Confirmed via `--nojit`:** running the exact same repro with
`--nojit` gets past this point entirely — proto registration completes, and
execution proceeds all the way through JGroups cluster topology recovery and
full cache-manager teardown (see the STW hang doc below for what's reached
after that). This isolates the bug to JIT compilation specifically, not the
interpreter's own invokedynamic/string-concat handling.

## Not yet root-caused / fixed

Whoever picks this up should:
1. Identify the actual JIT-compiled caller (likely something in
   `org.infinispan.protostream.descriptors.FileDescriptor`,
   `ResolutionContext`, or `Descriptor`/`EnumDescriptor` `Builder.build()`/
   dependency-resolution code, given the proto registration context)
   via `CRATONVM_JIT_BISECT_ONLY`/`CRATONVM_JIT_BISECT_SKIP` (see
   `reference_keycloak_test_harness` project memory) to binary-search which
   compiled method's absence avoids the crash.
2. Once found, dump its JIT disassembly (`CRATONVM_DBG_JIT_DISASM`) around
   the call site that invokes `fullName`, looking specifically at how the
   interpreter frame for the callee is constructed (pc initialization, code
   pointer/length setup) from JIT-compiled code.

## Repro

Same classpath/harness as the cache.config fix above: `RealmModelTest` via
`KcRunner` against the real Keycloak-resolved Infinispan 16.0.8 classpath
(`apps/keycloak-suite-runner/.suite/pathing-jars/testsuite_model-*.jar`),
run from `apps/keycloak/testsuite/model` with:
```
-Dkeycloak.model.parameters=Infinispan,Jpa
-Djava.util.logging.manager=org.jboss.logmanager.LogManager
-Dkeycloak.connectionsJpa.default.driver=org.h2.Driver
-Dkeycloak.connectionsJpa.default.database=keycloak
-Dkeycloak.connectionsJpa.default.user=sa
-Dkeycloak.connectionsJpa.default.password=
-Dkeycloak.connectionsJpa.default.url=jdbc:h2:mem:test;DB_CLOSE_DELAY=-1
```
Currently: `FAIL` with JIT on (deterministic, same pc/method every run).
`--nojit` avoids this specific crash but is otherwise much slower and then
hits the STW shutdown hang tracked separately.
