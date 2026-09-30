# `io.netty.channel.kqueue.KQueue*` — not a CratonVM bug, kqueue is BSD/macOS-only

| | |
|---|---|
| **Status** | Confirmed NOT a CratonVM bug. Structural platform mismatch, not a defect. |
| **Scope** | 56 classes, all under `io.netty.channel.kqueue.*`, in `testlist.txt`. |
| **Discovered** | 2026-09-06, full 733-class 3-GC-arm suite run (`dev` post `class-overrides.tsv` restoration). |

## Symptom

Every class in the package fails identically and immediately:

```
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError
  class=io/netty/channel/kqueue/Native cause=java/lang/IllegalStateException Only supported on OSX/BSD
```

`java.lang.ExceptionInInitializerError: null` is the wrapper the test-reporting
layer sees; the real cause is Netty's own kqueue native-support check.

## Why this is not a CratonVM defect

`kqueue(2)` is a BSD/macOS kernel facility. It does not exist on Linux at all —
this is not a missing native library or an unimplemented CratonVM feature, it is
an OS-level absence. Netty's own `io.netty.channel.kqueue.Native` static
initializer checks the platform and throws exactly this
`IllegalStateException` on any non-BSD/macOS host, **on HotSpot identically to
CratonVM** — there is no code path on Linux, real JDK or CratonVM, that could
make these classes pass.

This is collector-independent (identical across Generational, G1, and ZGC) and
was the single largest cluster in the 3-arm FAIL count: 56 of 78 FAIL classes
common to all three arms.

## Disposition

No fix needed or possible on this host. The correct long-term action is to
exclude `io.netty.channel.kqueue.*` from `testlist.txt` on Linux hosts (the
class discovery that produced `testlist.txt` does not currently filter by
platform-applicability), which would remove 56 permanent, uninformative FAIL
rows from every future full-suite tally on Azure. Not done as part of this
triage — recorded here so the next session does not re-diagnose it.

## Reproducing

```bash
cratonvm --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.channel.kqueue.KQueueChannelConfigTest
```
