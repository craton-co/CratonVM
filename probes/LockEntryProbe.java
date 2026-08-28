import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.locks.ReentrantLock;

/**
 * How many Rust&lt;-&gt;JIT boundary crossings does ONE uncontended
 * `ReentrantLock.lock()`/`unlock()` pair make?
 *
 * `AqsAttributionProbe` prices the pair at ~2400 ns and attributes only ~618 ns
 * of it: `Thread.currentThread()` x2, the state CAS, and
 * `setExclusiveOwnerThread` x2. The REPLICA rung — the SAME algorithm on
 * classes the VM has no opinion about — costs ~490 ns. So ~1800 ns of the real
 * pair has no owner, and it is not the algorithm.
 *
 * A ns/op number cannot say where it goes. A COUNT can:
 * `CRATONVM_DBG_JIT_SCAN_PROF=1` prints `jit_entries`, the exit-time tally of
 * `note_jit_boundary()` — every crossing between compiled code and the Rust
 * runtime, which is every native dispatch and every thin direct helper. That is
 * the instrument that cracked `BrotliIntegrationTest`'s 300x per-byte
 * `ByteBuf.writeByte` wall, where it reported ~2 entries per byte written.
 *
 * ONE ARM PER PROCESS, selected by argv[0], because `jit_entries` is a
 * process-wide total and two arms in one run cannot be told apart. Run the
 * `empty` arm to get the fixed cost of boot, and subtract it.
 *
 *   for arm in empty replica lock; do
 *     CRATONVM_DBG_JIT_SCAN_PROF=1 cratonvm --java-home $JDK -cp out \
 *       LockEntryProbe $arm 2000000
 *   done
 *
 * entries_per_pair = (entries[lock] - entries[empty]) / n
 */
public final class LockEntryProbe {

    static final ReentrantLock LOCK = new ReentrantLock();
    static long sink;

    /** The JDK, the number being explained. */
    private static long rLock(ReentrantLock l, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) {
            l.lock();
            a += i;
            l.unlock();
        }
        sink += a;
        return System.nanoTime() - t0;
    }

    /**
     * The same ALGORITHM with none of the JDK's classes: a volatile int state,
     * one CAS through a field updater, one plain owner field. Same rungs, same
     * order, so a difference is a difference in what the VM does to
     * `java.util.concurrent`, not in how much work is asked for.
     */
    private static final class ReplicaLock {
        private static final AtomicIntegerFieldUpdater<ReplicaLock> STATE =
                AtomicIntegerFieldUpdater.newUpdater(ReplicaLock.class, "state");

        volatile int state;
        Thread owner;

        boolean lock() {
            if (STATE.compareAndSet(this, 0, 1)) {
                owner = Thread.currentThread();
                return true;
            }
            return false;
        }

        void unlock() {
            int c = state;
            if (owner == Thread.currentThread()) {
                owner = null;
                STATE.lazySet(this, c - 1);
            }
        }
    }

    private static long rReplica(ReplicaLock l, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) {
            if (l.lock()) {
                a++;
            }
            l.unlock();
        }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rEmpty(int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) {
            a += i;
        }
        sink += a;
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) {
        String arm = args.length > 0 ? args[0] : "lock";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 2_000_000;
        ReplicaLock replica = new ReplicaLock();
        // Warm, then the measured pass. Both are counted by `jit_entries`, so
        // the denominator below is 4n, not n.
        long warm;
        long dt;
        switch (arm) {
            case "empty":
                warm = rEmpty(n);
                dt = rEmpty(n);
                break;
            case "replica":
                warm = rReplica(replica, n);
                dt = rReplica(replica, n);
                break;
            default:
                warm = rLock(LOCK, n);
                dt = rLock(LOCK, n);
                break;
        }
        System.out.printf("%-8s n=%d warm=%.1f ns/op measured=%.1f ns/op (pairs executed = %d)%n",
                arm, n, warm / (double) n, dt / (double) n, 2L * n);
        System.out.println("sink=" + (sink == 0 ? 1 : 0));
    }
}
