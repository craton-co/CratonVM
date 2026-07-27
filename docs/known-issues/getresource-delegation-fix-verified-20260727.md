# `Class.getResourceAsStream`/`getResource` classloader-delegation fix — rebuild + functional verification (2026-07-27)

`ceea4eb05` (`fix(classloading): delegate Class.getResourceAsStream/getResource to the
real defining ClassLoader`) previously landed on `dev` but had not yet been
rebuilt and functionally re-verified from this worktree. Closed out here.

## Rebuild

`cargo build --release -p cratonvm-cli --bin cratonvm` from
`wt-jitban-remaining-20260726` (post-merge with current `dev`, which includes
`ceea4eb05`) completes cleanly in ~3m47s, binary produced at
`target/release/cratonvm`.

## Functional verification

`docs/known-issues/repros/getresource-delegation-verify-20260727/`:

- `ResourceHolder.java` (`package probe.lib;`) — a class whose only resource
  (`data.txt`, packaged alongside it) is read via
  `ResourceHolder.class.getResourceAsStream("data.txt")` /
  `.getResource("data.txt")`.
- `probe-lib.jar` — `ResourceHolder.class` + `data.txt`, NOT placed on the
  main app classpath.
- `Driver.java` — on the main classpath. Creates an **isolated**
  `URLClassLoader` (`parent = null`) pointing only at `probe-lib.jar`, loads
  `probe.lib.ResourceHolder` through it, and invokes the two static methods
  via reflection.

Since `probe-lib.jar` is on no other loader's classpath and the child loader
has no parent, the resource is reachable **only** by delegating
`getResourceAsStream`/`getResource` to `ResourceHolder`'s own defining
loader — the exact loader-blind gap `ceea4eb05` fixed (previously these
calls fell back to bootstrap/system-loader lookup only and would have
returned `null` here).

Result: 3/3 runs pass (2x baseline, 1x `CRATONVM_JIT_THRESHOLD=1` aggressive)
— correct content read back and a well-formed
`jar:file:.../probe-lib.jar!/probe/lib/data.txt` URL returned in every run.

```
content=CRATONVM_GETRESOURCE_DELEGATION_PROBE_OK
url=jar:file:/data/tmp/getresource_verify/probe-lib.jar!/probe/lib/data.txt
RESULT=PASS
```

Fix confirmed working post-rebuild; no further action needed on this item.
