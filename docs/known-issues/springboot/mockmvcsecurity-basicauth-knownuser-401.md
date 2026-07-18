# `MockMvcSecurityIntegrationTests`: real Basic-Auth against a known user returns 401 instead of 200

**Status: OPEN — found 2026-07-17**

## Symptom

| Module | Class | Wall time |
|---|---|---:|
| `module/spring-boot-security-test` | `MockMvcSecurityIntegrationTests` | 93.8s (1/4 fail) |

```
JUnit Jupiter:MockMvcSecurityIntegrationTests:okResponseWithBasicAuthCredentialsForKnownUser()
  => org.opentest4j.AssertionFailedError: [HTTP status code]
expected: 200
 but was: 401
       org.springframework.test.web.servlet.assertj.AbstractHttpServletResponseAssert.hasStatus(AbstractHttpServletResponseAssert.java:185)
       org.springframework.boot.security.test.autoconfigure.webmvc.MockMvcSecurityIntegrationTests.okResponseWithBasicAuthCredentialsForKnownUser(MockMvcSecurityIntegrationTests.java:67)
```

Response detail from the log confirms Spring Security's own
`BasicAuthenticationFilter` rejected the credentials (standard
`WWW-Authenticate: Basic realm="Realm"` 401, not a routing/mapping error):

```
MockHttpServletResponse:
           Status = 401
    Error message = Unauthorized
          Headers = [WWW-Authenticate:"Basic realm="Realm", charset="UTF-8"", ...]
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-security-test.org.springframework.boot.security.test.autoconfigure.webmvc.M-8f04c02d354e.out.log`

The other 3 tests in the class pass, including
`okResponseWithMockUser()` (which uses `@WithMockUser`, a Spring Security
Test annotation that injects a pre-authenticated `Authentication` directly
into the `SecurityContext`, bypassing real credential verification
entirely) and `unauthorizedResponseWithNoUser()` (which expects 401 and
gets it). Only the one test that sends a **real** HTTP Basic
`Authorization` header and expects it to be accepted fails:

```java
@Test
void okResponseWithBasicAuthCredentialsForKnownUser() {
	assertThat(this.mvc.get()
		.uri("/")
		.header(HttpHeaders.AUTHORIZATION, "Basic " + Base64.getEncoder().encodeToString("user:secret".getBytes())))
		.hasStatusOk();
}
```
(`MockMvcSecurityIntegrationTests.java:62-68`)

## Root cause

**Not confirmed — hypothesis.** The pass/fail split (mock-authenticated
request passes, no-credentials request correctly gets 401, but a
*genuine* username/password pair that Spring Security's default
autoconfigured user store is supposed to accept gets rejected) isolates
the failure to Spring Security's real credential-verification path —
`DaoAuthenticationProvider`/`InMemoryUserDetailsManager` comparing the
supplied password ("secret") against the stored, encoded password via
`PasswordEncoder.matches(...)`. Spring Security's default encoder is
`DelegatingPasswordEncoder` wrapping `BCryptPasswordEncoder` for
`{bcrypt}`-prefixed stored hashes. `spring-security-crypto`'s BCrypt
implementation is pure Java (no JNI/native calls in the upstream library);
a targeted search of `native-builtins/src` for `BCrypt`/`bcrypt` found only
unrelated matches (Windows `BCryptGenRandom`/CNG API bindings used for
`SecureRandom`, not Spring Security's password hashing) — so no native
CratonVM intrinsic gap was found to explain a wrong `matches()` result.
Plausible candidates not yet checked: (a) the default test password itself
not being generated/configured as expected under CratonVM (e.g. if this
module's test config relies on `SecurityProperties.User` being
autoconfigured with a specific value and something in that property-binding
path differs), or (b) a real difference somewhere in the Base64-decode /
byte-comparison chain the `Authorization` header goes through before
reaching the password check. Neither confirmed this round — no existing
doc for this signature was found (searched
`docs/known-issues/springboot`, `docs/internal/springboot`,
`docs/internal/fixed-suite-bugs` for `bcrypt`/`BCrypt`, no hits).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security-test` | `org.springframework.boot.security.test.autoconfigure.webmvc.MockMvcSecurityIntegrationTests` |
