# Precise-JIT-roots: multi-thread (ForkJoinPool worker) reclamation test case

**Audience:** the session working on shadow-stack / precise JIT stack maps
(`project_precise_jit_stack_maps`, `PluginXmlParserTests JIT inline-new / GC-under-JIT`
heap corruption). This is a **handoff of a second, multi-threaded test case** for
your work — not a separate bug. Written from the HIB-CV-20 session (branch
`fix/hib-cv-20-jit-fjp-escape`, worktree `C:/craton/CratonVM-cv20`,
base `dev d9528134`).

Full raw transcript of the investigation that produced this:
`C:\Users\Victor\.claude\projects\C--craton-CratonVM\beb0697c-9a95-4fad-91fd-9cd2948aaed6.jsonl`

---

## TL;DR

Your shadow-stack work is the right fix for a JIT-held live object being
reclaimed by the non-moving sweep. Your validation is **single-threaded**
(bintrees, PluginXmlParserTests). I have a **multi-threaded** instance that the
current dev shadow-stack does **NOT** fix: live `ForkJoinTask`s are reclaimed
out from under **ForkJoinPool worker threads** running JIT-compiled
`ForkJoinPool.runWorker`. It's the §4 "multi-thread shadow scan" path that's
incomplete. Use my repro as a worker-thread test case.

## How to reach it (it's normally invisible)

This path only runs under the opt-in gate `CRATONVM_REAL_FORKJOINPOOL=1` (a
HIB-CV-20 fix now on `dev`, commit `d9528134`). With the gate OFF, the synthetic
ForkJoinPool runs every task inline on the calling thread — no real workers, no
real `runWorker`, so you never exercise the worker-thread JIT roots. That is why
no default-config run (bintrees, app suites) trips this.

## Minimal repro

`scratch/xworker/Fork3.java` (racy) and `scratch/xworker/Fork6.java`
(`System.gc()`-forced, deterministic ~rep 4). A `RecursiveTask<String>` divide-
and-conquer that combines via `l + r` (heavy allocation → frequent young GC),
run under the gate **with JIT on**:

```
CRATONVM_REAL_FORKJOINPOOL=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  target/release/cratonvm.exe --java-home <jdk25> -c scratch/xworker Fork3
```

Symptom: worker threads throw, in `ForkJoinWorkerThread.run(187) ->
ForkJoinPool.runWorker(1992)`:
- `NullPointerException: Cannot invoke trySetException on null`, and/or
- `ClassCastException: java/lang/Object cannot be cast to java/util/concurrent/ForkJoinTask`, and/or
- `gen_heap::get_field: out-of-bounds field read ... class_id=ClassId(0) num_slots=0`
  (the reclaimed-object signature: a swept slot reads as a zeroed `java.lang.Object`).

The deque/task references decay to reclaimed (zeroed) memory — a live
`ForkJoinTask` was collected because a JIT worker frame's root wasn't kept.

## Toggle matrix (current `dev` + the HIB-CV-20 fix, JIT on, gate on)

| toggle | result | what it tells you |
|---|---|---|
| `--Xmx 8g` (suppress young GC) | **PASSES** | it's GC **reclamation**, not codegen |
| `--nojit` | **PASSES** | interpreter frames are precisely scanned; JIT roots are the gap |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT=1` | fails (CCE) | **NOT** scalar replacement / the kafka bug-25 escape gap |
| `CRATONVM_NO_SELECTIVE_PROMOTE=1` | fails (CCE) | not the selective-promote sweep variant |
| `CRATONVM_SHADOW_STACK=1` | **fails** — NPE "trySetException on null" in workers **+ SIGSEGV(139)** | current precise roots do NOT cover worker frames |
| `CRATONVM_SHADOW_STACK=1 CRATONVM_SHADOW_PIN=1` | **fails** — CCE + AIOOBE in workers, hang | pin vs movable only changes the corruption shape |

So: definitely the non-moving-sweep-reclaims-a-live-JIT-root problem you're
solving, but the live root lives in a **worker thread's** JIT frame and the
multi-thread shadow path doesn't capture/remap it.

## Where to look (multi-thread shadow path)

- `vm/src/runtime/interpreter.rs:1463` — §4 "multi-thread shadow scan, marking
  half": `update_root_snapshot` publishes THIS thread's shadow roots into its
  `root_snapshot` (which the STW reads via `collect_all_root_snapshots`).
- `vm/src/runtime/interpreter.rs:1638` — §4 "remap half" in
  `apply_pointer_map_to_thread` (non-initiator threads).
- Question to answer: when a `commonPool-worker-N` thread is parked at the STW
  inside JIT-compiled `ForkJoinPool.runWorker`, is its shadow stack (a) non-empty
  with the live task oop, and (b) actually scanned as a root by the initiator?
  The `CRATONVM_DBG_SHADOW_DEPTH=1` trace (per-thread depth at each GC) is the
  quick probe; a worker depth of 0 at the GC = the JIT prologue never pushed the
  task to the shadow stack (runWorker/its callees aren't shadow-instrumented), so
  the task is invisible regardless of the marking/remap halves.
- Note `ForkJoinTask` family + `ForkJoinPool` methods: the FJP task family
  (`ForkJoinTask`/`RecursiveTask`/`RecursiveAction`/`CountedCompleter`) is JIT-
  **blocklisted** (`interpreter.rs::is_fjp_subclass_blocklisted`), but
  `ForkJoinPool.runWorker`/`WorkQueue` are **not** — so runWorker IS JIT-compiled
  and is the frame holding the popped task. I tried adding `ForkJoinPool*` to the
  blocklist: it only changed the crash shape (the task-holding frame is also
  elsewhere — likely the external submitter `Fork3.main`, JIT-compiled, holding
  the root task across `invoke`). A blocklist can't fully cover it; precise roots
  is the real fix.

## Context: the HIB-CV-20 fix this rides on (already merged to dev)

`native-api/src/registry.rs` (commit `d9528134`, fast-forwarded into `dev`):
under `CRATONVM_REAL_FORKJOINPOOL` the gate now ALSO drops the synthetic FJP
task-family natives (`ForkJoinTask`/`RecursiveTask`/`RecursiveAction`/
`CountedCompleter`). Before, those `getRawResult()`/`get()`/`join()` natives read
a `fjp_state` side-table the real `exec()` never populated → `pool.invoke/get`
returned stale garbage. With the gate, real task bytecode runs end-to-end —
which is what makes the **real worker threads actually execute**, which is what
exposes this JIT-root reclamation under JIT. (NOJIT census mode: fully green.)

So enabling real concurrency under JIT is the new thing that surfaces your
precise-roots gap on the multi-thread axis. Validating your shadow-stack fix
against this repro closes that axis.

## Suggested acceptance for your fix

`CRATONVM_REAL_FORKJOINPOOL=1` + JIT on + `Fork3`/`Fork6` → ALL-OK over several
runs, no worker exceptions, no SIGSEGV — alongside your existing single-thread
bintrees/PluginXmlParser checks. (`-Xmx8g` already passes, so the bar is "correct
at the default small heap where young GC actually runs.")

## Appendix: self-contained repro (no scratch deps)

`javac Fork6.java` then run as above with the gate + JIT. Deterministic failure
~rep 4 (the two `System.gc()` calls force a collection while tasks are in
flight). On HotSpot and on CratonVM `--nojit`/`-Xmx8g` it prints `ALL-OK`.

```java
import java.util.concurrent.*;
public class Fork6 {
    static final ForkJoinPool POOL = ForkJoinPool.commonPool();
    static volatile String[] SINK = new String[1];
    static volatile int ROOT_N = 0;
    static final class StrTask extends RecursiveTask<String> {
        final int lo, hi; StrTask(int lo,int hi){this.lo=lo;this.hi=hi;}
        protected String compute(){
            if(hi-lo<=1) return "["+lo+"]";
            int mid=(lo+hi)>>>1; StrTask left=new StrTask(lo,mid); left.fork();
            String r=new StrTask(mid,hi).compute(); String l=left.join();
            if(l==null||r==null) throw new IllegalStateException("nullchild["+lo+","+hi+")");
            String res=l+r; if(lo==0&&hi==ROOT_N) SINK[0]=res; return res;
        }
    }
    public static void main(String[] a){
        int N=64; ROOT_N=N; StringBuilder sb=new StringBuilder();
        for(int i=0;i<N;i++) sb.append("[").append(i).append("]");
        String want=sb.toString();
        for(int rep=0; rep<200; rep++){
            SINK[0]=null; Object got;
            try { StrTask t=new StrTask(0,N); ForkJoinTask<String> f=POOL.submit(t);
                  System.gc(); got=f.get(); }
            catch(Throwable e){ System.out.println("rep="+rep+" THREW "+e); return; }
            if(!want.equals(got)){ System.out.println("rep="+rep+" FAIL got="+got
                +" sinkOk="+want.equals(SINK[0])); return; }
        }
        System.out.println("ALL-OK");
    }
}
```

Note: this repro is the SAME object that surfaced the HIB-CV-20 nojit bug
(the synthetic side-table shadow) — under `--nojit` it now passes because of the
`d9528134` fix; under JIT it fails for the precise-roots reason documented above.
A reflective dump of a failed task is the giveaway: `RecursiveTask.result` is the
correct `String` via reflection, but `getRawResult()` / a worker's deque pop
returns a bare `java.lang.Object` (a reclaimed/zeroed slot).

