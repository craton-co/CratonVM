# OAuth2 issuer-URI test flakes on a loaded host — read timeout on its own mock server

**Status:** 🟡 **OPEN**, characterised 2026-08-03. Not a CratonVM defect as far
as the evidence goes — recorded because it reads exactly like a JIT regression
and cost a session's worth of A/B before it was excluded.

## What you see

```
[1/1] jit OAuth2ResourceServerAutoConfigurationTests rc=1 FAIL 249s
      SBRUNNER_RESULT tests=52 failed=1 aborted=0 skipped=0 containersFailed=0
```

Always the same single test:

```
JUnit Jupiter:OAuth2ResourceServerAutoConfigurationTests:
  autoConfigurationShouldConfigureResourceServerUsingOAuthIssuerUri()
  => JwtDecoderInitializationException: Failed to lazily resolve the supplied JwtDecoder instance
  Caused by: IllegalArgumentException: Unable to resolve the Configuration with the
             provided Issuer of "http://localhost:<port>/test"
  Caused by: ResourceAccessException: I/O error on GET request for
             "http://localhost:<port>/test/.well-known/openid-configuration": Read timed out
  Caused by: java.net.SocketTimeoutException: Read timed out
```

The test stands up its own `MockWebServer` on localhost and has Spring Security
fetch the OIDC discovery document from it. The read times out — an in-process,
same-host HTTP round trip that the harness's default `RestTemplate` read timeout
does not wait out when the box is busy.

## Why it is not a JIT regression

Interleaved A/B, alternating binaries inside one window so both arms see the
same load (`/data/data/btout/interleave.summary`, Azure host at load 15–25):

| round | control (`dev` b8f585cdeb) | with the tail-call fix |
| --- | --- | --- |
| 1 | **FAIL** | PASS |
| 2 | **FAIL** | **FAIL** |
| 3 | PASS | **FAIL** |
| 4 | PASS | **FAIL** |

Same test, same `SocketTimeoutException`, on a binary that predates the change.
Across the whole day: control 7 PASS / 2 FAIL, fixed 5 PASS / 4 FAIL over nine
runs each — a gap far inside the noise for a network-timeout flake at that n,
and the control's clean 5/5 block happened to land in a quieter window.

`--nojit` passed 3/3, which excludes nothing at this rate; the interleave is the
evidence, not the `--nojit` run.

## How to not lose a session to it

* **Run the arms INTERLEAVED, never back-to-back in separate windows.** Two
  consecutive 5-run blocks on a shared host compare load, not binaries — the
  control's block was quiet and scored 5/5, which is what made this look real.
  See [[reference_shared_host_load_invalidates_springboot_runs]].
* A `failed=1` on this class with `tests=52` is this flake until the stack trace
  says otherwise. Grep the log for `SocketTimeoutException` before anything else.
* The other 51 tests in the class pass throughout; a real regression here would
  not be this selective.

## If it needs fixing

The timeout is the harness's, not CratonVM's: the test's `RestTemplate` uses the
Spring default read timeout. Either raise it for the suite runner or pin the
class to a quiescent host. Neither belongs in the VM.
