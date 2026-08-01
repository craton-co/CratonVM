# `TestDefaultInstanceManager.testClassUnloading` — third occurrence, 2026-07-31

**Status:** OPEN — recurred again despite both fixes in
[defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md](defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md)
being present in the tree. Not re-root-caused here; this note exists so the
next session doesn't have to rediscover that the "FIXED" status is not
holding.

## Symptom (identical to the two prior writeups)

```
java.lang.AssertionError: expected:<8> but was:<9>
	at org.apache.catalina.core.TestDefaultInstanceManager.testClassUnloading(TestDefaultInstanceManager.java:66)
```

## What was checked before concluding this is real

1. Seen first in a 99-class, 4-parallel, 1500s-timeout rerun on `dev` merged
   fresh this session (top commit `32f9db9a2`).
2. Reproduced standalone, 2/2, from the correct CWD (`apps/tomcat`, matching
   how `run-tomcat-suite.ps1` invokes it — an initial standalone attempt from
   the wrong CWD produced an unrelated `FileNotFoundException:
   conf\logging.properties` red herring, discarded).
3. Confirmed the actual fix code is present and compiled into the binary
   under test: `mirror_pin_deferrable` exists in `gc/src/vm_heap.rs` and is
   referenced from `vm/src/memory/roots.rs`, both current at this `dev` tip.
4. Fully deterministic in this session's testing (3/3 in the original CWD
   mistake, 2/2 after correcting it) — not the "depends on whether the
   mirror had been promoted" non-determinism the original doc describes.
   That consistency is itself informative: if this were the same timing-
   sensitive defect, occasional PASSes would be expected.

## What this doesn't tell us

Whether this is: the same two defects reappearing via a third path the
fix's ON/ON matrix didn't cover, a *different* new retention/GC-timing bug
with the same symptom, or a regression in the fix itself from later `dev`
changes to `roots.rs`/`vm_heap.rs`. None of that was investigated — it needs
the same GC-forensics approach the original fix used
(`VmHeap::retention_paths`, root-source attribution), not a quick recheck.

## Reproduction

```powershell
cd apps\tomcat
$env:CRATONVM_REAL_NET_SOCKETS='1'; $env:CRATONVM_REAL_AQS='1'
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG='1'; $env:CRATONVM_ROOTSNAP_CACHE='1'
<cratonvm.exe> --Xmx 2g -c "$(Get-Content .suite\cp.txt)" `
  org.junit.runner.JUnitCore org.apache.catalina.core.TestDefaultInstanceManager
```
**Must run with CWD = `apps\tomcat`** — running from the repo root produces
an unrelated `FileNotFoundException: conf\logging.properties` that looks
like a different bug entirely (relative-path resolution, same class of trap
documented in `docs/internal/fixed-suite-bugs/tomcat/16-full-suite-6shard-rerun-20260721.md`'s
CWD-bug correction, on a different host).
