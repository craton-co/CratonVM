// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L1 -- BENCH probe for
// docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md
// ("Wave 102").
//
// How long after a child's exit does a waiter return? Two triggers whose
// instant the parent knows:
//   destroy    t0 = just before p.destroyForcibly() (TerminateProcess /
//              SIGKILL; includes the kill itself, the same on every VM);
//   stdin-eof  t0 = just before the child's stdin is closed (the child --
//              `findstr x` on Windows, `cat` elsewhere -- exits at EOF on its
//              own, a natural exit).
// Three waiters, each run at the bottom of a 4- and a 150-frame recursion (a
// gap that grows with depth is a root-snapshot rebuild on the region leave):
//   waitFor()       Windows: waitForInterruptibly; UNIX: the reaper's signalAll
//   waitFor(60s)    Windows: waitForTimeoutInterruptibly; UNIX: awaitNanos
//   onExit().get()  the ProcessHandleImpl reaper thread, then the future
// TIME lines carry ns from t0 to the waiter's return (median / p90 / min over
// RUNS runs after WARMUP); HotSpot 25 is the reference. Filter them out for
// the byte comparison (lines starting with "TIME ").
//
// Run (no setup; compare both modes, and the switches on and off):
//   cratonvm L1W102ProcessExitWakeBench
//   cratonvm --compatible L1W102ProcessExitWakeBench
//   CRATONVM_PROCESS_WAIT_DIRECT_WAKE=1 cratonvm ... L1W102ProcessExitWakeBench
//   CRATONVM_PROCESS_PIDFD_WAIT=1 cratonvm ... L1W102ProcessExitWakeBench   (Linux)
//   CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT=0 ...   (the wave-99 quiet leave off)
// Attribution: CRATONVM_DBG_PB=1 prints [PB-WAKE] / [PB-QUIET] / [PB-LEAVE]
// per wake (see the page). Interleave binaries and take medians of several
// runs: in-VM timings swing ~3x between repetitions on the dev host.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; TIME lines filtered)
// destroy waitFor() depth=4 runs=11 ok=true
// destroy waitFor() depth=150 runs=11 ok=true
// destroy waitFor(60s) depth=4 runs=11 ok=true
// destroy waitFor(60s) depth=150 runs=11 ok=true
// destroy onExit().get() depth=4 runs=11 ok=true
// destroy onExit().get() depth=150 runs=11 ok=true
// stdin-eof waitFor() depth=4 runs=11 ok=true
// stdin-eof waitFor() depth=150 runs=11 ok=true
// stdin-eof waitFor(60s) depth=4 runs=11 ok=true
// stdin-eof waitFor(60s) depth=150 runs=11 ok=true
// stdin-eof onExit().get() depth=4 runs=11 ok=true
// stdin-eof onExit().get() depth=150 runs=11 ok=true
// done
// HOTSPOT_EXPECTED_END
import java.util.Arrays;
import java.util.concurrent.TimeUnit;

public class L1W102ProcessExitWakeBench {
    static final boolean WINDOWS = System.getProperty("os.name").startsWith("Windows");
    static final int WARMUP = 2;
    static final int RUNS = 11;

    interface Wait {
        boolean run(Process p) throws Exception;
    }

    static Process child(boolean eof) throws Exception {
        ProcessBuilder pb;
        if (eof) {
            pb = WINDOWS ? new ProcessBuilder("findstr", "x") : new ProcessBuilder("cat");
        } else {
            pb = WINDOWS
                    ? new ProcessBuilder("ping", "-n", "30", "127.0.0.1")
                    : new ProcessBuilder("sleep", "30");
        }
        return pb.redirectOutput(ProcessBuilder.Redirect.DISCARD)
                .redirectError(ProcessBuilder.Redirect.DISCARD)
                .start();
    }

    /** Run {@code w} at the bottom of {@code depth} frames. */
    static boolean deep(int depth, Wait w, Process p) throws Exception {
        if (depth <= 1) {
            return w.run(p);
        }
        boolean r = deep(depth - 1, w, p);
        return r;
    }

    /** One wake: ns from the trigger to the waiter's return, or -1 on failure. */
    static long once(boolean eof, int depth, Wait w) throws Exception {
        Process p = child(eof);
        long[] returnedAt = new long[1];
        boolean[] ok = new boolean[1];
        Thread t = new Thread(() -> {
            try {
                ok[0] = deep(depth, w, p);
            } catch (Exception e) {
                ok[0] = false;
            }
            returnedAt[0] = System.nanoTime();
        }, "waiter");
        t.setDaemon(true);
        t.start();
        Thread.sleep(60);
        long t0 = System.nanoTime();
        if (eof) {
            p.getOutputStream().close();
        } else {
            p.destroyForcibly();
        }
        t.join(20_000);
        if (t.isAlive() || !ok[0] || p.isAlive()) {
            p.destroyForcibly();
            return -1;
        }
        return Math.max(0, returnedAt[0] - t0);
    }

    static void row(String trigger, boolean eof, String label, Wait w, int depth) throws Exception {
        for (int i = 0; i < WARMUP; i++) {
            once(eof, depth, w);
        }
        long[] ns = new long[RUNS];
        boolean ok = true;
        for (int i = 0; i < RUNS; i++) {
            ns[i] = once(eof, depth, w);
            ok &= ns[i] >= 0;
        }
        Arrays.sort(ns);
        System.out.println(trigger + " " + label + " depth=" + depth + " runs=" + RUNS + " ok=" + ok);
        System.out.println("TIME " + trigger + " " + label + " depth=" + depth
                + " median_ns=" + ns[RUNS / 2]
                + " p90_ns=" + ns[(RUNS * 9) / 10]
                + " min_ns=" + ns[0]);
    }

    public static void main(String[] args) throws Exception {
        String[] labels = {"waitFor()", "waitFor(60s)", "onExit().get()"};
        Wait[] waits = {
            p -> {
                p.waitFor();
                return true;
            },
            p -> p.waitFor(60, TimeUnit.SECONDS),
            p -> p.onExit().get(60, TimeUnit.SECONDS) == p,
        };
        for (int trig = 0; trig < 2; trig++) {
            boolean eof = trig == 1;
            String trigger = eof ? "stdin-eof" : "destroy";
            for (int k = 0; k < waits.length; k++) {
                row(trigger, eof, labels[k], waits[k], 4);
                row(trigger, eof, labels[k], waits[k], 150);
            }
        }
        System.out.println("done");
    }
}
