// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 80, lane L6 --
// docs/internal/fixed-bugs/interpreter-i70-L1-synthetic-process-waitfor-ignores-interrupt-FIXED-20261112.md
//
// A thread waiting for a running child is interruptible on HotSpot: UNIX
// ProcessImpl.waitFor() awaits a ReentrantLock condition, waitFor(long,
// TimeUnit) loops on awaitNanos; both throw `new InterruptedException()` (no
// message) and clear the status. A pre-interrupted waitFor() on a child that
// has already exited answers its exit code (`while (!hasExited)` never awaits)
// and the status stays set. Under --compatible a VM-spawned child is the
// synthetic Process, whose stand-ins ignored the interrupt until wave 80.
// Linux host (`sleep 30` / `true` children).
//
// Run, each mode:
//   cratonvm L6W80ProcessWaitForInterrupt
//   cratonvm --compatible L6W80ProcessWaitForInterrupt
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// waitFor(): java.lang.InterruptedException fast=true interrupted-after=false
// waitFor(60s): java.lang.InterruptedException fast=true interrupted-after=false
// pre-interrupted waitFor(): java.lang.InterruptedException interrupted-after=false
// pre-interrupted waitFor(60s): java.lang.InterruptedException interrupted-after=false
// exited child, pre-interrupted waitFor(): 0 interrupted-after=true
// exited child, pre-interrupted waitFor(60s): true interrupted-after=true
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.TimeUnit;

public class L6W80ProcessWaitForInterrupt {
    interface Wait {
        Object run(Process p) throws InterruptedException;
    }

    static String outcome(Wait w, Process p) {
        try {
            return String.valueOf(w.run(p));
        } catch (InterruptedException e) {
            return e.toString();
        }
    }

    /** Interrupts a thread waiting on a running child 200 ms into the wait. */
    static void interruptRow(String label, Wait w) throws Exception {
        Process p = new ProcessBuilder("sleep", "30").start();
        String[] out = new String[1];
        boolean[] after = new boolean[1];
        Thread t = new Thread(() -> {
            out[0] = outcome(w, p);
            after[0] = Thread.currentThread().isInterrupted();
        }, label);
        t.setDaemon(true);
        t.start();
        Thread.sleep(200);
        long start = System.nanoTime();
        t.interrupt();
        t.join(10_000);
        long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - start);
        System.out.println(label + ": " + out[0] + " fast=" + (!t.isAlive() && ms < 2000)
                + " interrupted-after=" + after[0]);
        p.destroyForcibly();
        p.waitFor();
    }

    /** The waiter interrupts itself before calling waitFor. */
    static void preInterruptedRow(String label, Process p, Wait w) throws Exception {
        String[] out = new String[1];
        boolean[] after = new boolean[1];
        Thread t = new Thread(() -> {
            Thread.currentThread().interrupt();
            out[0] = outcome(w, p);
            after[0] = Thread.interrupted();
        }, label);
        t.start();
        t.join(10_000);
        System.out.println(label + ": " + out[0] + " interrupted-after=" + after[0]);
    }

    public static void main(String[] args) throws Exception {
        interruptRow("waitFor()", Process::waitFor);
        interruptRow("waitFor(60s)", p -> p.waitFor(60, TimeUnit.SECONDS));

        Process running = new ProcessBuilder("sleep", "30").start();
        preInterruptedRow("pre-interrupted waitFor()", running, Process::waitFor);
        preInterruptedRow("pre-interrupted waitFor(60s)", running,
                p -> p.waitFor(60, TimeUnit.SECONDS));
        running.destroyForcibly();
        running.waitFor();

        Process exited = new ProcessBuilder("true").start();
        exited.waitFor();
        preInterruptedRow("exited child, pre-interrupted waitFor()", exited, Process::waitFor);
        preInterruptedRow("exited child, pre-interrupted waitFor(60s)", exited,
                p -> p.waitFor(60, TimeUnit.SECONDS));
        System.out.println("done");
    }
}
