// Interpreter round i1 wave 14, lane L3 -- a cross-thread getStackTrace() of a
// thread busy in a long poll-free stretch of compiled code
// (interpreter-L4-frame-trace-pause-freezes-a-running-compiled-thread-before-it-publishes-FIXED).
//
// A worker thread spins `System.arraycopy` over a 2 MB array (a bulk intrinsic
// whose next safepoint poll is far away) until told to stop; it never blocks.
// After warm-up (so the loop is OSR'd / compiled) the main thread samples
// `worker.getStackTrace()` SAMPLES times and counts the samples whose frames
// name `busy`. The trace is published by the target itself at a safepoint
// poll; before wave 14 the frame-trace pause's take-over froze a target still
// inside the intrinsic, which then published nothing, so the sample came back
// empty (the worker never blocked). Wave 14 waits a cooperative 2 ms slice
// before any freeze.
//
// HotSpot 25 prints exactly:
//   samples 200
//   named-busy 200
//   empty 0
//   result ok
//
// Compare `--compatible` with and without `--nojit`: identical stdout. stderr
// carries the elapsed time of the sampling phase. With `--verbose:gc`, the
// `door=frame-trace` pause lines should show `xt_frozen=0`.
public class L7FrameTraceOfBusyThread {
    static final int SAMPLES = 200;
    static volatile boolean stop;
    static volatile boolean warm;
    static volatile long sink;

    static long busy(int[] src, int[] dst) {
        long n = 0;
        while (!stop) {
            System.arraycopy(src, 0, dst, 0, src.length);
            n++;
            if (n == 2000) {
                warm = true;
            }
        }
        return n + dst[dst.length - 1];
    }

    public static void main(String[] args) throws Exception {
        final int[] src = new int[512 * 1024];
        final int[] dst = new int[src.length];
        for (int i = 0; i < src.length; i++) {
            src[i] = i;
        }
        Thread worker = new Thread(() -> sink = busy(src, dst), "busy-worker");
        worker.setDaemon(true);
        worker.start();
        long deadline = System.nanoTime() + 60_000_000_000L;
        while (!warm && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        int named = 0;
        int empty = 0;
        long t0 = System.nanoTime();
        for (int s = 0; s < SAMPLES; s++) {
            StackTraceElement[] frames = worker.getStackTrace();
            if (frames.length == 0) {
                empty++;
            }
            for (StackTraceElement f : frames) {
                if (f.getMethodName().equals("busy")
                        && f.getClassName().equals("L7FrameTraceOfBusyThread")) {
                    named++;
                    break;
                }
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        stop = true;
        worker.join(60_000L);
        System.err.println("sampling took " + ms + " ms; warm=" + warm);
        System.out.println("samples " + SAMPLES);
        System.out.println("named-busy " + named);
        System.out.println("empty " + empty);
        System.out.println("result " + (sink >= 0 ? "ok" : "bad"));
    }
}
