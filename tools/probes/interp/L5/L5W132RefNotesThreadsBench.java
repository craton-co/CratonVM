// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// javac -d out L5W132RefNotesThreadsBench.java
// java -cp out L5W132RefNotesThreadsBench [threads-list, default 1,4]
//
// Interpreter round i6, wave 132, lane L5 -- the thread arm that
// L5W77StreamSplitBench's `refs` family lacks, for
// `docs/internal/proposals/interpreter/i77-L5-proposal-reference-registry-notes-without-the-global-mutex-20261110.md`.
//
// Question: does the per-VM `ref_processor` mutex (taken by
// `ref_discovery::note_allocation` at every Reference allocation and again by
// `mark_reference_manually_enqueued` at every application `clear()`) serialise
// threads that allocate references? Each row runs the same body on 1 thread
// and on 4 threads AT ONCE (each thread does ITERS calls a round; long-lived
// workers, a barrier at each round's start and end, so thread start-up is not
// timed). If the bodies scale, ns/op-per-thread is the same for t=1 and t=4
// (on an otherwise idle machine with >= 4 P-cores). A t4/t1 ratio well above
// the ratio of a control body that allocates no Reference (`object-new`)
// means the threads wait on something shared: then profile it (the
// processor mutex under `note_allocation`).
//
// Rows (bodies as in L5W77StreamSplitBench, each a static method called once
// per iteration so that the JIT compiles it by invocation count):
//   object-new              new Object() kept in a per-thread slot (control:
//                           no Reference, allocation only).
//   weak-new                new WeakReference(o, q): one allocation note.
//   weak-clear-queued       new WeakReference(o, q).clear(): note + retire.
//   weak-clear-unqueued     new WeakReference(o).clear().
//   phantom-clear           new PhantomReference(o, q).clear().
//   cleaner-register-clean  Cleaner.register(o, action).clean().
//
// stdout: per row and thread count one deterministic line
//   "<row> t=<T> ops=<n>"            (n = successful ops per round, T*ITERS)
// and timing lines, which start with TIME and are excluded from comparisons:
//   "TIME <row> t=<T> <ns> ns/op-per-thread (median of rounds 1..6)"
//   "TIME <row> t4/t1 <ratio>"       (only when the list holds 1 and 4)
//
// Run ($B the cratonvm binary, $J the JDK 25 bin directory; pin to P-cores on
// the hybrid host, e.g. `start /affinity FFFF`; three repetitions, medians):
//   $J/javac -d out L5W132RefNotesThreadsBench.java
//   for JIT in "" --nojit; do for MODE in --jdk-only --compatible; do
//     $B --java-home $J/.. $JIT $MODE -cp out L5W132RefNotesThreadsBench
//   done; done
//   $J/java -cp out L5W132RefNotesThreadsBench          # HotSpot (and -Xint)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// object-new t=1 ops=20000
// object-new t=4 ops=80000
// weak-new t=1 ops=20000
// weak-new t=4 ops=80000
// weak-clear-queued t=1 ops=20000
// weak-clear-queued t=4 ops=80000
// weak-clear-unqueued t=1 ops=20000
// weak-clear-unqueued t=4 ops=80000
// phantom-clear t=1 ops=20000
// phantom-clear t=4 ops=80000
// cleaner-register-clean t=1 ops=20000
// cleaner-register-clean t=4 ops=80000
// HOTSPOT_EXPECTED_END

import java.lang.ref.Cleaner;
import java.lang.ref.PhantomReference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.util.Arrays;
import java.util.concurrent.CyclicBarrier;

public class L5W132RefNotesThreadsBench {
    static final int ROUNDS = 7;
    static final int ITERS = 20_000;

    static final String[] ROWS = {
        "object-new",
        "weak-new",
        "weak-clear-queued",
        "weak-clear-unqueued",
        "phantom-clear",
        "cleaner-register-clean",
    };

    static final Cleaner CLEANER = Cleaner.create();
    static final ReferenceQueue<Object> QUEUE = new ReferenceQueue<>();

    /** Per-thread state: a success counter and a keep slot. */
    static final class Slot {
        long count;
        Object keep;
        final Runnable action = () -> count++;
    }

    static long objectNew(Slot s) {
        Object o = new Object();
        s.keep = o;
        s.count++;
        return 1;
    }

    static long weakNew(Slot s) {
        Object o = new Object();
        WeakReference<Object> r = new WeakReference<>(o, QUEUE);
        if (r.refersTo(o)) {
            s.count++;
        }
        s.keep = o;
        return 1;
    }

    static long weakClearQueued(Slot s) {
        WeakReference<Object> r = new WeakReference<>(new Object(), QUEUE);
        r.clear();
        if (r.get() == null) {
            s.count++;
        }
        return 1;
    }

    static long weakClearUnqueued(Slot s) {
        WeakReference<Object> r = new WeakReference<>(new Object());
        r.clear();
        if (r.get() == null) {
            s.count++;
        }
        return 1;
    }

    static long phantomClear(Slot s) {
        Object o = new Object();
        PhantomReference<Object> r = new PhantomReference<>(o, QUEUE);
        r.clear();
        if (!r.refersTo(o)) {
            s.count++;
        }
        return 1;
    }

    static long cleanerRegisterClean(Slot s) {
        Cleaner.Cleanable c = CLEANER.register(new Object(), s.action);
        c.clean();
        return 1;
    }

    static void runBody(int body, Slot s, int iters) {
        switch (body) {
            case 0: for (int i = 0; i < iters; i++) { objectNew(s); } break;
            case 1: for (int i = 0; i < iters; i++) { weakNew(s); } break;
            case 2: for (int i = 0; i < iters; i++) { weakClearQueued(s); } break;
            case 3: for (int i = 0; i < iters; i++) { weakClearUnqueued(s); } break;
            case 4: for (int i = 0; i < iters; i++) { phantomClear(s); } break;
            case 5: for (int i = 0; i < iters; i++) { cleanerRegisterClean(s); } break;
            default: throw new IllegalArgumentException("body " + body);
        }
    }

    /** One row on `t` threads; returns the median ns/op-per-thread of rounds 1..ROUNDS-1. */
    static long row(int body, int t) throws Exception {
        final CyclicBarrier barrier = new CyclicBarrier(t + 1);
        final Slot[] slots = new Slot[t];
        final Throwable[] failure = new Throwable[1];
        Thread[] workers = new Thread[t];
        for (int k = 0; k < t; k++) {
            final Slot s = new Slot();
            slots[k] = s;
            workers[k] = new Thread(() -> {
                try {
                    for (int r = 0; r < ROUNDS; r++) {
                        barrier.await();
                        runBody(body, s, ITERS);
                        barrier.await();
                        if (r == 0) {
                            // The warm-up round does not count; main reads
                            // nothing between the two awaits of round 1.
                            s.count = 0;
                        }
                    }
                } catch (Throwable e) {
                    synchronized (failure) {
                        failure[0] = e;
                    }
                }
            }, "l5w132-" + k);
            workers[k].setDaemon(true);
            workers[k].start();
        }
        long[] perOp = new long[ROUNDS];
        for (int r = 0; r < ROUNDS; r++) {
            barrier.await();
            long t0 = System.nanoTime();
            barrier.await();
            perOp[r] = (System.nanoTime() - t0) / ITERS;
        }
        for (Thread w : workers) {
            w.join();
        }
        if (failure[0] != null) {
            throw new RuntimeException("worker failed", failure[0]);
        }
        long total = 0;
        for (Slot s : slots) {
            total += s.count;
        }
        System.out.println(ROWS[body] + " t=" + t + " ops=" + (total / (ROUNDS - 1)));
        long[] later = Arrays.copyOfRange(perOp, 1, ROUNDS);
        Arrays.sort(later);
        long median = later[later.length / 2];
        System.out.println("TIME " + ROWS[body] + " t=" + t + " " + median
                + " ns/op-per-thread (median of rounds 1.." + (ROUNDS - 1) + ")");
        return median;
    }

    public static void main(String[] args) throws Exception {
        String list = args.length > 0 ? args[0] : "1,4";
        String[] parts = list.split(",");
        int[] counts = new int[parts.length];
        for (int i = 0; i < parts.length; i++) {
            counts[i] = Integer.parseInt(parts[i].trim());
        }
        for (int body = 0; body < ROWS.length; body++) {
            long one = -1;
            long four = -1;
            for (int t : counts) {
                long ns = row(body, t);
                if (t == 1) {
                    one = ns;
                } else if (t == 4) {
                    four = ns;
                }
            }
            if (one > 0 && four >= 0) {
                long hundredths = (four * 100 + one / 2) / one;
                System.out.println("TIME " + ROWS[body] + " t4/t1 " + (hundredths / 100) + "."
                        + (hundredths % 100 < 10 ? "0" : "") + (hundredths % 100));
            }
        }
    }
}
