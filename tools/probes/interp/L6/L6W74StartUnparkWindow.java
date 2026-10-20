// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L6 -- behavioural probe for
// docs/internal/fixed-bugs/interpreter-i73-L6-unpark-in-the-thread-start-window-is-lost-20261106-FIXED-20261107.md:
// an unpark of a new thread that lands while Thread.start() is still running
// (after the thread reads alive -- eetop set -- and before start() returns)
// must end the thread's first park, as HotSpot's Unsafe_Unpark reaches the
// thread's Parker once JavaThread::prepare has run.
//
// Each round: main starts a child that parks until its round's `go` is set; a
// signaller thread spins until the child reads alive, sets `go` and unparks the
// child ONCE -- usually while main is still inside start(). A lost unpark
// leaves the child in a 1 s park that times out (counted as "lost"; without the
// timeout the program would hang). Before the fix (a second ParkState
// installed late in start0) a permit given in the window was dropped.
//
// Run, each mode:
//   cratonvm L6W74StartUnparkWindow
//   cratonvm --compatible L6W74StartUnparkWindow
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// rounds=300 lost=0
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L6W74StartUnparkWindow {
    static final int ROUNDS = 300;
    static final int MAX_LOST = 5;
    static final long PARK_NS = 1_000_000_000L;

    static final class Round {
        final Thread thread;
        volatile boolean go;

        Round(Thread thread) {
            this.thread = thread;
        }
    }

    static volatile Round current;
    static volatile boolean finished;
    static final AtomicInteger lost = new AtomicInteger();

    public static void main(String[] args) throws Exception {
        Thread signaller = new Thread(() -> {
            Round handled = null;
            while (!finished) {
                Round r = current;
                if (r == null || r == handled) {
                    Thread.onSpinWait();
                    continue;
                }
                // The child cannot finish before `go`, so alive comes and stays.
                while (!r.thread.isAlive()) {
                    Thread.onSpinWait();
                }
                r.go = true;
                LockSupport.unpark(r.thread);
                handled = r;
            }
        }, "w74-signaller");
        signaller.setDaemon(true);
        signaller.start();

        int rounds = 0;
        for (int i = 0; i < ROUNDS && lost.get() < MAX_LOST; i++) {
            Round[] box = new Round[1];
            Thread child = new Thread(() -> {
                Round me = box[0];
                while (!me.go) {
                    long t0 = System.nanoTime();
                    LockSupport.parkNanos(PARK_NS);
                    // The signaller unparks within microseconds of `alive`:
                    // a park that ran out its whole time missed that unpark.
                    if (System.nanoTime() - t0 >= PARK_NS * 9 / 10) {
                        lost.incrementAndGet();
                    }
                }
            }, "w74-child-" + i);
            Round r = new Round(child);
            box[0] = r;
            current = r;
            child.start();
            child.join();
            rounds++;
        }
        finished = true;
        System.out.println("rounds=" + rounds + " lost=" + lost.get());
        System.out.println("done");
    }
}
