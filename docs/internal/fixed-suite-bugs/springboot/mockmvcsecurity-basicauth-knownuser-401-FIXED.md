# `MockMvcSecurityIntegrationTests` real Basic-auth known-user 401 — FIXED 2026-07-18

## Symptom

`module/spring-boot-security-test` previously reported one failing method in
`MockMvcSecurityIntegrationTests`:

```text
okResponseWithBasicAuthCredentialsForKnownUser()
expected: 200
 but was: 401
```

The sibling `@WithMockUser` test passed, and the missing-credentials test
correctly received 401. The failing method is the real credential path: it
sends `Basic base64(user:secret)` and requires Spring Security to verify the
default user's BCrypt password.

## Root cause and correction

This was not a separate Base64, HTTP-header, or MockMvc mapping defect. It
uses the same Spring Security BCrypt verifier path that was repaired by
commit `90104787b49233fa20be9ab3aa3541ed09e62141` (`Fix Spring Security
BCrypt CLI stall`), now contained in `dev`.

`native-builtins/src/phases_late.rs` registers an intrinsic for Spring
Security's private `BCrypt.crypt_raw([B[BIZIZ)[B` key-schedule loop. Spring
continues to perform salt parsing, revision handling, encoded-password
formatting, and comparison in its own bytecode; only the repeated BCrypt
schedule uses the shared native implementation. The historical `$2x$`
compatibility mode explicitly retains the original bytecode implementation.

The prior report was left open because it was written before this correction
was validated against the actual MockMvc Security class. No additional VM
change is needed.

## Focused verification

The current `origin/dev` source was built in an isolated worktree as
`cratonvm-mockmvcsecurity-basicauth-20260718-019f753a.exe`. The Spring Boot
fixture's compiled `module/spring-boot-security-test` runtime classpath and
`SbRunner` were used to run only
`org.springframework.boot.security.test.autoconfigure.webmvc.MockMvcSecurityIntegrationTests`.

| Mode | Result | Time | Tests |
|---|---:|---:|---:|
| CratonVM JIT | PASS | 44.571s | 4/4, 0 failed |
| CratonVM `--nojit` | PASS | 45.637s | 4/4, 0 failed |

Both runs include `okResponseWithBasicAuthCredentialsForKnownUser()` and
therefore prove the original real Basic-auth request now returns 200. The
runner recorded zero failed or aborted containers in each mode.

The shared root correction is also documented in
`docs/internal/springboot/encodepasswordcommandtests-cli-hang-FIXED.md`.
