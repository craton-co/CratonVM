import java.util.concurrent.locks.ReentrantLock;

/**
 * Exactly how many native calls does one uncontended lock/unlock make?
 *
 * Run with `--dump-native-registry <out.json>`: the schema-2 census records an
 * `invocations` count per native. This probe does N uncontended
 * `lock()`/`unlock()` pairs and as close to nothing else as a Java main can, so
 * every native whose count is a clean multiple of N is on the lock path, and
 * the multiple IS the per-lock-pair call count.
 *
 * Beats reading the JDK source and guessing which accessors CratonVM has
 * shadowed: `AbstractQueuedSynchronizer.getState()` is `return state;` in JDK
 * 25 but could still be a registered native here, and the census answers that
 * without an argument.
 *
 * Pass the iteration count as argv[0] (default 1,000,000).
 */
public final class LockNativeCensusProbe {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 1_000_000;
        ReentrantLock lock = new ReentrantLock();
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            lock.lock();
            lock.unlock();
        }
        long d = System.nanoTime() - t0;
        System.out.printf("%d lock/unlock pairs in %.3f s (%.1f ns/pair)%n",
                n, d / 1e9, d / (double) n);
        System.out.println("now read the census: any native with invocations ~= k*" + n
                + " is on the lock path, k calls per pair");
    }
}
