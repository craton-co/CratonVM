// Interpreter round i5 wave 103, lane L1: the wake gap of a process waiter,
// measured from the moment the exit is OBSERVABLE, not from the trigger.
// Bench for docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md.
//
// `L1W102ProcessExitWakeBench` times trigger -> waiter return, which holds
// the child's own teardown (`destroyForcibly()` -> TerminateProcess / SIGKILL
// -> the process object signalled: about 3 ms on the w102 hosts), so the
// 60-130 us wake gap the page is about is below its noise (the w102 switch
// triage of `CRATONVM_PROCESS_WAIT_DIRECT_WAKE`: "about 3 ms either way").
// Here a spinner thread calls `p.isAlive()` in a tight loop and stamps the
// first `false`; the waiter stamps its return; the row is waiter - spinner
// (it can be negative: the waiter may see the exit first). That is the
// time the waiter's own wake path adds, the quantity HotSpot answers in a
// few microseconds.
//
// No arguments, no setup. Arms to interleave (in-JVM timings swing ~3x on
// these hosts: take medians over several interleaved runs): HotSpot;
// CratonVM default; `CRATONVM_PROCESS_WAIT_DIRECT_WAKE=1`;
// `CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT=0`; `CRATONVM_PROCESS_PIDFD_WAIT=0`
// (Linux); each in `--jdk-only` and `--compatible`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; the TIME lines are filtered)
// waitFor() runs=11 ok=true
// waitFor(60s) runs=11 ok=true
// onExit().get() runs=11 ok=true
// done
// HOTSPOT_EXPECTED_END
import java.util.Arrays;
import java.util.concurrent.TimeUnit;

public class L1W103ProcessWakeGapBench {
    interface Wait {
        void run(Process p) throws Exception;
    }

    static final boolean WINDOWS = System.getProperty("os.name").startsWith("Windows");
    static final int RUNS = 11;

    static Process longChild() throws Exception {
        ProcessBuilder pb = WINDOWS
                ? new ProcessBuilder("ping", "-n", "30", "127.0.0.1")
                : new ProcessBuilder("sleep", "30");
        return pb.redirectOutput(ProcessBuilder.Redirect.DISCARD).start();
    }

    /** One run: the waiter's return minus the spinner's first `!isAlive()`, in ns. */
    static long gap(Wait waitKind) throws Exception {
        Process p = longChild();
        long[] stamps = new long[2];
        boolean[] ok = new boolean[1];
        Thread waiter = new Thread(() -> {
            try {
                waitKind.run(p);
                stamps[0] = System.nanoTime();
                ok[0] = true;
            } catch (Exception e) {
                stamps[0] = System.nanoTime();
            }
        }, "waiter");
        Thread spinner = new Thread(() -> {
            while (p.isAlive()) {
                Thread.onSpinWait();
            }
            stamps[1] = System.nanoTime();
        }, "spinner");
        waiter.start();
        // Let the waiter block in its wait first (a native wait reads
        // RUNNABLE, so there is no state to poll for).
        Thread.sleep(30);
        spinner.start();
        Thread.sleep(5);
        p.destroyForcibly();
        waiter.join(60_000);
        spinner.join(60_000);
        if (!ok[0]) {
            return Long.MIN_VALUE;
        }
        return stamps[0] - stamps[1];
    }

    static void row(String name, Wait waitKind) throws Exception {
        long[] gaps = new long[RUNS];
        boolean ok = true;
        for (int i = 0; i < RUNS; i++) {
            gaps[i] = gap(waitKind);
            ok &= gaps[i] != Long.MIN_VALUE;
        }
        System.out.println(name + " runs=" + RUNS + " ok=" + ok);
        Arrays.sort(gaps);
        StringBuilder all = new StringBuilder();
        for (long g : gaps) {
            all.append(all.length() == 0 ? "" : ",").append(g / 1000);
        }
        System.out.println("TIME " + name + " gap-us median=" + gaps[RUNS / 2] / 1000
                + " all=[" + all + "]");
    }

    public static void main(String[] args) throws Exception {
        // Warm both paths once, unreported.
        gap(Process::waitFor);
        row("waitFor()", Process::waitFor);
        row("waitFor(60s)", p -> p.waitFor(60, TimeUnit.SECONDS));
        row("onExit().get()", p -> p.onExit().get());
        System.out.println("done");
    }
}
