import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Is the retaining frame the one that HELD the reference?
 *
 * `RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced` nulls
 * its `thread` local and then runs the `System.gc()` loop IN THE SAME METHOD.
 * `RecyclerRetainProbe` ran the loop in a callee and collected every time. That
 * is a difference nobody tested, and it is the difference between "a compiled
 * frame pins a dead local" and something subtler.
 *
 * Three arms, identical work, differing only in which frame is on the stack
 * while the collector runs:
 *
 *   inline  — create/start/join/null AND the gc-loop, all in one method
 *             (netty's shape)
 *   callee  — same method creates/starts/joins/nulls, a CALLEE runs the loop
 *   scoped  — a CALLEE creates/starts/joins the thread and returns void, so the
 *             looping frame never held the reference at all
 *
 * Run with CRATONVM_BG_COMPILE=0 so the methods are compiled on first call;
 * that is the lever that takes the netty test from 0/6 to 6/6.
 */
public final class FrameRetainProbe {

    static final int TIMEOUT_MS = 4000;

    static final class Watched extends Thread {
        final AtomicBoolean flag;
        Watched(AtomicBoolean f) { flag = f; }
        @Override public void run() { }
        @Override protected void finalize() { flag.set(true); }
    }

    /** The gc-loop, as a callee. Returns ms, or -1 on timeout. */
    static long loop(AtomicBoolean flag) throws InterruptedException {
        long t0 = System.nanoTime();
        long deadline = t0 + TIMEOUT_MS * 1_000_000L;
        while (!flag.get()) {
            if (System.nanoTime() > deadline) {
                return -1;
            }
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
        }
        return (System.nanoTime() - t0) / 1_000_000L;
    }

    /** Creates, starts, joins and drops the thread; the caller never sees it. */
    static void makeAndFinish(AtomicBoolean flag) throws InterruptedException {
        Watched t = new Watched(flag);
        t.start();
        t.join();
    }

    static long inlineArm() throws InterruptedException {
        AtomicBoolean flag = new AtomicBoolean();
        Watched thread = new Watched(flag);
        thread.start();
        thread.join();
        thread = null;
        // netty's shape: the loop is HERE, in the frame that held `thread`.
        long t0 = System.nanoTime();
        long deadline = t0 + TIMEOUT_MS * 1_000_000L;
        while (!flag.get()) {
            if (System.nanoTime() > deadline) {
                return -1;
            }
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
        }
        return (System.nanoTime() - t0) / 1_000_000L;
    }

    static long calleeArm() throws InterruptedException {
        AtomicBoolean flag = new AtomicBoolean();
        Watched thread = new Watched(flag);
        thread.start();
        thread.join();
        thread = null;
        return loop(flag);
    }

    static long scopedArm() throws InterruptedException {
        AtomicBoolean flag = new AtomicBoolean();
        makeAndFinish(flag);
        long t0 = System.nanoTime();
        long deadline = t0 + TIMEOUT_MS * 1_000_000L;
        while (!flag.get()) {
            if (System.nanoTime() > deadline) {
                return -1;
            }
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
        }
        return (System.nanoTime() - t0) / 1_000_000L;
    }

    static void report(String arm, long ms) {
        System.out.printf("  %-58s %s%n", arm,
                          ms < 0 ? "RETAINED (" + TIMEOUT_MS + "ms)" : "collected in " + ms + "ms");
    }

    public static void main(String[] args) throws Exception {
        // Two rounds: the first compiles nothing yet under background
        // compilation, the second runs against warmed methods. The netty test
        // shows exactly that shape (its #1 passes, everything after fails).
        for (int round = 1; round <= 2; round++) {
            System.out.println("round " + round + ":");
            report("inline  (loop in the frame that held the reference)", inlineArm());
            report("callee  (loop in a callee)", calleeArm());
            report("scoped  (looping frame never held the reference)", scopedArm());
        }
        System.exit(0);
    }
}
