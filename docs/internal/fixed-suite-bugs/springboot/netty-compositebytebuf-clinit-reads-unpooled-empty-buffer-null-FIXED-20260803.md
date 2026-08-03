# Netty `CompositeByteBuf.<clinit>` reads `Unpooled.EMPTY_BUFFER` as null — FIXED

**Status: FIXED — verified 2026-08-03.**

## Original symptom

`WebServiceMessageSenderFactoryTests.httpWhenDetectedReactor()` failed with a
bare `java.lang.NoClassDefFoundError`. The VM's own stderr trace showed the
underlying cause:

```
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError class=io/netty/buffer/CompositeByteBuf cause=java/lang/NullPointerException Cannot invoke "io.netty.buffer.ByteBuf.nioBuffer()" because "io.netty.buffer.Unpooled.EMPTY_BUFFER" is null
```

## Root cause (confirmed)

The test carries `@ClassPathExclusions({"httpclient5-*.jar", "jetty-client-*.jar"})`,
which Spring's `ModifiedClassPathExtension` implements by re-running the test
under an isolated `URLClassLoader`. Temporary `warn!`-level `<clinit>` entry
tracing (added and removed for this investigation) captured the exact chain
on that isolated loader:

```
Unpooled.<clinit> starts
  -> getstatic UnpooledByteBufAllocator.DEFAULT
  -> (JVMS §5.5: init superclass first) AbstractByteBufAllocator.<clinit>
    -> ResourceLeakDetector.addExclusions(AbstractByteBufAllocator.class, "toLeakAwareBuffer")
      -> AbstractByteBufAllocator.class.getDeclaredMethods()
        -> native_class_get_declared_methods -> link_isolated_method_signatures
          -> return type of declared method compositeBuffer() is CompositeByteBuf
          -> loader.loadClass("io.netty.buffer.CompositeByteBuf")   [correct]
          -> ctx.initialize_class(CompositeByteBuf)                 [BUG: runs <clinit> now]
            -> CompositeByteBuf.<clinit> reads Unpooled.EMPTY_BUFFER
               (still null — Unpooled.<clinit> hasn't reached its
               `putstatic EMPTY_BUFFER` yet, same thread, re-entrant) -> NPE
```

`link_isolated_method_signatures` (`native-builtins/src/lang_class.rs`, added
2026-07-30 for the OnBeanCondition/`BeanTypeDeductionException` residual) walks
every declared method of a class loaded by an isolated `URLClassLoader` and,
for each reference-typed return/parameter type, calls `loader.loadClass(...)`
**and then `ctx.initialize_class(class_id)`** — i.e. it runs the type's full
`<clinit>`. JVMS §5.5 never requires initialization merely because a class
name appears in another class's method signature; only `new`/`getstatic`/
`putstatic`/`invokestatic` on that exact class does. `loadClass` alone (load +
verify + prepare) was already sufficient to turn a genuinely missing
transitive type into `NoClassDefFoundError` for the original OnBeanCondition
case — the extra `initialize_class` call was gratuitous, and here it fired
while the calling thread was still mid-way through `Unpooled.<clinit>`,
letting `CompositeByteBuf.<clinit>` observe `Unpooled.EMPTY_BUFFER` before it
was assigned.

This was CratonVM-specific: real Netty bytecode for `Unpooled`,
`UnpooledByteBufAllocator`, `AbstractByteBufAllocator`, and `EmptyByteBuf`
never references `CompositeByteBuf` (confirmed by disassembling
netty-buffer 4.2.13.Final) — on HotSpot this reflective walk resolves
`CompositeByteBuf` as a `Class` object but never runs its `<clinit>`.

## Fix

- `vm/src/vm/vm_util.rs`: added a thread-local `<clinit>` nesting-depth guard
  (`ClinitDepthGuard` / `in_clinit_shared()`), incremented/decremented around
  the existing `<clinit>` invocation in `initialize_class_shared`.
- `native-api/src/registry.rs`: added `NativeContext::in_clinit()` (default
  `false`) so native code can query the depth without a `vm` crate dependency.
- `vm/src/vm/vm_exec.rs`: implemented `in_clinit()` for `NativeContextImpl`,
  backed by `in_clinit_shared()`.
- `native-builtins/src/lang_class.rs`: `link_isolated_method_signatures` now
  skips the `ctx.initialize_class(class_id)` call (treating the already-proven
  `loadClass` success as sufficient) whenever `ctx.in_clinit()` is true. Outside
  a `<clinit>` — the common case, including the original OnBeanCondition
  reproducer — behavior is unchanged.

## Validation

Built `cratonvm` from a worktree branched off `origin/dev`
(`fix/netty-compositebytebuf-clinit-20260803`) and ran on the Azure Linux
build host, JIT and `--nojit`:

| Module | Class | Result |
|---|---|---|
| `module/spring-boot-webservices` | `WebServiceMessageSenderFactoryTests` | 7/7 PASS (JIT + `--nojit`), stable across 4 repeated JIT runs |
| `module/spring-boot-webservices` | `OnWsdlLocationsConditionTests`, `WebServicesPropertiesTests`, `WebServicesAutoConfigurationTests`, `WebServiceTemplateAutoConfigurationTests`, `WebServiceTemplateBuilderTests` | all PASS (whole module clean) |
| `core/spring-boot-autoconfigure` | `OnBeanConditionTypeDeductionFailureTests` (the original 2026-07-30 `link_isolated_method_signatures` target) | 1/1 PASS — confirms the guard does not regress the eager-`NoClassDefFoundError` behavior outside a `<clinit>` |
| `module/spring-boot-actuator` | `ThreadDumpEndpointTests` (also from the 2026-07-30 fix set) | 2/2 PASS |
| `test-support/spring-boot-test-support` | `ModifiedClassPathExtensionOverridesTests`, `ModifiedClassPathExtensionOverridesParameterizedTests` | Still fail, but on an unrelated pre-existing cause (`SSLSocketFactory has no owning SSLContext` resolving Maven artifacts over the network) — same failure already listed in `docs/known-issues/springboot/non-passed.md` before this fix |
| `test-support/spring-boot-test-support` | `ResourcesTests` | Still 1/12 fails on an unrelated pre-existing filesystem issue (`IOException: Is a directory`), already listed in `non-passed.md` |

`WebServiceMessageSenderFactoryTests` is removed from the `non-passed.md`
24-FAIL snapshot as of this fix.
