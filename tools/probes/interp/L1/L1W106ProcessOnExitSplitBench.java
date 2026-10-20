// Interpreter round i5 wave 106, lane L1: where the `onExit().get()` wake gap
// of docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md
// goes. Bench; the TIME lines are filtered.
//
// L1W103ProcessWakeGapBench measured (Windows w105b, medians of 7 runs, us):
// `onExit().get()` 627 (`--jdk-only`) / 586 (`--compatible`) against
// HotSpot's 151-192, while `waitFor()` is 106 / 102 against 27-81. The
// `--compatible` route has one hand-off fewer than `--jdk-only`'s (its exit
// waiter completes the future itself; the JDK's `handleAsync` adds a
// common-pool hop), yet costs about the same, so the extra ~450 us is on the
// edge both share: the completing thread -> the `get()` waiter. This bench
// splits that path:
//
// * `onExit split`: a spinner stamps the first `!p.isAlive()`; a dependent
//   (`whenComplete`) registered on the waiter's future AFTER the waiter parked
//   in `get()` -- so `postComplete` runs it before it unparks the waiter --
//   stamps the moment the future completes; the waiter stamps its return.
//   `exit->complete` (the reaper's wake, its blocking-region leave, its Java,
//   and under `--jdk-only` the common-pool hop) and `complete->return` (the
//   `get()` waiter's unpark, wake and way back to Java).
// * `future handoff`: no process. A thread stamps and completes a
//   `CompletableFuture` a waiter is parked in `get()` on: the same
//   `complete->return` edge alone.
// * `park handoff`: a thread stamps and `LockSupport.unpark`s a waiter parked
//   in `LockSupport.park()`: the untimed park's wake alone.
//
// If `complete->return` and the two hand-off rows are hundreds of us against
// HotSpot's tens, the gap is the untimed park's wake (lane L6's park, not the
// process code); if `exit->complete` is, it is the reaper side (lane L1).
// Run with `CRATONVM_DBG_PB=1` for the reaper's `[PB-WAKE] site=..` /
// `[PB-LEAVE]` stamps (wave 106), and `CRATONVM_DBG_PARKLAT=50` for every
// unpark->wake over 50 us and the park epilogue.
//
// No arguments, no setup. Arms: HotSpot; CratonVM `--jdk-only`,
// `--compatible`, each default and `--nojit`; interleave, take medians.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; the TIME lines are filtered)
// onExit split runs=11 ok=true
// future handoff runs=11 ok=true
// park handoff runs=11 ok=true
// done
// HOTSPOT_EXPECTED_END
import java.util.Arrays;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.locks.LockSupport;

public class L1W106ProcessOnExitSplitBench {
    static final boolean WINDOWS = System.getProperty("os.name").startsWith("Windows");
    static final int RUNS = 11;

    static Process longChild() throws Exception {
        ProcessBuilder pb = WINDOWS
                ? new ProcessBuilder("ping", "-n", "30", "127.0.0.1")
                : new ProcessBuilder("sleep", "30");
        return pb.redirectOutput(ProcessBuilder.Redirect.DISCARD).start();
    }

    /** {exit->complete, complete->return} in ns, or null on a failed run. */
    static long[] onExitSplit() throws Exception {
        Process p = longChild();
        CompletableFuture<Process> f = p.onExit();
        long[] stamps = new long[3];
        boolean[] ok = new boolean[1];
        Thread waiter = new Thread(() -> {
            try {
                f.get();
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
        Thread.sleep(30);
        // After the waiter's Signaller: `postComplete` pops this one first.
        f.whenComplete((x, e) -> stamps[2] = System.nanoTime());
        spinner.start();
        Thread.sleep(5);
        p.destroyForcibly();
        waiter.join(60_000);
        spinner.join(60_000);
        if (!ok[0] || stamps[2] == 0) {
            return null;
        }
        return new long[] {stamps[2] - stamps[1], stamps[0] - stamps[2]};
    }

    /** complete -> get() return, in ns, or MIN_VALUE. */
    static long futureHandoff() throws Exception {
        CompletableFuture<String> f = new CompletableFuture<>();
        long[] back = new long[1];
        boolean[] ok = new boolean[1];
        Thread waiter = new Thread(() -> {
            try {
                f.get();
                back[0] = System.nanoTime();
                ok[0] = true;
            } catch (Exception e) {
                back[0] = System.nanoTime();
            }
        }, "waiter");
        waiter.start();
        Thread.sleep(30);
        long sent = System.nanoTime();
        f.complete("x");
        waiter.join(60_000);
        return ok[0] ? back[0] - sent : Long.MIN_VALUE;
    }

    static volatile boolean released;

    /** unpark -> park() return, in ns, or MIN_VALUE. */
    static long parkHandoff() throws Exception {
        released = false;
        long[] back = new long[1];
        Thread waiter = new Thread(() -> {
            while (!released) {
                LockSupport.park();
            }
            back[0] = System.nanoTime();
        }, "waiter");
        waiter.start();
        Thread.sleep(30);
        long sent = System.nanoTime();
        released = true;
        LockSupport.unpark(waiter);
        waiter.join(60_000);
        return back[0] == 0 ? Long.MIN_VALUE : back[0] - sent;
    }

    static String row(long[] ns) {
        long[] sorted = ns.clone();
        Arrays.sort(sorted);
        StringBuilder all = new StringBuilder();
        for (long g : sorted) {
            all.append(all.length() == 0 ? "" : ",").append(g / 1000);
        }
        return "median=" + sorted[sorted.length / 2] / 1000 + " all=[" + all + "]";
    }

    public static void main(String[] args) throws Exception {
        // Warm each path once, unreported.
        onExitSplit();
        futureHandoff();
        parkHandoff();

        long[] toComplete = new long[RUNS];
        long[] toReturn = new long[RUNS];
        boolean ok = true;
        for (int i = 0; i < RUNS; i++) {
            long[] split = onExitSplit();
            ok &= split != null;
            toComplete[i] = split == null ? 0 : split[0];
            toReturn[i] = split == null ? 0 : split[1];
        }
        System.out.println("onExit split runs=" + RUNS + " ok=" + ok);
        System.out.println("TIME onExit exit->complete-us " + row(toComplete));
        System.out.println("TIME onExit complete->return-us " + row(toReturn));

        long[] future = new long[RUNS];
        ok = true;
        for (int i = 0; i < RUNS; i++) {
            future[i] = futureHandoff();
            ok &= future[i] != Long.MIN_VALUE;
        }
        System.out.println("future handoff runs=" + RUNS + " ok=" + ok);
        System.out.println("TIME future complete->return-us " + row(future));

        long[] park = new long[RUNS];
        ok = true;
        for (int i = 0; i < RUNS; i++) {
            park[i] = parkHandoff();
            ok &= park[i] != Long.MIN_VALUE;
        }
        System.out.println("park handoff runs=" + RUNS + " ok=" + ok);
        System.out.println("TIME park unpark->return-us " + row(park));
        System.out.println("done");
    }
}
