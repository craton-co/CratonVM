// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L6 -- page
// i116-L9-strict-monitorexit-throws-on-an-understated-record: a frame that
// takes a lock INTERPRETED, then runs a hot loop that OSRs, and leaves the
// OSR'd body through an OSR exit (a class loaded mid-loop breaks the
// single-implementor assumption the loop's virtual call was compiled under),
// must release its own lock at its `monitorexit` -- with
// CRATONVM_MONITOREXIT_FRAME_STRICT=1 too. Before wave 117 the single-pass
// exit transfer left the frame's held-monitor record EMPTY, and the strict arm
// threw IllegalMonitorStateException at the frame's own `monitorexit`, which
// javac's catch-any handler catches and re-runs forever (`timeout`).
//
// Each row runs on its own thread with a 20 s watchdog.
//
// Run, each mode, JIT and --nojit, and with the single-pass OSR tier
// (e.g. CRATONVM_JIT_IR_OSR_ENTRY=0) as well as the default:
//   cratonvm L6W117StrictMonitorExitAfterOsrExit
//   CRATONVM_MONITOREXIT_FRAME_STRICT=1 cratonvm L6W117StrictMonitorExitAfterOsrExit
//   CRATONVM_DBG_MONITOR_LENIENCY=1 cratonvm L6W117StrictMonitorExitAfterOsrExit
//     (stderr: a `released` line names `locked` when the exit left the
//      record understated; with the strict arm the same line says
//      `released` too, since wave 117, instead of `strict-throw`)
//
// HOTSPOT_EXPECTED_BEGIN
// osr-exit: sum=true heldInside=true freeAfter=true imse=0
// nested: sum=true heldInside=true freeAfter=true imse=0
// HOTSPOT_EXPECTED_END

public class L6W117StrictMonitorExitAfterOsrExit {
    abstract static class Step {
        abstract int apply(int x);
    }

    static final class Plus extends Step {
        @Override
        int apply(int x) {
            return x + 1;
        }
    }

    /** Loaded only when the loop is well past its OSR point. */
    static final class Times extends Step {
        @Override
        int apply(int x) {
            return x * 1;
        }
    }

    static volatile Step late;

    static final Object LOCK = new Object();
    static final Object OUTER = new Object();

    static boolean heldInside;
    static int imse;

    /** The lock is taken interpreted, before the loop gets hot. */
    static long locked(int n) {
        long s = 0;
        synchronized (LOCK) {
            Step step = new Plus();
            for (int i = 0; i < n; i++) {
                s += step.apply(i);
                if (i == n / 2) {
                    // A second implementor: the OSR'd body's devirtualized
                    // `apply` is no longer valid, and it exits.
                    late = new Times();
                    step = late;
                }
            }
            heldInside = Thread.holdsLock(LOCK);
        }
        return s;
    }

    /** The same, two locks deep (the outer one taken in the caller frame). */
    static long nested(int n) {
        synchronized (OUTER) {
            return locked(n);
        }
    }

    static long expected(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i <= n / 2 ? i + 1 : i;
        }
        return s;
    }

    static boolean freeElsewhere(Object lock) throws InterruptedException {
        boolean[] got = {false};
        Thread t = new Thread(() -> {
            synchronized (lock) {
                got[0] = true;
            }
        });
        t.setDaemon(true);
        t.start();
        t.join(20_000);
        return got[0];
    }

    interface Row {
        String run() throws Exception;
    }

    static String withWatchdog(Row row) throws InterruptedException {
        String[] result = {"timeout"};
        Thread t = new Thread(() -> {
            try {
                result[0] = row.run();
            } catch (Throwable e) {
                result[0] = "threw " + e.getClass().getName();
            }
        });
        t.setDaemon(true);
        t.start();
        t.join(20_000);
        return t.isAlive() ? "timeout" : result[0];
    }

    public static void main(String[] args) throws Exception {
        int n = 400_000;
        System.out.println("osr-exit: " + withWatchdog(() -> {
            long s;
            try {
                s = locked(n);
            } catch (IllegalMonitorStateException e) {
                imse++;
                s = -1;
            }
            return "sum=" + (s == expected(n)) + " heldInside=" + heldInside
                    + " freeAfter=" + freeElsewhere(LOCK) + " imse=" + imse;
        }));
        System.out.println("nested: " + withWatchdog(() -> {
            long s;
            try {
                s = nested(n);
            } catch (IllegalMonitorStateException e) {
                imse++;
                s = -1;
            }
            return "sum=" + (s == expected(n)) + " heldInside=" + heldInside
                    + " freeAfter=" + (freeElsewhere(LOCK) && freeElsewhere(OUTER)) + " imse=" + imse;
        }));
    }
}
