# `RabbitAutoConfigurationTests` — synthetic lambda class `NoSuchMethodError`, 2026-08-05

**Status: OPEN — found 2026-08-05**

## Symptom

`module/spring-boot-amqp`'s `RabbitAutoConfigurationTests` fails 42 of 76
tests plus 1 container failure. This is unrelated to either prior doc on
file for this class:
`docs/internal/fixed-suite-bugs/springboot/rabbitautoconfigurationtests-cglib-enhance-hang-FIXED.md`
(fixed 2026-07-20: this class is CPU-heavy but finite, not a hang — added a
900s suite-runner timeout carve-out, and separately fixed
`KeyManagerFactory`/`TrustManagerFactory.getInstance` accepting any
algorithm string). Neither applies here: today's failure is not a timeout
(`FAIL`, not `HANG`) and is not the two SSL-algorithm tests that doc fixed.

```
[2m2026-08-05T11:40:...Z[0m WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="<unknown class 2147484086>.apply(Ljava/lang/Object;)Ljava/lang/Object;"
  caller="org/springframework/boot/autoconfigure/AutoConfigurationSorter.getInPriorityOrder(Ljava/util/Collection;)Ljava/util/List; @pc=59"
```

repeated dozens of times (one per invocation), followed in `.out.log` by:

```
java.lang.IllegalStateException: Unable to read meta-data for class org.springframework.boot.amqp.autoconfigure.RabbitAutoConfiguration
  ... AutoConfigurationSorter$AutoConfigurationClass.getAnnotationMetadata
Caused by: java.io.FileNotFoundException: class path resource [null] cannot be opened because it does not exist
  org.springframework.core.io.ClassPathResource.getInputStream
  org.springframework.core.type.classreading.SimpleMetadataReader.getClassReader
```

`AutoConfigurationSorter.getInPriorityOrder` calls a lambda
(`Collection.stream().sorted(...)` or similar) whose synthetic
`Function`/`Comparator` implementation class CratonVM reports as
`<unknown class 2147484086>` — i.e. the lambda-metafactory-generated class
has no resolvable name/identity at the point `apply` is invoked, so the
call fails `NoSuchMethodError` instead of dispatching. The caller then falls
back down a path that builds a `ClassPathResource` with a **null** resource
name, which throws `FileNotFoundException` for every autoconfiguration
class it tries to sort — explaining the 42-test failure count (essentially
every test that goes through `AutoConfigurations.of(...)`).

## Root cause

**Not confirmed — needs further investigation.** `class_id=2147484086`
(`0x8000_3736`) is in the synthetic/lambda class-id range. This looks like a
lambda-metafactory-generated class either (a) losing its binding to the
target functional-interface method by the time `getInPriorityOrder` invokes
it — a dispatch/registration gap for a specific lambda shape used inside
`AutoConfigurationSorter` — or (b) the class object itself being valid but
CratonVM's `NoSuchMethodError` diagnostic naming it wrong while the real
failure is elsewhere (the `getAnnotationMetadata`/`ClassPathResource`
`null` resource name looks like a *secondary* symptom of the sorter's
comparator/lambda never actually running, causing whatever populates the
resource name to be skipped). Worth checking
`AutoConfigurationSorter.getInPriorityOrder`'s exact lambda usage
(`AutoConfigurationClass::getAnnotationMetadata` as a method reference, or
a `Comparator` built via `Comparator.comparing(...)`) against how CratonVM's
lambda metafactory registers/names synthetic classes.

## Affected classes

- `module/spring-boot-amqp` — `org.springframework.boot.amqp.autoconfigure.RabbitAutoConfigurationTests`

Log: `craton-fullsuite-azure-20260805-s4/all-jit/logs/module_spring-boot-amqp.org.springframework.boot.amqp.autoconfigure.RabbitAutoConfigurationTests.{out,err}.log`
