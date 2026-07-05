# `ResourceTests` remaining HTTP URL failures

Status: OPEN as of 2026-07-05.

## Context

The residual classpath/Mockito/URL/FileSystemResource issues from
`resourcetests-residual-classpath-mockito-url-httpconn-bugs.md` are fixed and
that document has moved to `docs/internal`. The full Spring
`org.springframework.core.io.ResourceTests` class now runs at **65/68** under
CratonVM with `--jdk real --jit on`.

Verification command:

```bash
cd /data/cratonvm/apps/spring-suite-runner
CRATONVM_BIN=/data/cratonvm-resourcetests-residuals-20260705-13506 ./run-suite.sh run --jdk real --jit on --batch 1 --batch-to 600 --one-to 240 --tag res-residuals-13506-urlfix --only 'org\.springframework\.core\.io\.ResourceTests$'
```

Run output:

```text
test-methods: found=68 passed=65 failed=3
```

## Remaining failures

```text
FAILCAUSE org.springframework.core.io.ResourceTests :: canCustomizeHttpUrlConnectionForRead() :: java.io.IOException: URL.openStream failed: Resource temporarily unavailable (os error 11)
FAILCAUSE org.springframework.core.io.ResourceTests :: useUserInfoToSetBasicAuth() :: java.io.IOException: URL.openStream: unsupported scheme: alice:secret@localhost:<port>
FAILCAUSE org.springframework.core.io.ResourceTests :: remoteResourceExists() :: org.opentest4j.AssertionFailedError: Expecting value to be true but was false
```

Notes:

- `canCustomizeHttpUrlConnectionForRead()` is separate from the already-fixed
  `canCustomizeHttpUrlConnectionForExists*` cases. The read path still reaches
  `URL.openStream()` and gets an EAGAIN-style native I/O failure.
- `useUserInfoToSetBasicAuth()` appears to parse `alice:secret@localhost:<port>`
  as an unsupported URL scheme instead of preserving it as user-info authority
  for the HTTP URL.
- `remoteResourceExists()` now stands alone as a false `exists()` result for a
  live remote resource.

Suggested next pass: focus on `java.net.URL`/`HttpURLConnection` handling for
HTTP URLs with user-info, `openStream()` nonblocking/EAGAIN behavior, and the
`UrlResource.exists()` remote-resource path.
