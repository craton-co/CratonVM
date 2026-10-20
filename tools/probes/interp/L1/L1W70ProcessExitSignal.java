// Interpreter round i2 wave 70, lane L1: `CRATONVM_PROCESS_EXIT_SIGNAL`
// (default OFF; docs/internal/fixed-bugs/interpreter-i69-L1-proposal-signal-the-process-condition-at-exit-CLOSED-20261224.md).
//
// A VM-spawned child's `waitFor(long, TimeUnit)` stand-in (`--compatible`)
// slept in 10 ms slices, so a timed waiter returned up to 10 ms after the
// child exited; HotSpot's UNIX `ProcessImpl` waiter wakes at the reaper's
// `signalAll`. With the switch on, an exit reaper publishes the exit and the
// waiter wakes on it.
//
// Interpreter round i4 wave 97, lane L1
// (docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md):
// the output is byte-comparable with HotSpot. The raw latencies
// (`wake-us median=... max=... all=[...]`) go to STDERR only when the
// environment variable PROBE_TIMINGS is set (the battery merges stderr into
// the compared output, so an unconditional timing line could never match);
// stdout carries the bounded verdict `wakes-promptly=true` = the median wake
// is under 500 us after the untimed waiter returned (HotSpot Windows: 4 us;
// the old 10 ms poll: 880-4432 us; the wave-96 reaper hop: 60-130 us).
// `exitValue-is-kill-code`: 1 on Windows (TerminateProcess(h, 1)), a signal
// code elsewhere.
//   cratonvm --compatible L1W70ProcessExitSignal
//   PROBE_TIMINGS=1 CRATONVM_PROCESS_EXIT_SIGNAL=0 cratonvm --compatible L1W70ProcessExitSignal
// Needs `sh` and `sleep` (UNIX, or Git for Windows' on PATH).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// waitFor(60s) after destroy: true exitValue-is-kill-code=true alive=false wakes-promptly=true
// waitFor(60s) on a natural exit: true exitValue=3
// waitFor(50ms) on a live child: false alive=true
// done
// HOTSPOT_EXPECTED_END
import java.util.Arrays;
import java.util.concurrent.TimeUnit;

public class L1W70ProcessExitSignal {
    static final boolean WINDOWS = System.getProperty("os.name").startsWith("Windows");

    public static void main(String[] args) throws Exception {
        long[] wake = new long[7];
        boolean allTrue = true;
        boolean allSignal = true;
        boolean anyAlive = false;
        for (int i = 0; i < wake.length; i++) {
            Process p = new ProcessBuilder("sleep", "30").start();
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
            // The exit as the OS reports it: the untimed waitFor blocks on the
            // child itself in every arm.
            p.waitFor();
            long exitedAt = System.nanoTime();
            w.join(10_000);
            wake[i] = Math.max(0, returnedAt[0] - exitedAt) / 1000;
            allTrue &= result[0];
            allSignal &= WINDOWS
                    ? p.exitValue() == 1
                    : p.exitValue() == 137 || p.exitValue() == 143 || p.exitValue() == 128 + 9;
            anyAlive |= p.isAlive();
        }
        Arrays.sort(wake);
        if (System.getenv("PROBE_TIMINGS") != null) {
            System.err.println("wake-us median=" + wake[wake.length / 2] + " max=" + wake[wake.length - 1]
                    + " all=" + Arrays.toString(wake));
        }
        System.out.println("waitFor(60s) after destroy: " + allTrue + " exitValue-is-kill-code=" + allSignal
                + " alive=" + anyAlive + " wakes-promptly=" + (wake[wake.length / 2] < 500));

        Process q = new ProcessBuilder("sh", "-c", "sleep 0.2; exit 3").start();
        boolean done = q.waitFor(60, TimeUnit.SECONDS);
        System.out.println("waitFor(60s) on a natural exit: " + done + " exitValue=" + q.exitValue());

        Process r = new ProcessBuilder("sleep", "30").start();
        boolean early = r.waitFor(50, TimeUnit.MILLISECONDS);
        System.out.println("waitFor(50ms) on a live child: " + early + " alive=" + r.isAlive());
        r.destroyForcibly();
        r.waitFor();
        System.out.println("done");
    }
}
