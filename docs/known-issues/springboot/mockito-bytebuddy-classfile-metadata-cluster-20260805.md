# Mockito/ByteBuddy class-metadata-derived bytecode-generation failures — cluster, 2026-08-05

**Status: OPEN — found 2026-08-05**

## Symptom

Five Spring Boot classes fail, each inside Mockito's `InlineByteBuddyMockMaker`
or `SubclassBytecodeGenerator` while ByteBuddy is generating or retransforming
a mock/proxy class against a class whose method/parameter metadata CratonVM
supplies. Each shows a different concrete exception, but all originate inside
ByteBuddy's ASM-based `TypeWriter`/`ClassReader`/`Advice` machinery, and none
reproduce on the HotSpot baseline (which passes all of these classes cleanly):

1. `core/spring-boot` `EnableConfigurationPropertiesRegistrarTests` —
   `java.lang.NoSuchMethodError: java.lang.Integer.storeAt(I)Lnet/bytebuddy/implementation/bytecode/StackManipulation;`
   during `InlineBytecodeGenerator.triggerRetransformation`, followed by
   `retransformClasses0: UnsupportedClassRedefinitionError { ... "new bytes
   failed bytecode verification ... expected Float on stack, found Int" }` for
   `AbstractAutowireCapableBeanFactory.filterPropertyDescriptorsForDependencyCheck`.
2. `core/spring-boot-autoconfigure` `CertificateMatcherTests` —
   `java.lang.NullPointerException: Cannot invoke "java.util.List.size()"
   because "this.parameterDescriptions" is null` inside
   `ParameterList$TypeSubstituting.size` → `MethodGraph$Compiler$Default`,
   while `SubclassBytecodeGenerator.mockClass` builds a plain (non-inline)
   mock. Distinct from the earlier, already-`FIXED`
   `springboot-certificatematchertests-dsa-keypairgenerator-FIXED.md` (that
   doc's symptom was a missing DSA `KeyPairGenerator`, `tests=0
   containersFailed=4`; today's is `tests=18 failed=0 containersFailed=1`,
   a completely different mechanism against the same class).
3. `core/spring-boot-testcontainers` `ServiceConnectionContextCustomizerTests` —
   `java.lang.IllegalArgumentException: Lookup.defineClass: Linkage(ClassFormatError
   { class_name: "", message: "unexpected end of data at position 40" })`
   from `ClassInjector$UsingLookup.injectRaw` → `MethodHandles.Lookup.defineClass`
   (a hidden-class define, not retransformation).
4. `loader/spring-boot-loader` `JarUrlConnectionTests` —
   `java.lang.NullPointerException: Cannot invoke
   "net.bytebuddy.implementation.bytecode.StackManipulation.isValid()" because
   "writeAssignment" is null` inside `Advice$OffsetMapping$ForReturnValue.resolve`
   during `InlineBytecodeGenerator.triggerRetransformation` (2/47 tests fail).
5. `module/spring-boot-grpc-server` `GrpcServerHealthAutoConfigurationTests` —
   `java.lang.ClassCastException: java.lang.Integer cannot be cast to
   net.bytebuddy.implementation.bytecode.StackManipulation` inside
   `StackManipulation$Compound.<init>` while `ClassReader.readCode` replays a
   retransformed method's bytecode (1/30 tests fail).

## Root cause

**Not confirmed — needs further investigation.** All five sit downstream of
either (a) `retransformClasses0`/`InlineBytecodeGenerator`'s JVMTI-style
class-retransformation path, which hands ByteBuddy's ASM `ClassReader` a
byte array CratonVM assembled, or (b) `Lookup.defineClass`/`SubclassBytecodeGenerator`,
which reads CratonVM's `Class.getMethods()`/`getParameters()`-derived
metadata to build a new class. The shapes differ (a `NoSuchMethodError` for a
synthetic ASM-internal helper, an NPE for a null `parameterDescriptions`
list, a `ClassFormatError` truncated at byte 40, an NPE for a null
`writeAssignment`, and a CCE where ASM expected a `StackManipulation` object
but got a raw `Integer`) — consistent with several distinct off-by-something
bugs in the same family (CratonVM's class-file bytes or reflected
method/parameter descriptors disagreeing with what real HotSpot would
produce for the same class), rather than one single root cause. Worth
checking first: whether `native-builtins/src/lang_class.rs`'s
`Class.getMethods()`/`getParameters()` (already the confirmed root cause of
the unrelated, already-`FIXED` `class-getmethods-override-shadowing-duplicate-close-cluster`
and `jooq-destroy-method-ambiguity-and-hang` Cluster A bugs) has a sibling
gap in how it reports parameter metadata or method bytecode length/offsets
for these five specific method shapes.

## Affected classes

- `core/spring-boot` — `org.springframework.boot.context.properties.EnableConfigurationPropertiesRegistrarTests`
- `core/spring-boot-autoconfigure` — `org.springframework.boot.autoconfigure.ssl.CertificateMatcherTests`
- `core/spring-boot-testcontainers` — `org.springframework.boot.testcontainers.service.connection.ServiceConnectionContextCustomizerTests`
- `loader/spring-boot-loader` — `org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests`
- `module/spring-boot-grpc-server` — `org.springframework.boot.grpc.server.autoconfigure.health.GrpcServerHealthAutoConfigurationTests`

Full-suite rerun: `craton-fullsuite-azure-20260805-s1/s2/s3/s5`, `all-jit`.
