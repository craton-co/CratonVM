# `IntObjectHashMapTest` SIGSEGVs before its first test, intermittently, on pristine `dev`

**Status:** OPEN (2026-08-14). Not root-caused. Filed from a control run, not
from an investigation — it turned up while A/B-ing an unrelated collections
change and the control arm is what makes it worth a page.

`io.netty.util.collection.IntObjectHashMapTest` crashes the VM **before it
reports a single test**, on a binary built from `origin/dev` `c017029b3` with no
local changes. HotSpot JDK 25 runs the same class on the same classpath in
**454 ms, 35/35**.

## Measured

Azure host 2, Linux, one class per process, `-XX:+UseZGC`, idle box (load 2-3).

| binary | runs | outcome |
| --- | --- | --- |
| pristine `dev` `c017029b3` | 4 (first sweep) | 3 × 35/35 in ~4 s, 1 × SIGSEGV |
| pristine `dev` `c017029b3` | 3 (second sweep, ~1 h later) | 1 × 35/35, **2 × SIGSEGV** |
| HotSpot JDK 25 | 1 | 35/35 in 454 ms |

The rate moved from 1-in-4 to 2-in-3 within an hour on the same binary and the
same quiet box, so treat "it passed" as no evidence. A collections branch tested
against it showed the same rate and the **same crash signature**, which is how
the crash was attributed to `dev` rather than to that branch.

One run hung instead of crashing (240 s cap, no output), so a timeout on this
class is probably the same defect and not a separate one.

## Signature

Identical across binaries — the same fault address and the same register
pattern, which is what says it is one bug:

```
#  SIGSEGV at pc=0x…, addr=0x0
#  jdk mode: real-jdk
#  r10=0xee5fffebdfffff00 r11=0x… rsp=0x… rbp=0x20042496501
#  fault pc is in NO recently freed code buffer
#  fault pc is in NO live registered code buffer
#  maps: fault pc IS MAPPED - r-xp … /data/bin-asl-<tag>
#  slot[r10]: UNREADABLE (r10 is not a readable pointer)
```

`addr=0x0` with an unreadable `r10`, and a fault pc inside the VM's own text
rather than in JIT-emitted code (`NO live registered code buffer`), so this is
not the `pc==addr` unmapped-JIT-code shape. `r10` reads as a colored/tagged
word (`0xee5f…`), which is where an investigation should start.

The crash lands **during discovery**, before `CratonRunner` prints `@@RESULT`:
the captured stdout is 8.6 KB of startup tracing and nothing else.

## Not yet done

No `hs_err` file read beyond the console summary, no `--nojit` arm, no
collector comparison (only ZGC was run), and no attempt to narrow which of the
35 tests is being discovered when it faults. The class is generated
(`common/target/generated-test-sources/collections/…/IntObjectHashMapTest.java`),
so a minimal reproducer means reading that source rather than the repository's.

## Repro

```bash
cd apps/netty-suite-runner
for i in 1 2 3; do
  cratonvm --java-home <jdk25> --Xmx 1500m @common.args -XX:+UseZGC \
    -Dcraton.batch=1 CratonRunner io.netty.util.collection.IntObjectHashMapTest \
    > /tmp/iohm-$i.out 2>&1
  echo "run $i rc=$?"
done
CP=$(sed -n 2p common.args)
java -cp "$CP:." -Dcraton.batch=1 CratonRunner io.netty.util.collection.IntObjectHashMapTest
```

`rc=139` is the crash, `rc=124` the hang, `rc=0` with a `@@RESULT` line the pass.
