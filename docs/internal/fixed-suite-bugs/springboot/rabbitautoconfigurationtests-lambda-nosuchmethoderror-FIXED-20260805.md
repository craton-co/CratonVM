# `RabbitAutoConfigurationTests` — `NoSuchMethodError` on a lambda's `apply` — RESOLVED

**Status: FIXED (2026-08-05).** Not a lambda-metafactory defect. It is the
recycled-`JitInvokeInfo` dispatch aliasing fixed by `383e7f5cf`; see
`flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`
for the mechanism and the bisection.

## Original symptom (as filed)

42 of 76 tests failed plus 1 container failure in the 2026-08-05 Azure
full-suite run:

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="<unknown class 2147484086>.apply(Ljava/lang/Object;)Ljava/lang/Object;"
  caller="org/springframework/boot/autoconfigure/AutoConfigurationSorter.getInPriorityOrder(…) @pc=59"
```

repeated dozens of times, then:

```
java.lang.IllegalStateException: Unable to read meta-data for class …RabbitAutoConfiguration
Caused by: java.io.FileNotFoundException: class path resource [null] cannot be opened because it does not exist
```

## Root cause

The filed hypothesis was a lambda-metafactory naming/registration gap — that
`class_id=2147484086` (`0x8000_3736`, the synthetic range) had lost its
binding to the functional-interface method. **That was wrong.** The lambda
class is fine; the *call site* was not.

`vm/src/jit/helpers.rs` keys per-thread dispatch memos on
`(vm_identity, JitInvokeInfo pointer)`. Those boxes are freed with their
`CompiledMethod`, so the allocator re-issues the address to the next compile
while `VIRTUAL_TARGET_CACHE` still holds the **previous** site's resolved
dispatch class. The reused site then resolves its own — entirely correct —
method name against the wrong class.

That is precisely the signature `383e7f5cf` documents for this memo, where it
appeared as `NoSuchMethodError: java.lang.Object.annotationType()`: *a real
method name against a class that never declared it*. Here the real name is
`apply` and the wrong class is a synthetic lambda id. The
`ClassPathResource [null]` / `FileNotFoundException` cascade is downstream —
the sorter's comparator never ran, so nothing populated the resource name.

The doc's own instinct that the `FileNotFoundException` "looks like a
*secondary* symptom" was right; the primary was one level lower than it
looked.

## Validation

Local Windows, one process per class, runner env vars, `--Xmx 2g`:

| Binary | Result | Wall |
|---|---|---:|
| 08-05 full-suite binary (no `383e7f5cf`) | **42/76 FAIL** + 1 container | 93.9s (Azure) |
| current dev `96acd76ed` (has `383e7f5cf`) | **78/78 PASS** | 228.1s |
| HotSpot 25.0.3+9 control | 78/78 PASS | 10.2s |

Neither `<unknown class` nor `NoSuchMethodError` appears anywhere in the
fixed run's stdout or stderr. The two surviving `class path resource [...]`
lines are the fixture's own `[foo]`/`[bar]` missing-keystore assertions, not
the `[null]` of this bug.

Azure Linux (`/data/sbrun.sh`, `--Xmx 4g`, worktree
`/data/data/wt-flywayfix-20260805` at `origin/dev`):

```
[1/1] jit RabbitAutoConfigurationTests rc=0 PASS 233s tests=78 failed=0 aborted=0 containersFailed=0
```

Recorded history for this class on Azure: `08-02 PASS 264.506s` →
`08-05 FAIL 42/76` → fixed `PASS 233s`. It regressed inside the same
08-02→08-05 window as the Flyway and `TomcatServletWebServerAutoConfiguration`
classes, and comes back at its 08-02 speed.

This class keeps its 900s suite-runner carve-out from the 2026-07-20 CGLIB
work; at 233s it is comfortably inside it (HotSpot's own Azure baseline is
179.3s, so the CratonVM/HotSpot ratio here is only ~1.3x).

## Affected classes

- `module/spring-boot-amqp` — `org.springframework.boot.amqp.autoconfigure.RabbitAutoConfigurationTests`

Original log:
`craton-fullsuite-azure-20260805-s4/all-jit/logs/module_spring-boot-amqp.org.springframework.boot.amqp.autoconfigure.RabbitAutoConfigurationTests.{out,err}.log`
