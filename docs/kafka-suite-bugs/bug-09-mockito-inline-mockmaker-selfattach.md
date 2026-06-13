# Bug 09 — Mockito inline mock-maker fails to initialize (self-attach / Instrumentation) — DOMINANT

**Severity:** Critical / dominant — **307 test failures** across **15+ classes** in the
partial sweep alone; the single largest failure cluster in the kafka suite. Every
test that creates a Mockito mock (`mock(...)`, `@Mock`, `spy(...)`) fails.
Reproduces under `--nojit`. HotSpot clean.

## Symptom
```
Mockito is currently self-attaching to enable the inline-mock-maker. This will no
longer work in future releases of the JDK. Please add Mockito as an agent ...
=> java.lang.IllegalStateException: Could not initialize plugin:
   interface org.mockito.plugins.MockMaker (alternate: null)
     org.mockito.internal.MockitoCore.mock(MockitoCore.java:78)
     ...
   Caused by: java.lang.IllegalStateException: Internal problem occurred, please
   report it. Mockito is unable to load the default implementation of class that
   is a part of Mockito distribution. Failed to load interface
   org.mockito.plugins.MockMaker
     org.mockito.Mockito.<clinit>(Mockito.java:1810)
```

## Root cause
Mockito 5.x defaults to the **inline mock maker** (`mockito-core` bundles
`InlineByteBuddyMockMaker`), which needs a JVM **agent / `java.lang.instrument`
`Instrumentation`** to redefine/retransform classes at runtime. With no agent on
the command line, Mockito **self-attaches** at runtime (dynamic agent attach to its
own JVM) to obtain an `Instrumentation`. CratonVM does not implement the dynamic
agent-attach + `java.lang.instrument` surface (`Instrumentation.redefineClasses` /
`retransformClasses`, `ByteBuddyAgent.install()`), so the mock maker cannot
initialize → `MockMaker` plugin load fails → `IllegalStateException`.

This is the long-standing "Mockito self-attach" gap (see project memory).

## Fix direction (large)
Two viable paths:
1. **Implement enough of `java.lang.instrument` + self-attach** for ByteBuddy's
   `ByteBuddyAgent.install()` to succeed and `Instrumentation.redefineClasses` to
   work for the inline mock maker. This is the general fix but substantial.
2. **Provide a working subclass/proxy mock maker path** so Mockito can fall back to
   the subclass mock maker (`mock-maker-subclass`) — but the suite jars default to
   inline, and forcing subclass mode would require a resource
   (`org/mockito/configuration/...` / `mockito-extensions/org.mockito.plugins.MockMaker`)
   which we must not inject into the app (no synthetic stubs). Better to make
   `ByteBuddyAgent.install()` succeed.

Recommended: make ByteBuddy `ByteBuddyAgent.install()` / self-attach succeed (the
generic capability), which unblocks this entire cluster at once.

## Progress (2026-06-12) — self-attach + 6 downstream blockers FIXED (mock now generates)

The inline mock maker now self-attaches, injects its bootstrap dispatcher,
instruments the type hierarchy, AND generates + loads the mock subclass. Seven
distinct, general VM fixes landed (branch `fix/kafka-suite-loop`); each unblocked
the next layer:

1. **In-process self-attach** — implemented `com.sun.tools.attach.VirtualMachine`
   `attach`/`loadAgent`/`detach` as natives that drive the agent JAR's
   `agentmain(String,Instrumentation)` in-process (reusing the `-javaagent`
   `InstrumentationImpl` infra), and defaulted `-Djdk.attach.allowAttachSelf=true`
   so ByteBuddy takes the direct (not spawn-an-external-process) attach branch.
   `ByteBuddyAgent.install()` now returns a real `Instrumentation`.
   (`vm/src/runtime/instrument.rs` `register_self_attach_natives`; `vm_init.rs`.)
2. **JarEntry zip-layout shadow** — the synthetic `JarEntry` natives in
   `register_p59_jar` (active in real-JDK mode) wrote a 4-field layout (method=slot 3)
   over the REAL 17-field JDK `JarEntry`, leaving the real `method`/`crc`/`size`/
   `csize` fields (slots 9/5/6/7) at 0. A default `JarOutputStream` entry was thus
   read as STORED(0) → `ZipException: attempt to write past end of STORED entry`
   when Mockito builds its bootstrap-injection JAR. Made the `JarEntry` natives
   layout-aware (`native-builtins/src/phases_late.rs`).
3. **`appendToClassLoaderSearch0(JLjava/lang/String;Z)V`** — JDK 25's unified
   bootstrap/system classpath-append native was missing; added it
   (`instrument.rs`).
4. **Bootstrap classpath append** — `appendToBootstrapClassLoaderSearch` now
   targets the BOOTSTRAP finder (new `extend_bootstrap_classpath` /
   `register_bootstrap_classpath`), so Mockito's injected `MockMethodDispatcher`
   loads with `ClassLoaderId::Bootstrap`.
5. **`Class.getClassLoader()` for bootstrap-injected non-JDK classes** — it
   returned the app loader for any bootstrap-tagged class outside the JDK
   packages; now returns `null` for a *real* (non-stub) bootstrap class, which is
   what Mockito's `MockMethodDispatcher.<clinit>` asserts.

6. **`ReflectionFactory.getExecutableTypeAnnotationBytes` returned an empty
   `byte[]`** instead of `null`. `sun.reflect.annotation.TypeAnnotationParser`
   treats `null` as "no type annotations" but does `ByteBuffer.wrap(bytes)
   .getShort()` on a non-null buffer — so an empty array threw
   `BufferUnderflowException`. (This was the real cause of the "Could not modify
   all classes … BufferUnderflowException" — the retransform byte-provision path
   itself was already correct; verified the transformer receives byte-identical
   class bytes.) Returns `null` now (`native-builtins/src/lib.rs`).
7. **`TypeVariable.getGenericDeclaration()` returned null.** ByteBuddy's mock
   generation (`OfTypeVariable$ForLoadedType.getTypeVariableSource`) throws
   `IllegalStateException: Unknown declaration: null` if a type variable has no
   generic declaration. CratonVM's synthetic `TypeVariable` objects didn't carry
   one. Added a 3rd field (the declaring `Class`/`Executable`) populated for
   type-parameter *declarations* (`Class`/`Method.getTypeParameters`) and, via a
   thread-local `GenericDeclScope`, for type-variable *uses* inside method/field/
   superclass/interface signatures (`generics.rs`, `lang_class.rs`,
   `lang_reflect.rs`, `phases_late.rs`).

With (1)–(7) the inline mock maker **generates and loads the mock subclass**
(`...codegen/List$MockitoMock$…`).

8. **`Class.getModifiers()` returned 0 for primitive types and `void`** (the
   layer-8 root cause). `int.class.getModifiers()` was `0x000` on CratonVM vs
   `0x411` (`PUBLIC|FINAL|ABSTRACT`) on HotSpot — all 9 primitives + `void`.
   That made every primitive type read as *package-private*. Mockito's
   `SubclassBytecodeGenerator.mockClass` ends its builder chain with
   `ignoreAlso(isPackagePrivate().or(returns(isPackagePrivate())).or(
   hasParameters(whereAny(hasType(isPackagePrivate())))))` — so ByteBuddy
   *ignored* (left unimplemented) every method whose return type or any
   parameter type was a primitive. The generated mock therefore had only its
   all-reference-signature methods (`toString`, `iterator`, `toArray`,
   `getMockitoInterceptor`, …) and was missing `size()→int`, `isEmpty()→boolean`,
   `hashCode()→int`, `equals(Object)→boolean`, `get(int)`, `clear()→void`,
   `setMockitoInterceptor(…)→void`, etc. Invoking any of them hit the inherited
   abstract method → `AbstractMethodError: … has no Code attribute`. Fixed
   `native_class_get_modifiers` (`native-builtins/src/lang_class.rs`) to return
   `PUBLIC|FINAL|ABSTRACT` for primitive/`void` mirrors and
   `FINAL|ABSTRACT|<element-access>` for array classes (arrays were also wrong:
   `int[]` was `0x011`, missing `ABSTRACT`).

   **How it was isolated (the method-by-method bisection):** the CratonVM mock
   dumped 11 methods vs HotSpot's 32 (`-Dnet.bytebuddy.dump=DIR`). The drop was
   NOT in ByteBuddy's `MethodGraph` (probe: 40 nodes both VMs), NOT in plain
   subclass writing (`StubMethod` → 43 both), NOT in `MethodDelegation` binding
   (`@RuntimeType` → 43 both), and NOT in the individual `ElementMatchers`
   (`isHashCode`/`isEquals`/`returns(int.class)`/`takesArgument` all identical).
   The discriminator was purely "primitive anywhere in the signature", which
   pointed at a *type-level* predicate; decompiling Mockito's builder revealed
   the `isPackagePrivate()` `ignoreAlso`, and `int.class.getModifiers()` was the
   bug. Probes: `apps/kafka/tests/repro/{GraphProbe,SubProbe,DelProbe,
   MatchProbe,ModProbe}.java`.

With (1)–(8) `Mockito.mock(List.class)` is created and **stubbing**
(`when(...).thenReturn(...)`) works. Two more fixes were needed for
`verify(...)` / interaction-recording:

9. **`new LinkedList<>(Collection)` dropped all elements.** No
   `<init>(Ljava/util/Collection;)V` native was registered, so the copy
   constructor ran real JDK bytecode (`this(); addAll(c)`) which links nodes
   into the real `first`/`last`/`size` fields while CratonVM's overridden
   `size()`/`iterator()` read the native overlay → the copy looked empty. Added
   `native_ll_init_from_collection` (seeds the overlay, then `ll_link_last`s the
   source's elements). (`native-collections/src/lib.rs`.)
10. **`LinkedList.stream()` / `spliterator()` were empty on the overlay.** The
    `Collection.stream()` default routes through the real `LinkedList.
    spliterator()`, which reads the JDK `first`/`size` fields the overlay never
    populates → an empty stream for a non-empty list. Added `native_ll_stream`
    and `native_ll_spliterator` (array-backed, over the overlay snapshot).
    (`native-collections/src/lib.rs`.)

Fixes (9)+(10) were the last link in `verify(...)`: Mockito's
`DefaultRegisteredInvocations.getAll()` does
`new LinkedList<>(invocations)` then `copy.stream().filter(...)
.collect(toList())`. With the broken copy/stream the recorded invocations
vanished and every `verify(...)` failed with "zero interactions" even though the
mock's `handle()`/`Answer` fired. Isolated by reflecting into the live
`InvocationContainerImpl` (raw list size 3, `getAll()` 0) then bisecting the
copy → stream → filter path. Probes: `apps/kafka/tests/repro/{LLProbe,
StreamProbe,ItrRemProbe,ContProbe3,IgnProbe}.java`.

**STATUS: bug-09 COMPLETE.** Full inline mocking works: creation, stubbing,
`verify`/`times`, interface mocks with primitive-typed methods. Verified
`MockFull.java` prints `ALL MOCK BEHAVIOR OK` and the previously-failing
`FutureRecordMetadataTest` passes 2/2 (== HotSpot). Repro:
`apps/kafka/tests/repro/{MockProbe,MockFull}.java`.

## Affected classes (partial sweep — append more as the full run completes)
- clients.MetadataTest, clients.NetworkClientTest
- consumer.internals.AsyncKafkaConsumerTest, CommitRequestManagerTest,
  ConsumerNetworkClientTest, ConsumerNetworkThreadTest, CoordinatorRequestManagerTest,
  HeartbeatRequestManagerTest, MembershipManagerImplTest, NetworkClientDelegateTest,
  OffsetsRequestManagerTest, TopicMetadataRequestManagerTest, WakeupTriggerTest
- consumer.internals.events.ApplicationEventProcessorTest
- producer.internals.FutureRecordMetadataTest
- (expect many more consumer/producer/admin internals classes — Mockito is pervasive)
