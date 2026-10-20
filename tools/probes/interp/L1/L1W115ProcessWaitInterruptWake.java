// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L1 -- `Thread.interrupt` of a thread
// blocked in `Process.waitFor()` / `waitFor(long, TimeUnit)` on a running
// child, for the A/B of `CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT` (default off;
// Windows only: the wait on the child's process handle also waits on the
// thread's interrupt-wake event, as the JDK's Windows natives wait on
// `JVM_GetThreadInterruptEvent()`). Page:
// docs/internal/fixed-bugs/interpreter-i114-L1-proposal-windows-process-wait-wakes-on-interrupt-DONE-20261019.md.
//
// Each row must print the same with the switch off and on, in both modes,
// JIT and --nojit:
//   * the interrupted waiter throws InterruptedException with no message and
//     its interrupt status cleared; the child is still alive;
//   * the same waiter, re-waiting after the exception, returns the child's
//     exit once it is destroyed (no stale wake: its next wait blocks);
//   * an `unpark` of the waiter (which raises the same event) does not end
//     `waitFor` (it is not an interrupt), and the permit it gave is still
//     there for the waiter's next `parkNanos` (returns at once).
//
// TIME lines (not compared): the median microseconds from `t.interrupt()` to
// the waiter's catch, untimed and timed (HotSpot: tens of us; switch off: up
// to 10 ms, median ~5 ms; switch on: expected near HotSpot).
//
// Run: java L1W115ProcessWaitInterruptWake
//      cratonvm [--compatible] L1W115ProcessWaitInterruptWake
//      CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT=1 cratonvm [--compatible] L1W115ProcessWaitInterruptWake
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 HotSpot, Windows, run)
// waitFor(): exc=java.lang.InterruptedException msg=null flag=false alive=true rounds=7
// waitFor(60s): exc=java.lang.InterruptedException msg=null flag=false alive=true rounds=7
// re-wait after interrupt: returned=true alive=false
// unpark does not end waitFor: still-waiting=true permit-kept=true
// done
// HOTSPOT_EXPECTED_END

import java.util.Arrays;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

public class L1W115ProcessWaitInterruptWake {
    static final boolean WINDOWS = System.getProperty("os.name").startsWith("Windows");
    static final int ROUNDS = 7;

    static Process longChild() throws Exception {
        ProcessBuilder pb = WINDOWS
                ? new ProcessBuilder("ping", "-n", "60", "127.0.0.1")
                : new ProcessBuilder("sleep", "60");
        return pb.redirectOutput(ProcessBuilder.Redirect.DISCARD).start();
    }

    static void waitFor(Process p, boolean timed) throws InterruptedException {
        if (timed) {
            p.waitFor(60, TimeUnit.SECONDS);
        } else {
            p.waitFor();
        }
    }

    /** One row: ROUNDS interrupts of a waiter blocked in waitFor on one child. */
    static void row(boolean timed, long[] lat) throws Exception {
        Process p = longChild();
        String exc = "none";
        String msg = "none";
        boolean flag = true;
        int rounds = 0;
        for (int i = 0; i < ROUNDS; i++) {
            Object[] got = new Object[3];
            long[] caught = new long[1];
            Thread w = new Thread(() -> {
                try {
                    waitFor(p, timed);
                    got[0] = "returned";
                } catch (InterruptedException e) {
                    caught[0] = System.nanoTime();
                    got[0] = e.getClass().getName();
                    got[1] = String.valueOf(e.getMessage());
                    got[2] = Thread.currentThread().isInterrupted();
                }
            }, "waiter");
            w.start();
            // A native wait reads RUNNABLE: give it time to block.
            Thread.sleep(40);
            long t0 = System.nanoTime();
            w.interrupt();
            w.join(30_000);
            lat[i] = caught[0] - t0;
            if (got[0] != null && !"returned".equals(got[0])) {
                rounds++;
                exc = (String) got[0];
                msg = (String) got[1];
                flag = (Boolean) got[2];
            }
        }
        boolean alive = p.isAlive();
        p.destroyForcibly().waitFor();
        System.out.println((timed ? "waitFor(60s)" : "waitFor()") + ": exc=" + exc + " msg=" + msg
                + " flag=" + flag + " alive=" + alive + " rounds=" + rounds);
    }

    static long medianUs(long[] a) {
        long[] c = a.clone();
        Arrays.sort(c);
        return c[c.length / 2] / 1_000;
    }

    public static void main(String[] args) throws Exception {
        long[] untimed = new long[ROUNDS];
        long[] timed = new long[ROUNDS];
        row(false, untimed);
        row(true, timed);

        // The same thread: interrupted out of waitFor, then waits again until
        // the child is destroyed. A signal left on its event would end the
        // second wait at once (it would then loop on, so the visible answer
        // is still right; the row checks the second wait returns normally).
        Process p = longChild();
        boolean[] res = new boolean[1];
        Thread w = new Thread(() -> {
            try {
                p.waitFor();
            } catch (InterruptedException e) {
                // expected
            }
            try {
                p.waitFor();
                res[0] = true;
            } catch (InterruptedException e) {
                res[0] = false;
            }
        }, "re-waiter");
        w.start();
        Thread.sleep(40);
        w.interrupt();
        Thread.sleep(60);
        p.destroyForcibly();
        w.join(30_000);
        System.out.println("re-wait after interrupt: returned=" + res[0] + " alive=" + p.isAlive());

        // An unpark is not an interrupt: waitFor keeps waiting, and the permit
        // stays for the thread's next park.
        Process q = longChild();
        boolean[] permit = new boolean[1];
        Thread u = new Thread(() -> {
            try {
                q.waitFor();
            } catch (InterruptedException e) {
                return;
            }
            long s = System.nanoTime();
            LockSupport.parkNanos(5_000_000_000L);
            permit[0] = System.nanoTime() - s < 2_000_000_000L;
        }, "unparked-waiter");
        u.start();
        Thread.sleep(40);
        LockSupport.unpark(u);
        Thread.sleep(60);
        boolean stillWaiting = u.isAlive();
        q.destroyForcibly();
        u.join(30_000);
        System.out.println("unpark does not end waitFor: still-waiting=" + stillWaiting
                + " permit-kept=" + permit[0]);

        System.out.println("TIME untimed interrupt->catch median-us=" + medianUs(untimed));
        System.out.println("TIME timed interrupt->catch median-us=" + medianUs(timed));
        System.out.println("done");
    }
}
