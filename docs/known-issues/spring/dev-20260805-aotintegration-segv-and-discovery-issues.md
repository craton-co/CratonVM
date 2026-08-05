# `dev` between 2026-08-02 and 2026-08-05 breaks `AotIntegrationTests` — SIGSEGV in a freed JIT code buffer, and 294 critical discovery issues

**Status: OPEN.** Found on 2026-08-05 while re-verifying
`fix/spring-4tests-20260802` after merging `origin/dev` into it. **Not caused by
that branch** — see the controlled comparison below, which holds the branch's
two fixes constant and varies only the `dev` base.

## Evidence

Same host (Azure `20.83.144.174`), same runner, same repaired classpath, same
class (`org.springframework.test.context.aot.AotIntegrationTests`), runs
alternated inside one script so host load hits both equally. HotSpot's answer
for this class is `found=4 succ=2 fail=0 skip=2`.

| binary | contents | result |
|---|---|---|
| `cvm-spr4-gcscan.bin` | `dev` @ `86a01abf90` (2026-08-02) + the branch's 2 fixes | `found=4 succ=2 fail=0 skip=2` **OK**, three times (769 s, 865 s, 971 s) |
| `cvm-spr4-merged.bin` | `dev` @ `4fafae0d4`-era (2026-08-05, +882 commits) + **the same 2 fixes** | run 1: **SIGSEGV** at 157 s; run 2: `found=4 succ=0 fail=2 skip=2` at 77 s |

The branch's two fixes (`native-io/src/net.rs` EINTR arm, `gc/src/old_gen.rs`
dirty-card cursor) are byte-identical in both binaries, so they are not the
variable. The variable is the 882 `dev` commits.

## Failure 1 — SIGSEGV executing a freed JIT code buffer

```
#  SIGSEGV at pc=0x70636ff87f41, addr=0x5, pid=2930808
#  code_frees_total=0xef5
#  fault pc is inside a RECENTLY FREED code buffer: base=0x70636ff86000 len=0x2000
#      active_jit_executions_at_free=0x0
#  fault pc is inside a LIVE registered code buffer: base=0x70636ff87000 cap=0xb740
#  maps: fault pc IS MAPPED - perms are on the `here` line
#    here: 70636ff87000-70636ffab000 r-xp
```

Note the crash handler reports the pc as inside **both** a recently freed buffer
and a live registered one, i.e. a freed buffer's address range was recycled
under an executing frame. This is the shape described in
`jit-code-unmapped-while-executing`, except the page is still mapped (it was
handed to a *new* buffer), so it faults on decoded garbage rather than on an
unmapped page. `active_jit_executions_at_free=0` means the retirement queue
believed nothing was executing when it released it.

## Failure 2 — 294 critical discovery issues

The second run did not crash; it failed discovery outright:

```
org.junit.platform.launcher.core.DiscoveryIssueException:
  TestEngine with ID 'junit-platform-suite' encountered 294 critical issues
org.junit.platform.launcher.core.DiscoveryIssueException:
  TestEngine with ID 'junit-jupiter' encountered 20 critical issues
```

and a third run of the same class through a different driver produced
`TestContextAotException: Failed to generate AOT`. So the merged build fails
this class in at least three different ways, none of which the 2026-08-02 base
produces.

## What is NOT the cause

* **Not the branch's fixes** — held constant across both binaries (above).
* **Not host load.** `RestClientIntegrationTests`, A/B'd the same way on the
  same two binaries, gave `230/229/1 abort` (= HotSpot) for *both* in round 1
  and the same single load-induced I/O error for *both* in round 2 — so the
  harness does discriminate correctly and the two binaries are otherwise equal.
  `RequestMappingMessageConversionIntegrationTests` also scores 160/160 on both.
* **Not the classpath rot** described in
  `beanregistrations-verylarge-heap-footprint-20260805.md` — that was repaired
  first and re-checked (0 missing of 254) before these runs.

## Reproducing

```bash
cd apps/spring-suite-runner
CRATONVM_BIN=<merged-build> ./one.sh org.springframework.test.context.aot.AotIntegrationTests
```

~80-160 s to the failure, versus ~900 s for a clean pass. Bisecting the 882
commits is the obvious next step; the JIT code-cache retirement path is the
place to start given failure 1.
