# `GraphQlRSocketAutoConfigurationTests` RSocket-over-TCP blocking-read timeout

**Status: RESOLVED -- 2026-07-28 (stale, non-reproducible issue)**

## Resolution

No new VM source change was required. The original 2026-07-23 report was
explicitly low-confidence and had not been isolated. It was superseded by
already-integrated GraphQL package/resource fixes and the Netty RSocket
wildcard-listener loopback fix. A fresh standard release build of current
`dev` (commit `16fe5ef60`) was used to rerun the exact TCP test repeatedly and
the complete affected class in both execution modes.

The stale known-issue document is therefore retired rather than left open as a
possible networking defect. This closure does not claim a precise cause for the
single historical timeout: the original run captured neither a thread dump nor
a transport trace. It establishes that no current RSocket/TCP residual remains
in the affected class.

## Validation

The executable was built from an isolated worktree using the normal fat-LTO
`release` profile, copied under a unique run name, and SHA-256 verified before
testing:

```
F5100AB96A6D5423DB930FAEFC6FF7DDB164B14158D9593774F12F81EA9BB17B
```

All tests used the real Spring Boot fixture, real Netty RSocket TCP listeners,
and a fresh VM process per invocation.

| Scope | Mode | Result | Elapsed |
|---|---|---:|---:|
| `simpleQueryShouldWorkWithTcpServer` repetition 1 | JIT | 1/1 | 36.2 s |
| `simpleQueryShouldWorkWithTcpServer` repetition 2 | JIT | 1/1 | 29.5 s |
| `simpleQueryShouldWorkWithTcpServer` repetition 3 | JIT | 1/1 | 25.3 s |
| `simpleQueryShouldWorkWithTcpServer` repetition 1 | `--nojit` | 1/1 | 16.3 s |
| `simpleQueryShouldWorkWithTcpServer` repetition 2 | `--nojit` | 1/1 | 18.5 s |
| `simpleQueryShouldWorkWithTcpServer` repetition 3 | `--nojit` | 1/1 | 17.4 s |
| `GraphQlRSocketAutoConfigurationTests` complete class | JIT | 6/6 | 143.3 s |
| `GraphQlRSocketAutoConfigurationTests` complete class | `--nojit` | 6/6 | 79.4 s |

Every TCP-method repetition logged `Netty RSocket started on port ...`, and
every invocation reported zero failed, aborted, and skipped tests.

## Historical symptom

The original runner observed one failure of the six-test class:

```
simpleQueryShouldWorkWithTcpServer()
java.lang.IllegalStateException: Timeout on blocking read for 5000000000 NANOSECONDS
```

It had a five-second assertion timeout while the surrounding class was already
slow on a contended host. The current repeated isolated probes complete the
same real loopback request within the assertion window in both modes.

## Affected class

| Module | Class | Final result |
|---|---|---:|
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSocketAutoConfigurationTests` | 6/6 JIT, 6/6 `--nojit` |
