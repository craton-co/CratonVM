# `BasicErrorControllerIntegrationTests` aborts the VM with `checkcast: not an object reference` on dev `376114f635`

**Status: OPEN (found 2026-07-31, dev `376114f635`).** A hard VM abort, not a
test failure — the process dies mid-run:

```
[cratonvm] main-vm run() returned Err: Error in thread "main"
internal error: checkcast: not an object reference
```

Intermittent, but frequent: **5 hard aborts and 3 partial failures in 12
runs** of a single class.

## Affected

`org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`
(`module/spring-boot-webmvc`, Spring Boot 4.1.0-SNAPSHOT), run via
`sb-runner/SbRunner` under real JDK 25. The abort happens after the Spring
Boot banner prints, i.e. during application-context startup, and takes the
whole VM with it.

## It is a regression, and it is not the JIT bans

Measured while retiring the Spring/javac JIT-ban inventory
(`docs/internal/jit-bans/spring-jit-bans-inventory-and-ban-lift-experiment-20260730.md`),
12 runs per configuration:

| Binary | JIT bans | Aborts | Partial failures | Clean |
|---|---|---|---|---|
| dev `9ac1feffe`, all 11 Spring/javac bans deleted | removed | 0 | 0 | 12 |
| dev `9ac1feffe`, bans intact | present | 0 | 0 | 12 |
| dev `376114f635` **pristine** (detached worktree, zero local changes) | **present** | **5** | **3** | 4 |
| dev `376114f635` + the ban-removal branch | removed | 5 | 4 | 3 |

So it arrived on `dev` between `9ac1feffe` and `376114f635`, and it is
present with every JIT ban still in force. The two `376114f635` rows are
statistically indistinguishable, which is the point: the ban removal neither
causes nor worsens it.

Note the shape coincidence that made this worth chasing: `internal error:
checkcast: not an object reference` is also the documented symptom of
`SPRINGBOOT-HTTP-HEADER-COMPARATOR.1`, one of the bans removed on that
branch (its doc comment: "a call to `CaseInsensitiveComparator.apply(Object)`,
followed by a fatal invalid-reference checkcast"). The pristine-dev control
above is what separates the two.

## Suspects, and one ruled-out shortcut

`376114f635` is the merge of `codex/moving-young-default-20260730`, so the
moving-young generational default is the obvious first suspect. A quick check
with `CRATONVM_NO_MOVING_YOUNG=1` did **not** produce a clean run — the class
timed out at 600 s instead of aborting, i.e. that configuration trades the
abort for a hang and does not isolate anything. Not investigated further;
whoever picks this up should bisect `9ac1feffe..376114f635` properly rather
than reason from the merge title.

## Reproduction

```bash
CRATONVM_BIN=<binary> /data/data/sbrepeat.sh <label> 12
```

(`sbrepeat.sh` loops `sbone.sh module/spring-boot-webmvc <fqcn>` and keeps the
log of any run that is not 26/0; both scripts live on the Azure host at
`/data/data/`, pointed at the standalone Spring Boot checkout
`/data/data/springboot-jsonreader-deprecation-20260718`, whose
`module/spring-boot-webmvc/build/cratonvm-test-cp.txt` must be regenerated
with `:module:spring-boot-webmvc:cratonvmTestCp` — the checked-in copy holds
stale Windows paths.)

Expect roughly 4 in 10 runs to abort. A single green run means nothing here.
