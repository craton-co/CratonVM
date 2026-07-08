# ReactorClientHttpRequestFactoryTests residual timeout / EPoll gap [FIXED]

Status: RETIRED 2026-07-08. Current `dev` no longer reproduces this
tracker's actionable symptom: `ReactorClientHttpRequestFactoryTests` passes
10/10 under the real-JDK Spring harness path on the Azure Linux probe host.
This doc was moved out of `docs/known-issues` per the active-tracker rule.

## Current verification

Fresh isolated worktree off `dev@d49ce503`:

```bash
cd /data/data/cratonvm-worktrees/20260708-170805-reactor-http-timeout
CARGO_TARGET_DIR=/data/data/target-reactor-http-timeout-20260708-170805 \
  cargo build --release -p cratonvm-cli
cp /data/data/target-reactor-http-timeout-20260708-170805/release/cratonvm \
  /data/data/cratonvm-binaries/cratonvm-reactor-http-timeout-20260708-170805.bin

CP=$(tr -d '\r' < /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)
KRUN_STACK=1 timeout 180 \
  /data/data/cratonvm-binaries/cratonvm-reactor-http-timeout-20260708-170805.bin \
  --java-home /data/data/jdk25-real --stack-dump-on-timeout 0 \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.ReactorClientHttpRequestFactoryTests
```

Result (`/data/data/probes/reactor-http-timeout-20260708-170805/stdout.log`):

```text
RESULT org.springframework.http.client.ReactorClientHttpRequestFactoryTests found=10 succ=10 fail=0 skip=0 abort=0 ms=4087 status=OK
```

Historical current-dev evidence already agreed before this pass:

- `/data/data/osr2-fullrun-20260706-1954/results.tsv`: `OK`, 10/10, 5808 ms.
- `/data/data/tmp/out_regress_fixed/results.tsv`: `OK`, 10/10, 3914 ms,
  after `/data/data/tmp/out_regress_baseline/results.tsv` had classified the
  same class as `TIMEOUT`.

## Why the old tracker is obsolete

The old Linux note said the host could not run this class because
`sun/nio/ch/EPollSelectorImpl` was absent. That is no longer true on current
`dev`: `native-io/src/nio_selector.rs` now registers the JDK EPoll-facing
native surface and routes `EPollSelectorProvider.openSelector()` into
CratonVM's selector implementation, while `vm/src/vm/vm_exec.rs` force-routes
the public `EPollSelectorImpl` selector entry points through the same native
selector path. Netty/Reactor no longer dies in the EPoll static-init cascade.

The older Windows-side `TIMEOUT` entry never had a current hang stack in this
doc. The most relevant later fixes are the NIO/SocketChannel reactor fixes
that landed after the original cluster notes, especially `cf78cbb8` /
`e84859d7` (`supportedOptions()`), `1b833340` (`isConnectionPending()`),
`6d235ddc` (blocking-region brackets around real selector waits), and
`cf4b2549` (selector side-table GC pinning). With the class now green in the
same Spring real-JDK probe family, there is no remaining open CratonVM bug to
track here.

If a future Windows/JDK 25 harness run on current `dev` times out again, file a
new `docs/known-issues` document with fresh logs and a current stack dump rather
than reopening this stale split-out tracker.
