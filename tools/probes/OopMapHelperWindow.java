/**
 * Drive the HELPER-WINDOW root-scan pass, which needs a peer thread blocked in
 * a native call whose stack still carries JIT frames above the blocking point.
 *
 * `OopMapPeerCoverage` does not reach it: its threads only compute and
 * allocate, so `helper_window_pass` reports `0 window(s)` on every collection
 * and any change to that path is unexercised. The difference here is that the
 * blocking call is made FROM a hot, compiled method -- so the frozen peer's
 * stack has compiled frames above a Rust helper, which is exactly the shape
 * `incomplete_reason::XT_HELPER_WINDOW` describes.
 *
 * Read with `CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` and look for
 * `helper-window pass: N window(s)` with N > 0 before trusting any measurement
 * of that path.
 */
import java.util.concurrent.ArrayBlockingQueue;

public class OopMapHelperWindow {
    static final int BLOCKERS = 4;
    static final int CHURNERS = 4;
    static final int ROUNDS = 4000;
    static final ArrayBlockingQueue<Object> Q = new ArrayBlockingQueue<>(4);
    static volatile boolean done = false;

    /** Hot enough to compile, and it BLOCKS -- from compiled code. */
    static long blockingRound(Object[] carry, int n) throws Exception {
        Object[] fresh = new Object[4];
        for (int i = 0; i < 4; i++) {
            fresh[i] = new int[(n % 128) + 16];
        }
        // The blocking call, with `carry` and `fresh` live across it in frame
        // slots: a compiled frame suspended above a native wait.
        Object taken = Q.poll(1, java.util.concurrent.TimeUnit.MILLISECONDS);
        return ((int[]) fresh[1]).length + (taken == null ? 0 : 1) + carry.length;
    }

    static Object[] churn(int n) {
        Object[] out = new Object[8];
        for (int i = 0; i < 8; i++) {
            out[i] = new int[(n % 512) + 64];
        }
        return out;
    }

    public static void main(String[] args) throws Exception {
        Thread[] ts = new Thread[BLOCKERS + CHURNERS];
        final long[] sums = new long[ts.length];
        for (int t = 0; t < BLOCKERS; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                Object[] carry = new Object[3];
                long s = 0;
                try {
                    for (int r = 0; r < ROUNDS; r++) {
                        s += blockingRound(carry, r + id);
                    }
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
                sums[id] = s;
            }, "block-" + t);
        }
        for (int t = 0; t < CHURNERS; t++) {
            final int id = BLOCKERS + t;
            ts[id] = new Thread(() -> {
                long s = 0;
                for (int r = 0; r < ROUNDS * 4; r++) {
                    s += ((int[]) churn(r + id)[1]).length;
                }
                sums[id] = s;
            }, "churn-" + t);
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        done = true;
        long total = 0;
        for (long s : sums) total += s;
        System.out.println("PASS OopMapHelperWindow total=" + total);
    }
}
