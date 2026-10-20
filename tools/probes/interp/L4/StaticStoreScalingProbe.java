// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 7, lane L4: the multi-threaded static-store
// measurement docs/known-issues/interpreter/
// interpreter-L4-proposal-static-field-quickening-FIXED-20260930.md asks for since wave 3.
// Four threads each run a getstatic/putstatic counter loop on a DIFFERENT
// class's statics (int, long, volatile int, reference), first alone, then all
// four at once. Before wave 3 every putstatic took the one VM-wide `statics`
// write lock, so the concurrent phase serialized; since then the steady-state
// store is lock-free (vm_object::set_static_shared) and the site is quickened
// (field_fast::putstatic_site_hit), so the per-thread cost should stay close to
// the single-thread cost.
//
// stdout is deterministic (final counter values only); timing goes to stderr:
//   "alone: N ns/iter  together: M ns/iter/thread  ratio: R"
// Compare under --nojit with CRATONVM_JIT_NO_FIELD_FAST_PATH unset / set, and
// against HotSpot (-Xint for the interpreter comparison); 5 interleaved runs,
// medians. CRATONVM_DBG_GETSTATIC_PROF=1 must show put_hit/get_hit close to
// the loop counts (engagement).
//
// Expected on HotSpot 25:
//   alone: A.n=2000000 B.n=2000000 C.n=2000000 D.last=1999999
//   together: A.n=4000000 B.n=4000000 C.n=4000000 D.last=1999999
//   done
public class StaticStoreScalingProbe {
    static final int ITERS = 2_000_000;

    static final class A {
        static int n;
    }

    static final class B {
        static long n;
    }

    static final class C {
        static volatile int n;
    }

    static final class D {
        static Integer last;
    }

    static void runA() {
        for (int i = 0; i < ITERS; i++) {
            A.n = A.n + 1;
        }
    }

    static void runB() {
        for (int i = 0; i < ITERS; i++) {
            B.n = B.n + 1L;
        }
    }

    static void runC() {
        for (int i = 0; i < ITERS; i++) {
            C.n = C.n + 1;
        }
    }

    static void runD() {
        for (int i = 0; i < ITERS; i++) {
            D.last = i;
        }
    }

    static Runnable task(int k) {
        switch (k) {
            case 0:
                return StaticStoreScalingProbe::runA;
            case 1:
                return StaticStoreScalingProbe::runB;
            case 2:
                return StaticStoreScalingProbe::runC;
            default:
                return StaticStoreScalingProbe::runD;
        }
    }

    static String state() {
        return "A.n=" + A.n + " B.n=" + B.n + " C.n=" + C.n + " D.last=" + D.last;
    }

    public static void main(String[] args) throws Exception {
        long t0 = System.nanoTime();
        for (int k = 0; k < 4; k++) {
            task(k).run();
        }
        long alone = System.nanoTime() - t0;
        System.out.println("alone: " + state());

        Thread[] ts = new Thread[4];
        for (int k = 0; k < 4; k++) {
            ts[k] = new Thread(task(k));
        }
        long t1 = System.nanoTime();
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long together = System.nanoTime() - t1;
        System.out.println("together: " + state());

        double perAlone = (double) alone / (4.0 * ITERS);
        double perTogether = (double) together / ITERS;
        System.err.printf("alone: %.1f ns/iter  together: %.1f ns/iter/thread  ratio: %.2f%n",
                perAlone, perTogether, perTogether / perAlone);
        System.out.println("done");
    }
}
