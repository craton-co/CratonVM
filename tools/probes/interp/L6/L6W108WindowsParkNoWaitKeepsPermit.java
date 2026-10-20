// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L6 -- page
// docs/known-issues/gc/gci-i23pk-park-permit-residuals-windows-and-stand-ins-20261015.md
// item A. HotSpot 25's Parker::park is platform-dependent for a park that
// does not wait: on Windows (os_windows.cpp) a negative time or an elapsed
// absolute deadline returns BEFORE the park event (its permit) is looked
// at, so the permit is KEPT; on Linux (os_posix.cpp) `_counter` is taken
// first, so it is CONSUMED. Each case: unpark(self), the no-wait park, then
// parkNanos(1 s) -- at once (permit kept) or after ~1 s (consumed). The line
// prints whether the answer is the one HotSpot gives on THIS OS, so the
// expected output is the same everywhere.
//
// Cases: parkUntil(0), parkUntil(now - 1000 ms), parkUntil(Long.MIN_VALUE),
// parkUntil(0) with an interrupt pending (Windows decodes the time before the
// interrupt test too), and on both OSes parkNanos(-1) (JDK: no U.park at all,
// the permit is kept everywhere).
//
// HOTSPOT_EXPECTED_BEGIN
// parkUntil(0): platform answer=true
// parkUntil(past): platform answer=true
// parkUntil(MIN): platform answer=true
// parkUntil(0) interrupted: platform answer=true
// parkNanos(-1) kept permit: true
// done
// HOTSPOT_EXPECTED_END

import java.util.concurrent.locks.LockSupport;

public class L6W108WindowsParkNoWaitKeepsPermit {
    static final boolean WINDOWS = System.getProperty("os.name", "").startsWith("Windows");

    interface Case {
        void run();
    }

    /** True when the permit survived `first`. */
    static boolean keptPermit(Case first, boolean interrupt) {
        LockSupport.unpark(Thread.currentThread());
        if (interrupt) {
            Thread.currentThread().interrupt();
        }
        first.run();
        Thread.interrupted();
        long t0 = System.nanoTime();
        LockSupport.parkNanos(1_000_000_000L);
        long ms = (System.nanoTime() - t0) / 1_000_000;
        // Leave no permit behind for the next case.
        LockSupport.unpark(Thread.currentThread());
        LockSupport.parkNanos(1L);
        Thread.interrupted();
        return ms < 500;
    }

    static void row(String label, Case first, boolean interrupt) {
        boolean kept = keptPermit(first, interrupt);
        System.out.println(label + ": platform answer=" + (kept == WINDOWS));
    }

    public static void main(String[] args) {
        row("parkUntil(0)", () -> LockSupport.parkUntil(0L), false);
        row("parkUntil(past)", () -> LockSupport.parkUntil(System.currentTimeMillis() - 1000), false);
        row("parkUntil(MIN)", () -> LockSupport.parkUntil(Long.MIN_VALUE), false);
        row("parkUntil(0) interrupted", () -> LockSupport.parkUntil(0L), true);
        System.out.println("parkNanos(-1) kept permit: "
                + keptPermit(() -> LockSupport.parkNanos(-1L), false));
        System.out.println("done");
    }
}
