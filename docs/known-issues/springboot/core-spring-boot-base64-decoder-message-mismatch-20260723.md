# Base64 decode error message text doesn't match real JDK ("Invalid base64 char" vs "Illegal base64")

**Status: OPEN — found 2026-07-23**

## Symptom

```
java.lang.AssertionError:
Expecting throwable message:
  "Invalid base64 char:  "
to contain:
  "Illegal base64"
but did not.

Throwable that failed the check:

java.lang.IllegalArgumentException: Invalid base64 char:
	at org.springframework.boot.io.Base64ProtocolResolver.decode(Base64ProtocolResolver.java:47)
	at org.springframework.boot.io.Base64ProtocolResolver.resolve(Base64ProtocolResolver.java:41)
```

`Base64ProtocolResolverTests.base64LocationWithInvalidBase64ThrowsException()`
and one of `JksSslStoreBundleTests`'s three failures
(`invalidBase64EncodedLocationThrowsException()`) both assert that a
malformed base64 payload throws `IllegalArgumentException` with a message
containing `"Illegal base64"` (the real `java.util.Base64.Decoder`'s wording)
— but CratonVM's decoder throws a different message text.

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard8/logs/core_spring-boot.org.springframework.boot.io.Base64ProtocolResolverTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard8/logs/core_spring-boot.org.springframework.boot.ssl.jks.JksSslStoreBundleTests.out.log`

## Root cause

Confirmed at file:line. CratonVM ships a custom `java.util.Base64` decode
implementation in `native-builtins/src/lib.rs`, whose error path formats:

```rust
// native-builtins/src/lib.rs:63942,63944,63959,63976
.ok_or_else(|| format!("Invalid base64 char: {}", filtered[i] as char))?;
```

Real `java.util.Base64.Decoder` (`decode0`/`decodeBlock`) throws
`IllegalArgumentException("Illegal base64 character " +
Integer.toString(sl[si] & 0xff, 16))` — a different wording ("Illegal base64
character <hex>" vs CratonVM's "Invalid base64 char: <literal char>"). Any
test asserting on that exact substring — as both of these do — sees a
message mismatch even though the *behavior* (rejecting the malformed input)
is otherwise correct.

`JksSslStoreBundleTests` has two other, unrelated failures in the same run
(`whenHasKeyStoreProvider`, `whenHasTrustStoreProvider`) — see
`core-spring-boot-keystore-provider-name-swallowed-20260723.md` for those.

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.io.Base64ProtocolResolverTests |
| core/spring-boot | org.springframework.boot.ssl.jks.JksSslStoreBundleTests (1 of 3 failures: `invalidBase64EncodedLocationThrowsException`) |
