// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 96, lane L1 --
// docs/internal/fixed-bugs/interpreter-i96-L1-windows-processimpl-wait-ignores-interrupt-FIXED-20261128.md
//
// A thread waiting for a running child wakes on Thread.interrupt and on the
// child's exit. Windows ProcessImpl.waitFor() is waitForInterruptibly(handle)
// (WaitForMultipleObjects on the process and the thread's interrupt event)
// then `if (Thread.interrupted()) throw new InterruptedException();`; the timed
// overload loops on waitForTimeoutInterruptibly the same way. UNIX awaits a
// ReentrantLock condition the reaper signals. CratonVM's Windows natives waited
// in Child::wait (interrupt ignored) and polled the timed wait in 10 ms sleeps
// (woke 1-4 ms after the exit; L1W70ProcessExitSignal).
//
// Portable: the child is `ping -n 30 127.0.0.1` on Windows, `sleep 30` elsewhere.
// No arguments, no setup:
//   cratonvm L1W96ProcessWaitWakes
//   cratonvm --compatible L1W96ProcessWaitWakes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, derived from the Windows and UNIX ProcessImpl sources)
// waitFor() interrupted: java.lang.InterruptedException fast=true interrupted-after=false
// waitFor(60s) interrupted: java.lang.InterruptedException fast=true interrupted-after=false
// pre-interrupted waitFor() on a running child: java.lang.InterruptedException interrupted-after=false
// waitFor(60s) after destroy: true wakes-promptly=true
// done
// HOTSPOT_EXPECTED_END
// The "wakes-promptly" row (median wake under 500 us after the untimed waiter
// returns) is a timing bound, not a byte from the sources; HotSpot measured
// a 0-4 us median on Windows for the same shape (L1W70ProcessExitSignal);
// the 10 ms poll measured 880-4432 us, the wave-96 reaper hop 60-130 us.
// Wave 97 (docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md):
// the raw `wake-us median=... all=[...]` line goes to STDERR only when the
// environment variable PROBE_TIMINGS is set -- the battery merges stderr
// into the compared output, so an unconditional timing line never matches.
import java.util.Arrays;
import java.util.concurrent.TimeUnit;

public class L1W96ProcessWaitWakes {
    interface Wait {
        Object run(Process p) throws InterruptedException;
    }

    static final boolean WINDOWS = System.getProperty("os.name").startsWith("Windows");

    static Process longChild() throws Exception {
        ProcessBuilder pb = WINDOWS
                ? new ProcessBuilder("ping", "-n", "30", "127.0.0.1")
                : new ProcessBuilder("sleep", "30");
        return pb.redirectOutput(ProcessBuilder.Redirect.DISCARD).start();
    }

    static String outcome(Wait w, Process p) {
        try {
            return String.valueOf(w.run(p));
        } catch (InterruptedException e) {
            return e.toString();
        }
    }

    static void interruptRow(String label, Wait w) throws Exception {
        Process p = longChild();
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

    public static void main(String[] args) throws Exception {
        interruptRow("waitFor() interrupted", Process::waitFor);
        interruptRow("waitFor(60s) interrupted", p -> p.waitFor(60, TimeUnit.SECONDS));

        Process running = longChild();
        String[] out = new String[1];
        boolean[] after = new boolean[1];
        Thread pre = new Thread(() -> {
            Thread.currentThread().interrupt();
            out[0] = outcome(Process::waitFor, running);
            after[0] = Thread.interrupted();
        });
        pre.setDaemon(true);
        pre.start();
        pre.join(10_000);
        System.out.println("pre-interrupted waitFor() on a running child: " + out[0]
                + " interrupted-after=" + after[0]);
        running.destroyForcibly();
        running.waitFor();

        long[] wake = new long[7];
        boolean allTrue = true;
        for (int i = 0; i < wake.length; i++) {
            Process p = longChild();
            long[] returnedAt = new long[1];
            boolean[] result = new boolean[1];
            Thread w = new Thread(() -> {
                try {
                    result[0] = p.waitFor(60, TimeUnit.SECONDS);
                } catch (InterruptedException e) {
                    result[0] = false;
                }
                returnedAt[0] = System.nanoTime();
            });
            w.start();
            Thread.sleep(100);
            p.destroyForcibly();
            p.waitFor();
            long exitedAt = System.nanoTime();
            w.join(10_000);
            wake[i] = Math.max(0, returnedAt[0] - exitedAt) / 1000;
            allTrue &= result[0];
        }
        Arrays.sort(wake);
        if (System.getenv("PROBE_TIMINGS") != null) {
            System.err.println("wake-us median=" + wake[wake.length / 2] + " all=" + Arrays.toString(wake));
        }
        System.out.println("waitFor(60s) after destroy: " + allTrue
                + " wakes-promptly=" + (wake[wake.length / 2] < 500));
        System.out.println("done");
    }
}
