# `RedirectUtilsTest` — a redirect URI containing a literal space is accepted instead of rejected

Status: fixed — `URI.create(String)` now rejects malformed raw-space URIs consistently with the `URI(String)` constructor.
## Root cause

`RedirectUtils.toUri` uses `URI.create(redirectUri)`, not the public `URI(String)` constructor. CratonVM's
native `URI.create(String)` constructed a synthetic URI with `make_uri` without applying the constructor's
strict character checks. A literal space therefore reached Keycloak's wildcard matcher and was accepted.

## Resolution

`native-builtins/src/net_phase_e.rs` now applies the same malformed-scheme, illegal-character, and
empty-scheme-specific-part checks in `URI.create(String)`, translating failures to
`IllegalArgumentException` as the JDK factory does. Valid inputs still use `make_uri`, preserving the
synthetic URI field layout required by Keycloak's loopback and custom-scheme redirect checks.

## Validation

On Azure from `dev` base `4d9db567`, a fresh release binary named
`cratonvm-keycloak-redirect-uri-space-20260715` passed the compiled Keycloak 26.6.1
`RedirectUtilsTest` class in both modes: HotSpot 10/10, CratonVM JIT 10/10, and CratonVM no-JIT 10/10.
The final CratonVM result files are under
`/data/data/tmp-keycloak-redirect-uri-space-20260715/suite/results/redirecturi-space-craton-*-final-20260715/`.

Date observed: 2026-07-14 (4-shard `nonpassed-v3` rerun against `others.tsv`, branch fix/keycloak-nonpassed-rerun-v2-20260710, binary `cratonvm-nonpassed-v3-refresh-20260714.exe`)

## Original failure

`services :: org.keycloak.protocol.oidc.utils.RedirectUtilsTest::testverifyInvalidRedirectUri` fails:

```
=> java.lang.AssertionError: expected null, but was:<https://keycloak.org/path space/>
   org.keycloak.protocol.oidc.utils.RedirectUtilsTest.testverifyInvalidRedirectUri(RedirectUtilsTest.java:183)
```

The test feeds `RedirectUtils` a redirect URI whose path contains a literal, unencoded space
(`https://keycloak.org/path space/`) and expects the verification helper to reject it (returning `null`, since
this is `testverifyInvalidRedirectUri` — testing the *invalid* case). CratonVM returns the URI unchanged instead
of rejecting it.

## Original hypothesis

A raw space character is not a legal character in a URI per RFC 3986 — real Java's `java.net.URI`/`URL`
constructors and validators normally reject or reformat such input (throwing `URISyntaxException` for the
strict `URI` constructor, or otherwise flagging it as malformed). If `RedirectUtils`'s validation path relies on
constructing a `URI`/`URL` object from the candidate string and catching a parse exception to decide validity,
this suggests CratonVM's URI/URL parsing is more lenient than real Java's — silently accepting a raw space
where HotSpot would throw, so the "is this a well-formed URI" check that should fail here instead succeeds and
the (invalid) URI is returned unmodified.

This is consistent with a broader pattern of validation-leniency divergences also seen in
`stax-parser-malformed-xml-not-rejected.md` and `test-classserver-invalidpackage-classnotfound-not-thrown.md`
from earlier in this project's investigation — CratonVM tends to accept malformed input in several distinct
parsers/validators (XML, URI, class-name resolution) where real Java rejects it. Each has a different underlying
component, but the pattern (validation code paths being too permissive) is worth keeping in mind as a category.

## Original next steps

1. Find `RedirectUtils`'s verification method (`services` module, `org.keycloak.protocol.oidc.utils.RedirectUtils`)
   and identify exactly which URI/URL API call is supposed to catch this malformed input.
2. Minimal repro: `new java.net.URI("https://keycloak.org/path space/")` — confirm whether CratonVM throws
   `URISyntaxException` here (if the bug is at the `URI` constructor level) or accepts it silently, compared to
   real HotSpot which should throw.
3. Re-verify against the full `RedirectUtilsTest` class once fixed (10/11 tests already pass, so this is narrow).

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-redirecturi-space -ClassList <(printf 'module\tclass\nservices\torg.keycloak.protocol.oidc.utils.RedirectUtilsTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v3-refresh-20260714.exe -JdkHome $jdk
```

## Evidence

`apps/keycloak-suite-runner/.suite/results/nonpassed-v3-shard1/all-jit/logs/services.org.keycloak.protocol.oidc.utils.RedirectUtilsTest.out.log`,
2026-07-14 rerun with binary `cratonvm-nonpassed-v3-refresh-20260714.exe` built from `dev` at commit `e85f76d00`.
