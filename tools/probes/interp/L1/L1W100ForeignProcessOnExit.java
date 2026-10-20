// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L1 --
// docs/internal/fixed-bugs/interpreter-i100-L1-windows-foreign-process-onexit-polls-FIXED-20261015.md
//
// `ProcessHandle.onExit()` of a process this VM did NOT spawn (a grandchild:
// a child launcher starts it, prints its pid and exits, so the grandchild is
// nobody's child of ours). The future is taken, the process is killed
// through `destroyForcibly()`, and the probe reports whether the future
// completed promptly (< 200 ms after the kill).
//
// HotSpot's answer is platform-dependent, and the probe is compared with
// HotSpot on the same platform:
//  * Windows: `waitForProcessExit0` waits on the process handle of ANY pid,
//    so `onExit-prompt=true`. CratonVM answered `NOT_A_CHILD`, sending the
//    JDK to its `isAlive0` poll (first check after 300 ms): false.
//  * Linux / macOS: `waitpid` gives ECHILD, `NOT_A_CHILD`, the same poll on
//    both VMs: `onExit-prompt=false`.
//
// Setup: none (the launcher of this VM is re-run from
// `ProcessHandle.current().info().command()` with this class path).
//   javac -d out L1W100ForeignProcessOnExit.java
//   cratonvm -cp out L1W100ForeignProcessOnExit
//   cratonvm --compatible -cp out L1W100ForeignProcessOnExit
// PROBE_TIMINGS=1 prints the latency on stderr ("TIME onExit-ms <n>").
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, Windows)
// foreign alive=true
// destroyed=true done=true alive=false
// onExit-prompt=true
// HOTSPOT_EXPECTED_END
// (Linux / macOS HotSpot: the same with onExit-prompt=false.)

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;

public class L1W100ForeignProcessOnExit {
    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "main";
        String cmd = ProcessHandle.current().info().command().orElseThrow();
        String cp = System.getProperty("java.class.path");
        if (mode.equals("sleeper")) {
            Thread.sleep(120_000);
            return;
        }
        if (mode.equals("parent")) {
            Process grandchild = new ProcessBuilder(cmd, "-cp", cp,
                    "L1W100ForeignProcessOnExit", "sleeper")
                    .redirectOutput(ProcessBuilder.Redirect.DISCARD)
                    .redirectError(ProcessBuilder.Redirect.DISCARD)
                    .start();
            System.out.println(grandchild.pid());
            System.out.flush();
            // Exit without waiting: the grandchild outlives its parent.
            System.exit(0);
        }
        Process parent = new ProcessBuilder(cmd, "-cp", cp,
                "L1W100ForeignProcessOnExit", "parent")
                .redirectError(ProcessBuilder.Redirect.DISCARD)
                .start();
        long pid;
        try (BufferedReader r = new BufferedReader(
                new InputStreamReader(parent.getInputStream()))) {
            pid = Long.parseLong(r.readLine().trim());
        }
        parent.waitFor();
        ProcessHandle h = ProcessHandle.of(pid).orElseThrow();
        System.out.println("foreign alive=" + h.isAlive());
        CompletableFuture<ProcessHandle> f = h.onExit();
        Thread.sleep(50);
        long t0 = System.nanoTime();
        boolean destroyed = h.destroyForcibly();
        f.get(30, TimeUnit.SECONDS);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("destroyed=" + destroyed + " done=" + f.isDone()
                + " alive=" + h.isAlive());
        System.out.println("onExit-prompt=" + (ms < 200));
        if (System.getenv("PROBE_TIMINGS") != null) {
            System.err.println("TIME onExit-ms " + ms);
        }
    }
}
