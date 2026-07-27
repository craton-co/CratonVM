# `http_parse_url` lost its query-only-path fix in a silent merge — `BasicErrorControllerIntegrationTests` "bad port" failures are back

**Status: OPEN — found 2026-07-23 (confirmed at source level; this is a regression of an already-"FIXED" doc, not a new bug)**

## Symptom

| Module | Class | Failures |
|---|---|---:|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` | 5/26 |

```
JUnit Jupiter:BasicErrorControllerIntegrationTests:testErrorForMachineClientAlwaysParams()
    => org.springframework.web.client.ResourceAccessException: I/O error on GET request for "http://localhost:51281": HttpClient request failed: bad port in http://localhost:51281?trace=false&message=false
     Caused by: java.io.IOException: HttpClient request failed: bad port in http://localhost:51281?trace=false&message=false
       org.springframework.http.client.JdkClientHttpRequest.executeInternal(JdkClientHttpRequest.java:118)
       ...
```

All 5 failures are the same shape: `TestRestTemplate`/`JdkClientHttpRequest` builds
a query-only request target (`http://host:port?trace=...&message=...`, no `/`
before the `?`), and CratonVM's native HTTP client rejects it with `bad port in
<url>` — the literal text `51281?trace=false&message=false` is being fed to a
`u16` parser as if it were all port digits.

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard4/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.error.BasicErrorContro-957a1d0f4289.out.log`

## This is a confirmed regression, not a new bug

This **exact** symptom, on this **exact** class, was already root-caused and
marked fixed in
[`../../internal/fixed-suite-bugs/springboot/webmvc-error-forward-and-multiboot-timeout-cluster-FIXED.md`](../../internal/fixed-suite-bugs/springboot/webmvc-error-forward-and-multiboot-timeout-cluster-FIXED.md)
("Cluster B"), landed in commit `7a27b20ff` (2026-07-19) and verified
`BasicErrorControllerIntegrationTests` 26/26 PASS. That commit changed
`http_parse_url` (`native-builtins/src/net_phase_e.rs`) to split the URL's
authority from its path on `/`, `?`, **or** `#`, instead of `/` alone:

```rust
// 7a27b20ff (2026-07-19) — the fix
let (authority, path) = match rest.find(|c| matches!(c, '/' | '?' | '#')) {
    Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
    Some(i) if rest.as_bytes()[i] == b'?' => (&rest[..i], format!("/{}", &rest[i..])),
    Some(i) => (&rest[..i], "/".to_string()),
    None => (rest, "/".to_string()),
};
```

**That code is no longer present.** The current `http_parse_url` (verified at
both this worktree's HEAD `a3d75f295` and `origin/dev`'s current tip
`fa1b0ade0`, 2026-07-27) is back to the pre-fix, slash-only split:

```rust
// native-builtins/src/net_phase_e.rs:4621 (current, both this worktree and origin/dev)
let (authority, path) = match rest.find('/') {
    Some(i) => (&rest[..i], &rest[i..]),
    None => (rest, "/"),
};
```

## Root cause of the regression — a silent merge, not a revert

Two independent branches modified `http_parse_url` around the same period:

- `7a27b20ff` (2026-07-19, `codex/fix-webmvc-error-timeout-20260718-019f768e`) —
  the `?`/`#` query-only-path fix described above.
- `b0dd2e726` (2026-07-07, `Roadmap Orchestrator`, "Fix ResourceTests HTTP URL
  failures: user-info authority, EAGAIN read, HEAD content-length") — added
  `userinfo` parsing (splitting `alice:secret@host:port` authorities) and
  changed the function's return type from a 4-tuple to a 5-tuple. This commit
  was authored **before** `7a27b20ff` (2026-07-07 vs. 2026-07-19) but its
  branch wasn't merged into `dev` until *after* — `git log -S "match
  rest.find('/')"` shows only these two commits (plus the two independent
  "Open-source initial commit" roots) ever toggling this exact line, and
  `7a27b20ff` is **not** an ancestor of `b0dd2e726`.

When `b0dd2e726`'s branch was eventually merged into `dev`, it carried its own
complete rewrite of `http_parse_url` — based on the **old**, pre-`7a27b20ff`
version of the function (slash-only split) — and that rewrite is what ended up
on `dev`. The merge wasn't a textual conflict (the two commits touched
different *aspects* of the same few lines: one added `'?'`/`'#'` to the
`find()` pattern, the other inserted `userinfo` splitting and widened the
tuple), so nothing forced a human to reconcile them; whichever side "won" the
merge simply carried the userinfo feature forward and dropped the query-path
fix. This is the same "silent merge landmine" pattern noted elsewhere in this
project's history — two branches independently touching the same function,
merged without a real conflict, one fix silently lost.

The current function (with `userinfo` intact, `?`/`#` handling gone):

```rust
fn http_parse_url(url: &str) -> Result<(bool, String, u16, String, Option<String>), String> {
    ...
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (Some(authority[..i].to_string()), &authority[i + 1..]),
        None => (None, authority),
    };
    let (host, port) = match hostport.rfind(':') {
        Some(i) => {
            let (h, p) = (&hostport[..i], &hostport[i + 1..]);
            let pn: u16 = p.parse().map_err(|_| format!("bad port in {url}"))?;
            (h.to_string(), pn)
        }
        None => (hostport.to_string(), if scheme { 443 } else { 80 }),
    };
    Ok((scheme, host, port, path.to_string(), userinfo))
}
```

## Fix direction

Re-apply `7a27b20ff`'s `'/' | '?' | '#'` split logic on top of the current
(userinfo-aware) function — the two changes are orthogonal (one splits
authority-from-path, the other splits userinfo-from-host-within-the-authority)
and should compose without conflict. Also worth grepping the rest of
`net_phase_e.rs` / `http_url_connection.rs` for any other `rest.find('/')`
occurrence that should have received the same `?`/`#` treatment but didn't
(this file has several structurally-similar URL-splitting call sites for
jar:/redirect handling).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` |

Likely affects any other Spring Boot test using `TestRestTemplate`/
`JdkClientHttpRequest` against a query-only path (no explicit `/`) — this doc
only confirms the one class hit in the 2026-07-23 rerun batch.
