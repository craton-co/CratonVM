# ReactorClientHttpRequestFactoryTests: Linux EPollSelectorImpl gap (expected) + Windows TIMEOUT (uninvestigated) [OPEN]

Status: OPEN (the Windows-side symptom; the Linux-side symptom below is a
documented environment limitation, not a CratonVM bug). Split out of the
retired `http-client-cluster-redefine-dispatch-and-jdk21-gaps.md`.

## Linux: `sun/nio/ch/EPollSelectorImpl` gap (not a bug)

Netty's epoll-based `Selector` is Linux-only (Windows uses a completely
different selector implementation) and CratonVM does not implement it.
This blocks 8/10 tests in `ReactorClientHttpRequestFactoryTests` when run
on the Azure Linux dev host. This is expected and confirmed absent from
the Windows JDK 25 install via `javap` — see memory
`azure-host-jdk21-linux-only-native-gaps`. Not something to fix; only
relevant when using the Linux host for fast iteration.

## Windows: TIMEOUT, not investigated

On the actual target platform (Windows + JDK 25, the platform the
original bug report was captured on), this class instead shows `TIMEOUT`
(previously `FAIL` 2/8, before the DNS-resolver native fix documented in
`docs/internal/fixed-suite-bugs/http-client-cluster-redefine-dispatch-fixes-FIXED.md`
got it past a `NoClassDefFoundError` cascade). Since the Linux host's
failure mode (`EPollSelectorImpl`-blocked) is a different platform gap
entirely, the Linux host cannot help debug the Windows-side TIMEOUT — it
needs a dedicated Windows-side session with the official
`apps/spring-suite-runner` harness.

**Next step:** reproduce directly on Windows
(`run-suite.sh run --jdk real --jit on --batch 1 --only
'http\.client\.ReactorClientHttpRequestFactoryTests'`), capture where it
actually hangs (likely needs Windows-native thread-dump tooling since gdb
isn't available there), and determine whether it's the same class of bug
as the other Reactor-Netty-based residuals in this cluster or something
new.
