// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.time.Duration;
import java.util.concurrent.locks.LockSupport;

/**
 * gci i27/vt (item B of gci-i23pk-park-permit-residuals-windows-and-stand-ins):
 * does each LockSupport entry take the permit, or not, as JDK 25's bodies and
 * HotSpot 25's Parker::park do?
 *
 *   parkNanos(nanos) / parkNanos(blocker, nanos): `if (nanos > 0)` comes
 *     before U.park, so a non-positive time never parks: the permit is KEPT,
 *     interrupted or not.
 *   parkUntil(deadline) / parkUntil(blocker, deadline): always U.park(true,
 *     deadline), which takes the permit first (Linux) and then returns for a
 *     zero, negative or elapsed deadline: the permit is CONSUMED.
 *   parkNanos(positive) while interrupted: U.park takes the permit, then
 *     returns for the interrupt: CONSUMED.
 *
 *   GciVt27LockSupportOrderProbe platform|virtual
 *
 * Each case sets up its state, makes its first call, clears the interrupt,
 * then parkNanos(300 ms): the second park waited (>= 150 ms; the permit was
 * consumed) or returned at once (the permit was kept).
 *
 * stdout (deterministic, the same on HotSpot 25 Linux):
 *   vt27 <mode> nanos0-intr second=at-once
 *   vt27 <mode> nanosneg second=at-once
 *   vt27 <mode> nanos0-blocker-intr second=at-once
 *   vt27 <mode> nanos-intr second=waited
 *   vt27 <mode> until0 second=waited
 *   vt27 <mode> until0-blocker second=waited
 *   vt27 <mode> untilpast second=waited
 * stderr: the measured milliseconds.
 */
public class GciVt27LockSupportOrderProbe {
    static final long SECOND_NS = 300_000_000L;
    static final long WAITED_MS = 150L;
    static final Object BLOCKER = new Object();

    static void first(String name) {
        switch (name) {
            case "nanos0-intr":
                LockSupport.parkNanos(0L);
                break;
            case "nanosneg":
                LockSupport.parkNanos(-5L);
                break;
            case "nanos0-blocker-intr":
                LockSupport.parkNanos(BLOCKER, 0L);
                break;
            case "nanos-intr":
                LockSupport.parkNanos(BLOCKER, 1_000L);
                break;
            case "until0":
                LockSupport.parkUntil(0L);
                break;
            case "until0-blocker":
                LockSupport.parkUntil(BLOCKER, 0L);
                break;
            case "untilpast":
                LockSupport.parkUntil(BLOCKER, System.currentTimeMillis() - 1_000L);
                break;
            default:
                throw new IllegalArgumentException(name);
        }
    }

    static String runCase(String name) {
        Thread me = Thread.currentThread();
        if (name.endsWith("-intr")) {
            me.interrupt();
        }
        LockSupport.unpark(me);
        first(name);
        boolean wasInterrupted = Thread.interrupted();
        long s = System.nanoTime();
        LockSupport.parkNanos(SECOND_NS);
        long ms = (System.nanoTime() - s) / 1_000_000L;
        // Leave no permit behind for the next case, whatever happened.
        LockSupport.unpark(me);
        LockSupport.park();
        Thread.interrupted();
        System.err.println("[vt27] " + name + " firstInterrupted=" + wasInterrupted + " secondMs=" + ms);
        return ms >= WAITED_MS ? "waited" : "at-once";
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "platform";
        String[] cases = {
            "nanos0-intr", "nanosneg", "nanos0-blocker-intr", "nanos-intr",
            "until0", "until0-blocker", "untilpast"
        };
        String[] answers = new String[cases.length];
        Runnable body = () -> {
            for (int i = 0; i < cases.length; i++) {
                answers[i] = runCase(cases[i]);
            }
        };
        if (mode.equals("virtual")) {
            Thread vt = Thread.ofVirtual().name("vt27-vt").start(body);
            if (!vt.join(Duration.ofSeconds(30))) {
                System.out.println("vt27 virtual hung");
                return;
            }
        } else {
            Thread t = new Thread(body, "vt27-platform");
            t.start();
            t.join();
        }
        for (int i = 0; i < cases.length; i++) {
            System.out.println("vt27 " + mode + " " + cases[i] + " second=" + answers[i]);
        }
    }
}
