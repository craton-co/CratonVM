# gen-GC loses a `CompletableFuture` completion under promotion + allocation churn (OPEN)

**Status:** OPEN. Reliable workarounds exist (`-XX:+UseG1GC`, `CRATONVM_NO_GC_PROMOTION=1`); no clean code-level fix yet.

**Severity:** blocks the Keycloak 26.6.3 / Quarkus boot (see
[keycloak-quarkus-boot-progress.md](keycloak-quarkus-boot-progress.md)). General: any
`CompletableFuture.get()`-on-a-worker pattern under heap churn can hang.

## Symptom

Under the **default generational (copying) GC**, a thread blocked in
`CompletableFuture.get()` waiting on an async worker is **never woken**, even though
the worker completes the future. The boot deadlock surfaces at the end of the Hibernate
SessionFactory build:

```
io.quarkus.runner.ApplicationImpl.doStart
 → io.quarkus.hibernate.orm.runtime.JPAConfig.startAll()
  → java.util.concurrent.CompletableFuture.get() → waitingGet
   → ForkJoinPool.unmanagedBlock → CompletableFuture$Signaller.block() → LockSupport.park()  ← hangs forever
```

The async persistence-unit worker finishes and returns to its pool (idle); main stays
parked. Under heap *pressure* (small `-Xmx`) the same scenario instead **aborts** the
process (`KERNEL32!FatalExit`, silent exit — no Java exception / panic / `System.exit`).

## Root cause (confirmed mechanism)

When the `CompletableFuture` is **promoted to old gen** by a young GC while a thread's
pushed `CompletableFuture$Signaller` (linked via the future's lock-free `stack` field)
**stays young**, the gen-GC loses that old→young reference: the young `Signaller` is not
kept alive across a subsequent young GC, so the completing worker's `postComplete`
never fires it → `LockSupport.unpark(mainThread)` is **never called** → the parked
thread hangs.

Confirmed:
- `CRATONVM_NO_GC_PROMOTION=1` → **reliably fixes** it (no promotion ⇒ no old→young edge ⇒ no loss). This is the decisive structural proof the bug is promotion-related.
- `-XX:+UseG1GC` → **immune** (G1's remembered-set/marking handles it).
- The park machinery is sound: the park/unpark trace (`CRATONVM_DBG_PARK`) shows
  `unpark(main)` HITS the correct `ParkState` when called, **0 lookup misses**, and the
  poll-based `ParkState` is lost-wakeup-immune. The unpark is simply **never issued** —
  the completion-stack linkage to the `Signaller` is gone.
- GC path = `gc/src/gen_heap.rs::sweep_young_non_moving` (CFProbe2's heavy allocation
  routes here under old-gen pressure, even with `CRATONVM_DISABLE_JIT=1`).

## What was ruled out

- **NOT a missed *next-GC* card.** Tried (and reverted): in the `new_old_young` scan
  (`sweep_young_non_moving`, ~`gen_heap.rs:4007`) conservatively re-dirty **every**
  promoted object's card (not just `pts`-detected ones). One run-batch passed 5/5, but a
  later clean batch (no CPU contention) hung 3/3 at iter ~22. So the `Signaller` is lost
  in the **same GC** as the promotion (mark-phase miss / evacuation race), not via a card
  that's merely missing for the next GC.
- **NOT the write barrier / flush.** `write_barrier` (`gen_heap.rs:1981`) correctly
  cards old→young; `compare_and_swap_field` (`vm/src/vm/vm_exec.rs:4609`, the VarHandle
  CAS used by `tryPushStack`) calls it; `flush_all` drains parked threads' per-thread
  dirty buffers before the card scan (`gen_heap.rs:2451`). All correct statically.
- Most likely a **promotion/evacuation × concurrently-completing-worker race** (the
  worker's `postComplete` CAS on `cf.stack` racing the GC's scan/evacuate of that
  object), plausibly tied to the MT-STW/safepoint machinery.

## Heisenbug — why it resists in-VM instrumentation

Both built-in detectors **mask** it: `CRATONVM_SP_VERIFY=1` and
`CRATONVM_DBG_SWEEP_EDGES=1` each add a per-GC full old-gen walk whose extra time shifts
the GC/mutator interleaving so the failing non-moving-sweep-promotion path isn't taken →
the run passes and prints no report. Only `CRATONVM_NO_GC_PROMOTION` (a behavior change,
not added overhead) reliably fixes it. Pinning the exact loss therefore needs a
**non-perturbing** technique: a heap-dump diff across the failing GC, or a hardware
watchpoint on the future's `stack` slot.

## Reproducer (≈3 s)

`CFProbe2` — a worker that allocates heavily (triggers GC) while main blocks on
`cf.get()`. HotSpot: 30/30 OK. CratonVM default gen-GC: HANG (or abort at small heap).
CratonVM `-XX:+UseG1GC`: 30/30 OK. The no-allocation variant (`CFProbe`) passes
everywhere, confirming the hang is GC-induced.

```java
import java.util.concurrent.*;
import java.util.*;
public class CFProbe2 {
    public static void main(String[] a) throws Exception {
        ExecutorService ex = Executors.newFixedThreadPool(3);
        for (int i = 0; i < 30; i++) {
            final int n = i;
            CompletableFuture<String> cf = CompletableFuture.supplyAsync(() -> {
                List<byte[]> junk = new ArrayList<>();
                long acc = 0;
                for (int j = 0; j < 300000; j++) {
                    byte[] b = new byte[256]; acc += b.length;
                    junk.add(b);
                    if (junk.size() > 2000) junk.subList(0, 1000).clear();
                }
                return "done" + n + ":" + acc;
            }, ex);
            String r = cf.get();   // main PARKS here for the heavy worker's duration
            System.out.println("[cf2] iter " + n + " -> " + r);
        }
        System.out.println("[cf2] ALL 30 OK");
        ex.shutdown();
    }
}
```

Run (hangs on default gen-GC, ~iter 22; OK with `-XX:+UseG1GC` or `CRATONVM_NO_GC_PROMOTION=1`):
```
cratonvm.exe --Xmx 2g -cp <dir> CFProbe2          # default gen-GC → HANG
cratonvm.exe -XX:+UseG1GC --Xmx 2g -cp <dir> CFProbe2   # → ALL 30 OK
```

## Next steps

1. Non-perturbing observation to pin the same-GC loss (heap-dump diff / HW watchpoint on
   `cf.stack`); then a targeted fix in `sweep_young_non_moving` mark/evacuate.
2. For a *booting* Keycloak sooner, prefer the **G1 route** (immune to this bug) and fix
   the separate, distinct hang seen only under G1 (two persistence-unit worker threads
   stuck executing bytecode at the same point — a worker livelock, not this GC bug).

## Related

- The VarHandle static-field init fix (dev `b223fd21`) that first unblocked the whole
  Hibernate SessionFactory build, exposing this next blocker.
- Other moving-GC root/edge issues: [gc-moving-interpreter-lost-tag-missed-root.md](gc-moving-interpreter-lost-tag-missed-root.md),
  [gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md](../internal/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md) (FIXED).
