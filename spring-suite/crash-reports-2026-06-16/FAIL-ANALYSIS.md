# Spring suite — CratonVM bug families hiding in the FAILs (run 2026-06-16)

Clustering ~1,237 `FAILCAUSE` lines collected so far (partial run, ~960 classes) to separate genuine
**CratonVM-unique** correctness bugs from environmental noise. Confirmed CV-unique where noted (HotSpot
passes the same class).

## CV-unique bug families (real CratonVM bugs)
| Family | Signature | Blast radius | CV-unique? | Report |
|--------|-----------|--------------|-----------|--------|
| **Generics reflection** | `FieldTypeSignature cannot be cast to Type[]` | **12 classes** (core spring-beans/AOP/events/binding) | ✅ (BeanWrapperGenericsTests 42/42 on HotSpot) | [bug-05](bug-05-generics-fieldtypesignature-cce.md) |
| **Interface "no Code attribute"** | `method <iface>.<m> has no Code attribute` for `Consumer.accept`, `Function.apply`, `Collector.supplier`, `WritableByteChannel.write`, `FileSystemProvider.isSameFile`, `Delayed.getDelay` | 9 classes | known itable family (spring-bug-03) | spring-bug-03 |
| **GC/mirror corruption → Object** | `java/lang/Object cannot be cast to java/lang/Class \| CharSequence \| ...Failure` | several | known GC race (bug-04) | bug-04 |
| **CGLIB VerifyError** | `...$$EnhancerByCGLIB$$N.testBean: Java 7+ method ... requires StackMapTable` | 1+ (`ConfigurationClassAspectIntegrationTests`) | likely CV verifier (CGLIB gen lacks frames) | (new) |
| **Synthetic anon clone** | `NoSuchMethodError: cratonvm/synthetic/AnonymousObject$4.clone()` | `MetadataNamingStrategyTests` | CV synthetic-class gap | (new) |
| **NPE cascades** | `Cannot invoke getDeclaredMethod on null` (19×), `... write on null` (8×), `... length on null` (8×) | — | reflection/lookup returns null where HotSpot returns a value | needs tracing |

## Environmental (NOT CratonVM bugs — HotSpot also fails; drop in triage)
Dominant `NoClassDefFoundError`s are missing optional/test-only deps:
```
93  org/springframework/aot/test/generate/TestGenerationContext   (test-only module)
28  org/springframework/core/ReactiveAdapterRegistry$MutinyRegistrar (Mutiny optional)
13  org/springframework/core/test/io/support/MockSpringFactoriesLoader (test util)
 6  org/springframework/.../MethodValidationInterceptor$ReactorValidationHelper (Reactor)
 4  io/mockk/impl/JvmMockKGateway   (Kotlin MockK)
 3  org/springframework/core/test/tools/TestCompiler (test util)
```
`sun/reflect/misc/Trampoline` (6×) is a JDK-internal — borderline; worth a quick check.

## Cascades (large counts, root cause is the nested exception)
- `BeanCreationException` (375), `BeanDefinitionParsingException` (130), `BeanDefinitionStoreException`
  (28) — mostly *downstream* of the generics-reflection bug (bug-05), the interface-dispatch bug, and
  the environmental missing-deps. Re-triage these after bug-05 is fixed; many should clear.

## Method
`grep '^FAILCAUSE' shard-*/failcauses.log` → cluster by exception type → drill into type/dispatch/value
errors → HotSpot-triage representatives. Pure value mismatches (`AssertionFailedError`, 145) are not yet
individually triaged — a second pass after the run completes will bucket those (they're the remaining
CV-unique correctness tail once the big families above are removed).

> Updated as the run completes; counts are from the partial (~960-class) sample.
