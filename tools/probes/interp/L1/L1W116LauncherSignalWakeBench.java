// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L1 -- how late a caught host signal
// reaches its Java handler, for the A/B of
// `CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE` (default off): the launcher's signal
// dispatcher waits on a self-pipe its handler writes instead of looking at
// the counters every 20 ms. Page:
// docs/known-issues/interpreter/i116-L1-proposal-launcher-signal-dispatcher-direct-wake-20261218.md.
//
// POSIX only (Windows has no way for a process to send itself Ctrl-C without
// reaching the whole console; there the compared line is printed untested).
// A `Signal.handle("INT")` handler counts; the probe runs `kill -INT <self>`
// ROUNDS times and waits for each count. TIME lines: the median and max
// microseconds from the `kill` child's exit to the handler's run (HotSpot:
// tens to hundreds of us; switch off: up to 20 ms more, median ~10 ms).
//
// Run (Linux / macOS, the cratonvm launcher, both modes):
//   java L1W116LauncherSignalWakeBench
//   cratonvm L1W116LauncherSignalWakeBench
//   CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE=1 cratonvm L1W116LauncherSignalWakeBench
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; the TIME lines are filtered)
// handled-all=true
// done
// HOTSPOT_EXPECTED_END

import java.util.Arrays;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import sun.misc.Signal;

public class L1W116LauncherSignalWakeBench {
    static final int ROUNDS = 15;

    public static void main(String[] args) throws Exception {
        if (System.getProperty("os.name").startsWith("Windows")) {
            System.out.println("handled-all=true");
            System.out.println("TIME skipped on Windows");
            System.out.println("done");
            return;
        }
        AtomicInteger handled = new AtomicInteger();
        AtomicLong handledAt = new AtomicLong();
        Signal.handle(new Signal("INT"), s -> {
            handledAt.set(System.nanoTime());
            handled.incrementAndGet();
        });
        String pid = Long.toString(ProcessHandle.current().pid());
        long[] lat = new long[ROUNDS];
        boolean all = true;
        for (int i = 0; i < ROUNDS; i++) {
            Process kill = new ProcessBuilder("kill", "-INT", pid).start();
            kill.waitFor();
            long exited = System.nanoTime();
            long until = exited + 5_000_000_000L;
            while (handled.get() < i + 1 && System.nanoTime() < until) {
                Thread.onSpinWait();
            }
            if (handled.get() < i + 1) {
                all = false;
                break;
            }
            // Can be negative: the handler may run before `waitFor` returns.
            lat[i] = Math.max(0, handledAt.get() - exited);
            Thread.sleep(5);
        }
        System.out.println("handled-all=" + all);
        long[] c = lat.clone();
        Arrays.sort(c);
        System.out.println("TIME kill-exit->handler median-us=" + c[ROUNDS / 2] / 1_000
                + " max-us=" + c[ROUNDS - 1] / 1_000);
        System.out.println("done");
    }
}
