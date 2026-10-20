// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 26, lane L7: time-to-safepoint of a thread that
// runs interpreted code with no back edge and no allocation, now that the
// dispatch loop polls its thread's own poll word (`LoopPollWord`: bit 0 raised
// by every stop-the-world request right after `stw_requested`) instead of the
// flag, and back edges no longer load the flag at all.
//
// Two worker threads run `fib(27)` over and over: pure recursion, so between
// two of their back edges (one per outer `while` trip) they execute ~630 000
// calls, and they never allocate inside `fib`. Only the dispatch loop's
// per-bytecode poll can stop them promptly. Meanwhile `main` calls
// `System.gc()` ROUNDS times and times each call.
//
// Correctness row (stderr): `gc max ms` and `gc median ms`. If a request
// failed to raise a worker's word, a pause would wait for the worker's next
// back edge (a whole `fib(27)`, tens to hundreds of ms under --nojit) and the
// max would jump to that scale, with `gc max ms` near `fib ms`. Expected: the
// same as the previous build, NOT a step up. `fib ms` (stderr) is the cost of
// the recursion itself: expected equal or slightly down (one load + compare
// fewer per bytecode). For scale, HotSpot 25 on the Windows dev box: gc
// median 11 ms / max 71 ms (a full collection per call) with fib 1.8 ms;
// under -Xint gc median 14 ms / max 83 ms with fib 66 ms.
//
// stdout is deterministic and identical on HotSpot 25:
//     fib(27)=196418
//     gc rounds=40 workers=2 done
//
//     cratonvm --java-home <jdk25> --nojit -cp <dir> L7W26PollWordSafepointBench
//     (also without --nojit, and with --compatible: same stdout)
public class L7W26PollWordSafepointBench {
    static final int ROUNDS = 40;
    static final int WORKERS = 2;
    static volatile boolean stop;
    static volatile long fibNanos;

    static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    public static void main(String[] args) throws Exception {
        int check = fib(27);
        System.out.println("fib(27)=" + check);
        Thread[] workers = new Thread[WORKERS];
        for (int w = 0; w < WORKERS; w++) {
            workers[w] = new Thread(() -> {
                long best = Long.MAX_VALUE;
                while (!stop) {
                    long t0 = System.nanoTime();
                    int r = fib(27);
                    long dt = System.nanoTime() - t0;
                    if (r != 196418) {
                        throw new AssertionError("fib(27)=" + r);
                    }
                    if (dt < best) {
                        best = dt;
                        fibNanos = best;
                    }
                }
            });
            workers[w].setDaemon(true);
            workers[w].start();
        }
        // Let both workers get deep into their first recursion.
        Thread.sleep(200);
        long[] gcNanos = new long[ROUNDS];
        for (int i = 0; i < ROUNDS; i++) {
            long t0 = System.nanoTime();
            System.gc();
            gcNanos[i] = System.nanoTime() - t0;
            Thread.sleep(5);
        }
        stop = true;
        for (Thread t : workers) {
            t.join();
        }
        java.util.Arrays.sort(gcNanos);
        System.out.println("gc rounds=" + ROUNDS + " workers=" + WORKERS + " done");
        System.err.printf("gc max ms    %8.3f%n", gcNanos[ROUNDS - 1] / 1e6);
        System.err.printf("gc median ms %8.3f%n", gcNanos[ROUNDS / 2] / 1e6);
        System.err.printf("fib ms       %8.3f (best fib(27) on a worker)%n", fibNanos / 1e6);
    }
}
