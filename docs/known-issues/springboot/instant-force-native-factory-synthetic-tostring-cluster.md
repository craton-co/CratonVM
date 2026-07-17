# `Instant.now()` force-routed to a synthetic-stub factory produces a debug `toString()` (`"Instant(sec.nano)"`) instead of ISO-8601, breaking Jackson date serialization

**Status: OPEN — found 2026-07-17 (confirmed at source level)**

## Symptom

| Class | Method | tests failed/total |
|---|---|---:|
| `Jackson2EndpointAutoConfigurationTests` | `endpointObjectMapperDoesNotSerializeDatesAsTimestamps()` | 1/1 |
| `JacksonEndpointAutoConfigurationTests` | `endpointJsonMapperDoesNotSerializeDatesAsTimestamps()` | 1/1 |

```
JUnit Jupiter:Jackson2EndpointAutoConfigurationTests:endpointObjectMapperDoesNotSerializeDatesAsTimestamps()
  => java.lang.AssertionError:
Expecting actual:
  "{"timestamp":"Instant(1784318675.352389000)"}"
to contain:
  "2026-07-17T20:04:35.352389Z"
     org.springframework.boot.actuate.autoconfigure.endpoint.jackson.Jackson2EndpointAutoConfigurationTests.lambda$endpointObjectMapperDoesNotSerializeDatesAsTimestamps$0(Jackson2EndpointAutoConfigurationTests.java:87)

JUnit Jupiter:JacksonEndpointAutoConfigurationTests:endpointJsonMapperDoesNotSerializeDatesAsTimestamps()
  => java.lang.AssertionError:
Expecting actual:
  "{"timestamp":"Instant(1784318677.737632300)"}"
to contain:
  "2026-07-17T20:04:37.737632300Z"
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-actuator-autoconfigure.org.springframework.boot.actuate.autoconfigure.endpo-5f0d89fce1ac.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-actuator-autoconfigure.org.springframework.boot.actuate.autoconfigure.endpo-29dfbc007e30.out.log`

Both tests configure an `ObjectMapper`/`JsonMapper` with
`SerializationFeature.WRITE_DATES_AS_TIMESTAMPS` disabled and serialize a
payload containing `Instant.now()`, expecting Jackson's
`jackson-datatype-jsr310` `InstantSerializer` to produce the standard
ISO-8601 string (`"2026-07-17T20:04:35.352389Z"`). CratonVM instead produces
a debug-style dump: `"Instant(1784318675.352389000)"` — literally
`ClassSimpleName(epochSecond.nano)`.

## Root cause (confirmed against source)

`Instant.now()`/`ofEpochSecond(J)`/`ofEpochSecond(JJ)`/`ofEpochMilli(J)` are
**unconditionally force-routed to native "synthetic stub" dispatch**, even
in real-JDK mode, per `is_time_native_override`
(`vm/src/runtime/interpreter.rs:22793-22806`):

```rust
pub(crate) fn is_time_native_override(
    class_name: &str, method_name: &str, descriptor: &str,
) -> bool {
    class_name == "java/time/Instant"
        && matches!(
            (method_name, descriptor),
            ("now", "()Ljava/time/Instant;")
                | ("ofEpochSecond", "(J)Ljava/time/Instant;")
                | ("ofEpochSecond", "(JJ)Ljava/time/Instant;")
                | ("ofEpochMilli", "(J)Ljava/time/Instant;")
        )
}
```

This routes into `register_synthetic_instant_stub_natives`
(`native-builtins/src/lib.rs:77633-77724`, called unconditionally from
`native-builtins/src/lib.rs:38537-38540`), whose own comment explains why:
*"WildFly can load `java.time.Instant` through a bootstrap synthetic stub
even in real-JDK mode. The interpreter already force-routes the hot Instant
factories to native dispatch; make the bridge available in essentials too."*

The consequence: every `Instant` object created via these factories —
including the one Jackson serializes here, and any other real-JDK Spring
Boot code path that calls `Instant.now()` — is a **synthetic-stub-backed**
object, not a genuine real-JDK `Instant`. Its `toString()` is then also
served by the matching synthetic native
(`native_synthetic_instant_to_string`, `native-builtins/src/lib.rs:77591-77603`):

```rust
fn native_synthetic_instant_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (sec, nano) = synthetic_instant_parts(ctx, this);
    let s = if nano == 0 { format!("Instant({sec})") } else { format!("Instant({sec}.{nano:09})") };
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}
```

— a hand-rolled debug format, not the real ISO-8601 `DateTimeFormatter`
machinery HotSpot's `Instant.toString()` runs. This same object, being
synthetic-stub-flagged, correctly (by CratonVM's own SyntheticStub/real-JDK
precedence rules — see the comment pattern used identically for
`StringJoiner`/`EnumSet` at the same registration call site) dispatches
`toString()` to this native instead of yielding to real bytecode, because
the object genuinely isn't a real-JDK-backed instance — the force-routing
of the *factory* is what puts it on the wrong side of that precedence rule
in the first place, not a dispatch-precedence bug in `toString()` itself.

Any Spring Boot test (or production code path) in real-JDK mode that
serializes, formats, or otherwise stringifies an `Instant` obtained via
`now()`/`ofEpochSecond`/`ofEpochMilli` will show this divergence — these two
Jackson tests are simply the first in this batch to assert on the exact
string. `Instant`s constructed other ways (e.g. deserialized, or built via
`Instant.parse(...)`, which is not in the force-route list) are presumably
unaffected.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-actuator-autoconfigure` | `org.springframework.boot.actuate.autoconfigure.endpoint.jackson.Jackson2EndpointAutoConfigurationTests` |
| `module/spring-boot-actuator-autoconfigure` | `org.springframework.boot.actuate.autoconfigure.endpoint.jackson.JacksonEndpointAutoConfigurationTests` |
