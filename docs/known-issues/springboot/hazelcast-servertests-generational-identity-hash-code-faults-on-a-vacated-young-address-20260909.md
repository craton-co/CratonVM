# `identityHashCode` faults on an address the Generational collector already evacuated — `HazelcastAutoConfigurationServerTests`

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-09 at dev tip `ccf731dd3`. Reproduced standalone. Not root-caused; the READER is named exactly, the producer is not. |
| **Scope** | `module/spring-boot-hazelcast`, JIT on, real JDK 25, `--Xmx 2g`, **`--XX:UseGc Generational` only**. G1 and ZGC pass 20/20. HotSpot passes 20/20 in 33 s. |
| **Rate** | **3 observed.** 2 SIGSEGVs in 6 concurrent runs, plus 1 in a six-arm mixed matrix at 52 s. 0 in 3 runs launched alone or in pairs. It is load-sensitive, but a SIGSEGV is not a load artefact. |
| **Reproducer** | `org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfigurationServerTests`, Linux x86-64, 128–375 s |
| **Family** | The same "a stale reference reaches a reader after a moving young cycle" family as [`bindabletests-local-holds-an-interior-word-of-a-retired-tlab-filler-20260909.md`](bindabletests-local-holds-an-interior-word-of-a-retired-tlab-filler-20260909.md) and [`bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md`](bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md). Both of those have a **deterministic** reproducer under `CRATONVM_DBG_GC_STRESS`; this one does not, so **chase them, not this page.** |
| **Supersedes** | the Generational rows of the retired [Hazelcast crash-or-hang page](../../internal/springboot/hazelcast-autoconfiguration-crash-hang-RETIRED-20260909.md). Every other row of that page was a timeout or a fixed defect. |

## The reader is `NativeContextImpl::identity_hash_code`, in both crashes

Symbolised from the crash report's own `pc` and the `maps:` load base it
prints (`addr2line -f -C -i`), on three independent runs and two binaries:

| run | binary | `pc` | load base (`prev:` line) | file offset | symbol |
|---|---|---|---|---|---|
| `base2-Server-Generational` | pre-fix | `0x5c5895cc9f2b` | `0x5c5893cfb000` | `0x1fcef2b` | `NativeContextImpl::identity_hash_code` |
| `rate1-2` | pre-fix | `0x5e9dd1878f2b` | `0x5e9dcf8aa000` | `0x1fcef2b` | `NativeContextImpl::identity_hash_code` |
| `fix2-Server-Generational` | post-fix | `0x62a22510b1bb` | `0x62a22313c000` | `0x1fcf1bb` | `NativeContextImpl::identity_hash_code` |

Same function every time, `rdx=0x4` and the fault at `rsi+8` on all three. The crash handler's own operand decode agrees
it is a **read**, not a write:

```
#  SIGSEGV at pc=…f2b, addr=0x7ed46d1f08a0
#  rdx=0x4 rsi=0x7ed46d1f0898
#  fault addr decodes as an indexed load: [rsi+rdx*2] [r14+rdx*2]
#  fault pc is in NO recently freed code buffer
#  fault pc is in NO live registered code buffer
#  maps: fault pc IS MAPPED …  r-xp … /data/cratonvm-hzac-base
```

`rsi` is the receiver; the fault is at `rsi+8`, the mark word `identity_hash_code`
lazily CASes a hash into. The pc is in the VM's own `.text`, **not** in JIT
code and not in a freed code buffer, so this is the interpreter/native path
dereferencing a receiver it was handed — not compiled code reading through a
stale register.

## The receiver is inside the semi-space the collection emptied

```
#  gc_decommits_total=0x4
#  fault addr is inside a RECENTLY DECOMMITTED heap span:
#     base=0x7ed463c00000 len=0xa200000 site=unbumped-middle
#    *** and NOT re-committed since. ***
#  this thread last applied a relocation map at cycle=0x7 path=2 of relocating_cycles=0x7
```

`site=unbumped-middle` is `Arena::decommit_unbumped_middle`, and on this
collector its only caller is `GenerationalHeap::uncommit_evacuated_young`,
which runs on the **evacuated** young semi-space after the flip. Its stated
premise is "nothing live is in it — that is what the flip means". So the
address is one a completed moving cycle vacated, and something still named it.

The faulting thread had applied the newest relocation map (`cycle=0x7 of
relocating_cycles=0x7`), so this is not a thread that missed the remap; the
reference itself was never in a slot the remap covers.

## `CRATONVM_GEN_UNCOMMIT` is the messenger, not the defect

With `CRATONVM_GC_RESERVE=0` (or `CRATONVM_GEN_UNCOMMIT=0`) the granules stay
mapped and the same read returns stale bytes instead of faulting — the trade
[`types/src/flags.rs`'s `gen_uncommit` doc](../../../types/src/flags.rs) states
in full. Do not read a green arm under either flag as a fix.

## What is NOT this defect

* **Not the `java/nio/Bits$1` residual read** the superseded page named as its
  lead. That fires at line 6 of stderr, during boot, and on runs that pass
  12/12. Fixed for its own sake in `07fc03c0a`; it changed nothing here.
* **Not G1 or ZGC.** Both pass 20/20 on this class (491 s / 476 s).
* **Not `ClientTests`.** 12/12 on all three collectors.

## Why this page is not the one to chase

`CRATONVM_DBG_VACATED_FRAMES=1` — the instrument whose crash-handler arm prints
`VACATED REGISTER: rsi=0x… named an object a completed collection moved to
0x…`, which is the single line that would close this — **dilates this
workload about ninefold, past the point where the crash window is reachable.**
Measured in Hazelcast member lifecycles (`is STARTED`) rather than wall time, so
what is reported is work done and not host load:

| arms | budget | `CRATONVM_DBG_VACATED_FRAMES` | lifecycles reached | SIGSEGVs |
|---|---|---|---:|---:|
| 6 | 1200 s | off | 16 by 820 s, then PASS 20/20 | 2 of 6 |
| 6 | 1500 s | **on** | 3 to 9 | 0 of 6 |
| 6 | 3600 s | **on** | 7 to 9 | 0 of 6 |

Twelve armed runs and 5.5 armed hours produced no crash and not one
`VACATED REGISTER` line. Arming this instrument on this workload measures a
different run.

The two `BindableTests` pages reach the same family in **6–20 seconds** with
`CRATONVM_DBG_GC_STRESS=262144`, deterministically, with the producer already
narrowed to two candidates. This page's value is as a **second, independent
workload** confirming the family is not specific to JUnit's own machinery: the
victim here is reached through a native's `identityHashCode`, not through an
`invokevirtual` receiver or a frame local.

**Retire this page when either `BindableTests` page is fixed and this class
runs 20/20 on Generational over six concurrent arms.**

If someone does want to close it here rather than there, the measurement that
would do it is a verdict cheap enough to stay armed: the vacated ledger
consulted only AT THE FAULT, not fed by a recording side that runs all process
long. `was_vacated_try` is already signal-safe and already called from
`crash_handler.rs`; it is `vacated_frames_enabled()` gating the RECORD path
that costs the ninefold above.

## Repro

```bash
SB=<repo>/apps/spring-boot
cd "$SB/module/spring-boot-hazelcast"
for i in 1 2 3 4 5 6; do
  <cratonvm> --java-home <jdk25> --Xmx 2g \
    --add-opens=java.base/java.net=ALL-UNNAMED --stack-dump-on-timeout 0 \
    --XX:UseGc Generational -Dfile.encoding=UTF-8 -Djava.awt.headless=true \
    -cp "$SB/sb-runner:$(cat build/cratonvm-test-cp.txt)" \
    SbRunner org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfigurationServerTests \
    > run-$i.out 2> run-$i.err &
done; wait
grep -l DECOMMITTED run-*.err
```

Run them **concurrently**; one arm at a time passed 20/20 three times running.
Allow at least 1200 s per arm — a shorter budget reports a timeout, which is
how the superseded page came to describe this class as hanging.
