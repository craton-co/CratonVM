import java.util.Random;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

// Repro for docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md.
//
// Built as a BUG-03-family probe (field-sharing variant: unlike SpawnN.java,
// which showed staleness landing only in a THREAD FRAME local, this hammers
// a small shared array of HEAP FIELDS) to try to reproduce gen_heap.rs's
// "corrupt Value cell" guard (HIB-CV-32) via a real ExecutorService pool
// (mirroring WildFly's ManagedExecutorService), matching
// EEConcurrencyExecutorShutdownTestCase's shape: many real OS-thread-backed
// workers, constant allocation, small-heap GC pressure, JIT enabled.
//
// What it actually found instead (2026-07-06): on unmodified dev, this
// reliably DEADLOCKS within the first round (confirmed via live `gdb -p
// <pid> -batch -ex 'thread apply all bt'`) on an AB-BA lock-order inversion
// between `SharedVm.class_manager` and `SharedVm.vtable_manager`:
//   - class loading (ClassManager::define_class_with_options ->
//     fire_vtable_install_hook -> vtable_install_adapter) holds
//     class_manager (write) then takes vtable_manager (write);
//   - the interpreter's virtual-dispatch fast path
//     (execute_invokevirtual_vtable_fast, vm/src/runtime/interpreter.rs)
//     held vtable_manager (read) while ALSO taking class_manager (read).
// Fixed by dropping the vtable_manager guard before acquiring class_manager
// in the dispatch fast path (interpreter.rs ~26840-26880). This explains the
// WildFly symptom's TIMEOUT/hang shape very well (concurrent class loading +
// dispatch during domain startup); whether it is ALSO the literal trigger
// for the repeated HIB-CV-32 "corrupt Value cell" log line is not
// separately confirmed by this probe — across 5 clean post-fix runs the
// guard never fired (only the unrelated, expected/benign `mark_young:
// rejecting ... implausible extent` conservative-root-candidate noise did).
// Usage: FieldSpawn [threads=8] [rounds=2000] [slots=64] [itersPerRound=500]
// e.g. -Xmx48m FieldSpawn 8 200 32 200 reliably deadlocked pre-fix.
public class FieldSpawn {
    static class Holder {
        Object ref;
    }

    static class Node {
        final Object payload;
        Node(Object p) { payload = p; }
    }

    public static void main(String[] args) throws Exception {
        try {
            run(args);
        } catch (Throwable t) {
            System.out.println("MAIN THREW: " + t);
            t.printStackTrace(System.out);
            System.out.flush();
            throw t;
        }
    }

    static void run(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        int slots = args.length > 2 ? Integer.parseInt(args[2]) : 64;
        int itersPerRound = args.length > 3 ? Integer.parseInt(args[3]) : 500;

        Holder[] holders = new Holder[slots];
        for (int i = 0; i < slots; i++) holders[i] = new Holder();

        ExecutorService pool = Executors.newFixedThreadPool(threads);
        AtomicLong ops = new AtomicLong();
        AtomicBoolean failed = new AtomicBoolean(false);
        AtomicLong badTypeHits = new AtomicLong();

        long t0 = System.currentTimeMillis();
        for (int r = 0; r < rounds; r++) {
            final int round = r;
            CountDownLatch latch = new CountDownLatch(threads);
            for (int t = 0; t < threads; t++) {
                final int tid = t;
                pool.submit(() -> {
                    try {
                        Random rnd = new Random(tid * 7919L + round);
                        for (int i = 0; i < itersPerRound; i++) {
                            int slot = rnd.nextInt(slots);
                            Holder h = holders[slot];
                            Node n = new Node(new byte[16 + (i & 63)]);
                            n = new Node(n);
                            n = new Node(n);
                            h.ref = n;
                            Object seen = h.ref;
                            if (seen != null && !(seen instanceof Node)) {
                                badTypeHits.incrementAndGet();
                                failed.set(true);
                                System.out.println("BAD TYPE at slot " + slot
                                        + ": " + seen.getClass() + " val=" + seen);
                            }
                            ops.incrementAndGet();
                        }
                    } catch (Throwable t2) {
                        failed.set(true);
                        System.out.println("EXCEPTION in worker: " + t2);
                        t2.printStackTrace();
                    } finally {
                        latch.countDown();
                    }
                });
            }
            latch.await();
            if (r % 5 == 0) {
                // No string concatenation here on purpose: a known separate
                // JIT bug (UnreachedCode uncommon-trap on the dead
                // StringConcatFactory branch, see
                // docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md
                // 2026-07-06 third-session note) silently truncates a hot
                // loop's remaining iterations when string concatenation runs
                // inside it. Print with bare non-concatenating calls instead
                // so THIS probe isn't confounded by that separate bug.
                System.out.print("round ");
                System.out.print(r);
                System.out.print(" ops=");
                System.out.print(ops.get());
                System.out.print(" elapsed=");
                System.out.print(System.currentTimeMillis() - t0);
                System.out.println("ms");
                System.out.flush();
            }
        }
        pool.shutdown();
        pool.awaitTermination(60, TimeUnit.SECONDS);
        System.out.println("DONE ops=" + ops.get() + " failed=" + failed.get()
                + " badTypeHits=" + badTypeHits.get());
    }
}
