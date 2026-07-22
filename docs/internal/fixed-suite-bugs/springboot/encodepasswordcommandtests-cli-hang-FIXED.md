# `EncodePasswordCommandTests` BCrypt verifier stall — FIXED 2026-07-18

## Symptom

`cli/spring-boot-cli`'s `EncodePasswordCommandTests` was reported as a
per-class HANG after roughly four minutes. HotSpot completed the same five
tests in seconds.

The original report incorrectly associated the visible `Unknown algorithm`
message with the stall. A diagnostic CratonVM run with
`--stack-dump-on-timeout=90` showed that the main thread was still executing
the first test's Mockito assertion. Its active frames ended at:

```
EncodePasswordCommandTests.encodeWithNoAlgorithmShouldUseBcrypt
  -> BCryptPasswordEncoder.matchesNonNull
  -> BCrypt.hashpwforcheck
  -> BCrypt.crypt_raw
  -> BCrypt.key
  -> BCrypt.encipher
```

## Root cause

Spring Security's `org.springframework.security.crypto.bcrypt.BCrypt` keeps
the BCrypt key schedule in Java bytecode. At the ordinary cost factor of 10,
the repeated `encipher` calls are too expensive in CratonVM's interpreter to
finish within the suite timeout. The VM already had an equivalent native
intrinsic for Bouncy Castle's differently named BCrypt implementation, so
Spring Security never reached it.

## Fix

`native-builtins/src/phases_late.rs` now shares the existing native BCrypt
key-schedule implementation across Bouncy Castle and Spring Security's public
P/S tables. It registers an intrinsic only for Spring Security's private
`crypt_raw([B[BIZIZ)[B` work loop. Salt parsing, revisions, textual encoding,
and constant-time comparison remain in the library's original bytecode.

The obsolete `$2x$` sign-extension compatibility mode explicitly invokes the
original bytecode body so its historical behavior is unchanged.

## Validation

- HotSpot baseline: PASS in 4.4 seconds.
- CratonVM before the fix: watchdog-aborted after 90 seconds in
  `BCrypt.encipher`.
- CratonVM JIT: PASS in 3.7 seconds.
- CratonVM `--nojit`: PASS in 3.0 seconds.
- The fixed class covers default BCrypt, explicit BCrypt, PBKDF2, and the
  invalid-algorithm error path.

The Spring Boot fixture was checked through its actual compiled test/runtime
classpath before diagnosis; no stale source-only conclusion was used.
