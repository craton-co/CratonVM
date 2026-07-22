# Pulsar `PropertyMapper` → `BiConsumer<Integer,TimeUnit>` lambda dispatch: `TimeUnit` argument arrives `null`

**Status: FIXED — already landed on `dev` 2026-07-18 (commit `102b33b65`), confirmed and closed 2026-07-21.**

## Symptom (original)

`module/spring-boot-pulsar`'s `PulsarPropertiesMapperTests` failed 1 of its
14 tests:

```
JUnit Jupiter:PulsarPropertiesMapperTests:customizeClientBuilderWhenHasFailover()
    => java.lang.NullPointerException: Cannot invoke "java.util.concurrent.TimeUnit.toNanos(long)" because "timeUnit" is null
       org.apache.pulsar.client.impl.AutoClusterFailover$AutoClusterFailoverBuilderImpl.failoverDelay(AutoClusterFailover.java:329)
       org.springframework.boot.pulsar.autoconfigure.PulsarPropertiesMapper.lambda$timeoutProperty$0(PulsarPropertiesMapper.java:221)
       org.springframework.boot.pulsar.autoconfigure.PropertyMapper$Source.to(PropertyMapper.java:292)
```

## Root cause (confirmed)

`PulsarPropertiesMapper.timeoutProperty` binds a `BiConsumer<Integer, TimeUnit>`
(`setter`) via a method reference — for the failing case,
`autoClusterFailoverBuilder::failoverDelay`, whose real target is
`AutoClusterFailoverBuilder.failoverDelay(long, TimeUnit)`. Every *other*
`timeoutProperty(...)` call site in the same file binds to a real method
taking `(int, TimeUnit)` (`ClientBuilder`/`PulsarAdminBuilder`/
`ProducerBuilder`), which is why only the `long`-typed failover/switch-back/
check-interval setters were affected and the file's many sibling tests
(admin/client timeout tests) already passed.

The actual bug was in `unbox_wrapper` (`vm/src/runtime/interpreter.rs`,
called from `coerce_lambda_args`'s per-argument coercion loop): unboxing a
boxed `Integer` argument to fulfill a `long`/`float`/`double` implementation
parameter returned the raw unboxed `Value::Int` unchanged instead of
*widening* it to `Value::Long`/`Value::Float`/`Value::Double`. The lambda
call site correctly identified that `args[i]` needed unboxing (SAM param
erased to `Object`, impl param `J`) but stopped one step short — Java's
`BiConsumer<Integer, TimeUnit>` → `(long, TimeUnit)` adaptation is
unbox-*then*-widen, not just unbox. The resulting `Value::Int`-tagged value,
carried into the real method's call frame where the bytecode expected a
`Value::Long` occupying the `long` parameter's slot, threw off the frame's
argument layout enough that the subsequent `TimeUnit` reference argument was
not read from where the callee's bytecode expected it — observed as
`timeUnit` arriving `null` inside `failoverDelay`.

This is NOT the `coerce_lambda_args`-argument-count/indexing bug speculated
in the original (2026-07-17) version of this doc — the coercion loop's
indexing, capture-count handling, and `checkcast_lambda_instantiated_args`
logic are all correct. The bug was specifically the missing widen step after
unboxing for the `J`/`F`/`D` primitive targets.

## The fix

Landed incidentally as part of an unrelated commit,
**`102b33b65` "Fix Spring Integration JMX and scheduler residuals"**
(2026-07-18), which reworked `unbox_wrapper`:

```rust
// Before:
('J', Value::Object(Some(b))) => shared.heap.get_field(b, 0),   // wrong: stays Value::Int
('F', Value::Object(Some(b))) => shared.heap.get_field(b, 0),
('D', Value::Object(Some(b))) => shared.heap.get_field(b, 0),

// After:
('J' | 'F' | 'D', Value::Object(Some(b))) => {
    widen_unboxed_primitive(prim_char, shared.heap.get_field(b, 0))
}
```

with a new `widen_unboxed_primitive` helper performing the JLS-permitted
unbox-then-widen conversions (`Integer→long/float/double`,
`Long→float/double`, `Float→double`). That commit's own subject/tests did
not mention Pulsar or `AutoClusterFailoverBuilder` at all — the fix was
general (any lambda/method-reference dispatch unboxing an `Integer`/`Long`/
`Float` into a wider primitive impl parameter), so it fixed this symptom as
a side effect without anyone cross-referencing this doc at the time.

## Verification (2026-07-21)

Ran the entire `module/spring-boot-pulsar` test suite (6 classes, 117 tests)
against a fresh worktree build of current `dev` (which already contains
`102b33b65`), both under `--nojit` and with JIT enabled:

| Class | Tests | Result |
|---|---|---|
| `PulsarPropertiesMapperTests` | 14 | **14/14 pass** (both modes) — the originally-failing `customizeClientBuilderWhenHasFailover` now passes |
| `PulsarAutoConfigurationTests` | 74 (2 skipped, JRE-range gated) | 74/74 pass |
| `PulsarPropertiesTests` | 24 | 24/24 pass |
| `PulsarContainerFactoryCustomizersTests` | 3 | 3/3 pass |
| `DeadLetterPolicyMapperTests` | 2 | 2/2 pass |
| `PropertiesPulsarConnectionDetailsTests` | 2 | 2/2 pass |

No residuals found — no other test in the module, and no other known-issues
doc referencing `coerce_lambda_args`/`unbox_wrapper`/lambda argument
coercion, is open. Since the fix is general (lives in the shared unboxing
helper used by every lambda/method-reference dispatch, not anything
Pulsar-specific), no further code change was required this session; this
doc is closed purely as a documentation/triage update.

## Affected classes (original)

| Module | Class |
|---|---|
| `module/spring-boot-pulsar` | `org.springframework.boot.pulsar.autoconfigure.PulsarPropertiesMapperTests` (1 of 14 tests) |
