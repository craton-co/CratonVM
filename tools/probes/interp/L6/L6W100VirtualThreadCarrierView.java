// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L6 -- what Java sees of a virtual
// thread's carrier and of a pinned virtual thread, against HotSpot 25.
//
// `VirtualThread.toString()` is "VirtualThread[#id(,name)]/" followed, when
// mounted, by the CARRIER's state in lower case and "@" + the carrier's name
// (JDK 25 `appendCarrierInfo`), else by the virtual thread's own state. So:
//  * on itself: "runnable@ForkJoinPool-1-worker-1" (the carrier runs it);
//  * parked and unmounted, seen from main: "waiting";
//  * parked while PINNED (a park inside a class initializer, which pins),
//    seen from main: still mounted, the carrier parked: "waiting@ForkJoinPool-1-worker-1";
//  * terminated: "terminated".
// The carrier is the scheduler pool's only worker (parallelism = maxPoolSize
// = 1 set before the first virtual thread), a jdk.internal.misc.CarrierThread
// named "ForkJoinPool-1-worker-1". Ids are masked.
//
// JDK 25 prints nothing for a pinned park (jdk.tracePinnedThreads is gone);
// the stdout below must have no other line. `Thread.onSpinWait()` is a
// no-op hint.
//
// HOTSPOT_EXPECTED_BEGIN
// self: VirtualThread[#N,w100-self]/runnable@ForkJoinPool-1-worker-1
// self-unnamed: VirtualThread[#N]/runnable@ForkJoinPool-1-worker-1
// parked: VirtualThread[#N,w100-parked]/waiting state=WAITING
// pinned: VirtualThread[#N,w100-pinned]/waiting@ForkJoinPool-1-worker-1 state=WAITING
// pinned-done: true
// terminated: VirtualThread[#N,w100-parked]/terminated
// spin: ok
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.LockSupport;

public class L6W100VirtualThreadCarrierView {
    static volatile boolean go;
    static volatile Thread pinnedThread;

    static String mask(String s) {
        return s.replaceAll("#\\d+", "#N");
    }

    /** Parks inside its own initializer: a pinned park on JDK 25. */
    static final class ParksInClinit {
        static final boolean DONE;

        static {
            while (!go) {
                LockSupport.park();
            }
            DONE = true;
        }
    }

    static void waitFor(Thread t, Thread.State s) throws InterruptedException {
        while (t.getState() != s) {
            Thread.sleep(1);
        }
    }

    public static void main(String[] args) throws Exception {
        System.setProperty("jdk.virtualThreadScheduler.parallelism", "1");
        System.setProperty("jdk.virtualThreadScheduler.maxPoolSize", "1");

        AtomicReference<String> self = new AtomicReference<>();
        Thread.ofVirtual().name("w100-self").start(() ->
            self.set(Thread.currentThread().toString())).join();
        System.out.println("self: " + mask(self.get()));
        Thread.ofVirtual().start(() -> self.set(Thread.currentThread().toString())).join();
        System.out.println("self-unnamed: " + mask(self.get()));

        Thread parked = Thread.ofVirtual().name("w100-parked").start(() -> {
            while (!go) {
                LockSupport.park();
            }
        });
        waitFor(parked, Thread.State.WAITING);
        System.out.println("parked: " + mask(parked.toString()) + " state=" + parked.getState());

        AtomicReference<Boolean> done = new AtomicReference<>();
        Thread pinned = Thread.ofVirtual().name("w100-pinned").unstarted(() ->
            done.set(ParksInClinit.DONE));
        pinnedThread = pinned;
        pinned.start();
        waitFor(pinned, Thread.State.WAITING);
        System.out.println("pinned: " + mask(pinned.toString()) + " state=" + pinned.getState());

        go = true;
        LockSupport.unpark(pinned);
        pinned.join();
        System.out.println("pinned-done: " + done.get());
        LockSupport.unpark(parked);
        parked.join();
        System.out.println("terminated: " + mask(parked.toString()));

        for (int i = 0; i < 1000; i++) {
            Thread.onSpinWait();
        }
        System.out.println("spin: ok");
        System.out.println("done");
    }
}
