# `String.join(CharSequence, CharSequence...)` drops non-`String` `CharSequence` elements — collapses Spring Boot's hand-built `docker-compose` regexes to near-empty, so every `Pattern.matches()` fails

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | Failures |
|---|---:|
| `org.springframework.boot.docker.compose.core.DefaultDockerComposeTests` | 3/9 |
| `org.springframework.boot.docker.compose.core.DefaultRunningServiceTests` | 9/9 |
| `org.springframework.boot.docker.compose.core.ImageNameTests` | 12/17 |
| `org.springframework.boot.docker.compose.core.ImageReferenceTests` | 13/15 |
| `org.springframework.boot.docker.compose.service.connection.ConnectionNamePredicateTests` | 5/5 |

Every failure is the same `IllegalArgumentException` from `Assert.isTrue`,
thrown for image-reference strings that are trivially valid ("ubuntu",
"redis", "localhost/ubuntu", "index.docker.io/ubuntu",
"repo.example.com:8080/library/ubuntu:v1", "openzipkin/zipkin", …) — i.e.
**every** input fails to match, not just malformed ones:

```
JUnit Jupiter:ImageNameTests:ofWhenNameOnlyCreatesImageName()
  => java.lang.IllegalArgumentException: 'value' path must contain an image reference in the form '[domainHost:port/][path/]name' (with 'path' and 'name' containing only [a-z0-9][.][_][-]) [ubuntu]
     org.springframework.util.Assert.isTrue(Assert.java:136)
     org.springframework.boot.docker.compose.core.ImageName.of(ImageName.java:122)
     org.springframework.boot.docker.compose.core.ImageNameTests.ofWhenNameOnlyCreatesImageName(ImageNameTests.java:34)
```

```
JUnit Jupiter:ConnectionNamePredicateTests:organization()
  => java.lang.IllegalArgumentException: 'value' path must contain an image reference in the form '[domainHost:port/][path/]name[:tag][@digest] (with 'path' and 'name' containing only [a-z0-9][.][_][-]) [openzipkin/zipkin]
     org.springframework.util.Assert.isTrue(Assert.java:136)
     org.springframework.boot.docker.compose.core.ImageReference.of(ImageReference.java:166)
     org.springframework.boot.docker.compose.service.connection.ConnectionNamePredicate.asCanonicalName(ConnectionNamePredicate.java:55)
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-docker-compose.org.springframework.boot.docker.compose.core.DefaultDockerComposeTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-docker-compose.org.springframework.boot.docker.compose.core.ImageNameTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-docker-compose.org.springframework.boot.docker.compose.core.ImageReferenceTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-docker-compose.org.springframework.boot.docker.compose.service.connection.Con-6e9f828a8bac.out.log`

## Root cause (confirmed against source)

Spring Boot's internal `org.springframework.boot.docker.compose.core.Regex`
class (`apps/spring-boot/core/spring-boot-docker-compose/src/main/java/.../Regex.java`)
is `final class Regex implements CharSequence` — it builds
`Pattern.PATH`/`Pattern.DOMAIN`/etc. at class-init time by *programmatically
composing* regex fragments via helper methods (`Regex.of(...)`,
`Regex.group(...)`, `Regex.oneOf(...)`) that all bottom out in
`String.join("", expressions)`, where `expressions` is a
`CharSequence...` varargs array whose elements are frequently **other,
previously-built `Regex` instances**, not string literals — e.g.
`Regex.group(component, dotComponent.oneOrMoreTimes())`,
`PATH_COMPONENT = Regex.of(segment, Regex.group(separatedSegment).zeroOrOnce())`.

CratonVM's native for the varargs-array overload of `String.join` —
`native_string_join`, `native-builtins/src/lang_string.rs:3868-3891`,
registered for descriptor
`(Ljava/lang/CharSequence;[Ljava/lang/CharSequence;)Ljava/lang/String;` in
`native-builtins/src/lib.rs` — reads each array element like this:

```rust
for i in 0..len {
    if let Value::Object(Some(elem)) = ctx.get_array_element(arr, i) {
        parts.push(ctx.read_string(elem).unwrap_or_default());
    } else {
        parts.push("null".to_string());
    }
}
```

`ctx.read_string` (`vm/src/vm/vm_exec.rs:4853-4879`) has a deliberate safety
guard: if the element's runtime class is known and isn't literally
`java/lang/String`, it returns `None` (added to stop `String.join` from
mis-reading synthetic `StringBuilder`/`StringBuffer` internal layouts as
string data). `Regex` is a real, distinct, non-`String` class implementing
`CharSequence` — so every time a previously-built `Regex` object is passed
into `String.join`, `read_string` returns `None`, `.unwrap_or_default()`
silently substitutes `""`, and that fragment is dropped from the joined
result instead of using its `toString()`.

The sibling `native_string_join_iterable` (same file, lines 3893-3934,
backs the `String.join(CharSequence, Iterable<? extends CharSequence>)`
overload) does this correctly — it calls `invoke_to_string(ctx, obj)` for
each element, real polymorphic dispatch that works for any `CharSequence`.
Only the varargs-array overload has the asymmetric bug.

**Cascading effect:** `PATH_COMPONENT` and `PATH`'s construction nests
several `Regex.group(...)`/`Regex.of(...)` calls whose arguments are other
`Regex` objects. Each such call collapses to `""` on CratonVM, and the empty
value keeps propagating through further concatenation, `oneOrMoreTimes()`
(`value + "+"`), `zeroOrOnce()` (`value + "?"`), and more `String.join`
calls. The final `PATH.value` ends up drastically truncated — plausibly
reduced to something that only matches the empty string once wrapped in
`Pattern.compile("^" + value + "$")` — which is exactly consistent with
*every* non-empty test input failing to match while the regex is
syntactically well-formed (no `PatternSyntaxException` is ever thrown, just
`Assert.isTrue` failing). `Pattern`/`Matcher` (`native-builtins/src/lib.rs`,
`java.util.regex` backed by the `regex`/`fancy-regex` crates) are not
implicated — the corrupted pattern string reaches them already wrong.

This is a native-bridge (`String.join` argument marshalling) bug, not a
regex-engine defect, and not specific to `docker-compose` — it will
mis-join **any** `CharSequence[]` varargs call where an element is a
custom, non-`String` `CharSequence` implementation. `docker-compose` is
simply the one Spring Boot module in this suite whose production code does
that (most other callers of `String.join` pass literal `String`s).

## Suggested fix direction

`native_string_join` should fall back to real polymorphic `toString()`
dispatch (matching `native_string_join_iterable`'s existing correct
behavior) whenever `read_string` returns `None` for a non-null array
element, instead of substituting `""`.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-docker-compose` | `org.springframework.boot.docker.compose.core.DefaultDockerComposeTests` |
| `core/spring-boot-docker-compose` | `org.springframework.boot.docker.compose.core.DefaultRunningServiceTests` |
| `core/spring-boot-docker-compose` | `org.springframework.boot.docker.compose.core.ImageNameTests` |
| `core/spring-boot-docker-compose` | `org.springframework.boot.docker.compose.core.ImageReferenceTests` |
| `core/spring-boot-docker-compose` | `org.springframework.boot.docker.compose.service.connection.ConnectionNamePredicateTests` |
