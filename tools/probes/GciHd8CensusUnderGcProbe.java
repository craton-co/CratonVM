// gci wave i8, lane hd: `ThreadMXBean` dumps that report `lockedSynchronizers`
// while another thread asks for full collections back to back
// (docs/known-issues/gc/gci-i3hd-heap-walks-and-diagnostics-review-findings-20261012.md, item 8).
//
// CratonVM answers `lockedSynchronizers` with one heap walk per dump in a
// non-moving pause (`owned_synchronizer_census_in`, vm/src/vm/vm_exec.rs). When
// a collection owned the world, the census used to retry its pause for 50 ms
// as a running mutator that neither arrived nor blocked -- holding that
// collection up -- and then answered nothing, so the dump reported no
// synchronizer for a thread that holds two. The fix waits each refusal out
// GC-safe and retries.
//
// A thread `holder` holds a ReentrantLock and a ReentrantReadWriteLock's write
// lock and waits; `gcloop` calls System.gc() until the dumps are done. The main
// thread alternates `dumpAllThreads(false, true)` and
// `getThreadInfo(new long[] {holder}, false, true)` and checks the holder's
// synchronizers (class names, sorted) on every dump.
//
//   javac -d <classes> tools/probes/GciHd8CensusUnderGcProbe.java
//   java     -XX:+UseSerialGC -Xmx64m -cp <classes> GciHd8CensusUnderGcProbe 100
//   cratonvm -Xmx64m -cp <classes> GciHd8CensusUnderGcProbe 100
//
// Expected stdout (HotSpot 25; deterministic):
//
//   holder [java.util.concurrent.locks.ReentrantLock$NonfairSync, java.util.concurrent.locks.ReentrantReadWriteLock$NonfairSync]
//   dumps 100 bad 0 firstBad none
//
// stderr (diagnostics only): `[hd8c] gcs=<System.gc calls> dump_max_ms=<n> slow40=<dumps >= 40 ms>`.
import java.lang.management.LockInfo;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.Arrays;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.ReentrantReadWriteLock;

public class GciHd8CensusUnderGcProbe {
    static final String WANT = "[java.util.concurrent.locks.ReentrantLock$NonfairSync, "
            + "java.util.concurrent.locks.ReentrantReadWriteLock$NonfairSync]";

    public static void main(String[] args) throws Exception {
        int dumps = args.length > 0 ? Integer.parseInt(args[0]) : 100;
        ReentrantLock lock = new ReentrantLock();
        ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
        CountDownLatch held = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        Thread holder = new Thread(() -> {
            lock.lock();
            rw.writeLock().lock();
            try {
                held.countDown();
                release.await();
            } catch (InterruptedException e) {
                // not expected; the locks are released below either way
            } finally {
                rw.writeLock().unlock();
                lock.unlock();
            }
        }, "holder");
        holder.start();
        held.await();
        long hid = holder.threadId();
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        // Before the collections start: the shape every later dump must repeat.
        System.out.println("holder " + synchronizersOf(mx.getThreadInfo(new long[] {hid}, false, true), hid));

        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong gcs = new AtomicLong();
        Thread gc = new Thread(() -> {
            while (!stop.get()) {
                System.gc();
                gcs.incrementAndGet();
            }
        }, "gcloop");
        gc.setDaemon(true);
        gc.start();

        int bad = 0;
        String firstBad = null;
        long maxNs = 0;
        int slow = 0;
        for (int d = 0; d < dumps; d++) {
            long s = System.nanoTime();
            ThreadInfo[] infos = (d % 2 == 0)
                    ? mx.dumpAllThreads(false, true)
                    : mx.getThreadInfo(new long[] {hid}, false, true);
            long ns = System.nanoTime() - s;
            maxNs = Math.max(maxNs, ns);
            if (ns >= 40_000_000L) {
                slow++;
            }
            String got = synchronizersOf(infos, hid);
            if (!WANT.equals(got)) {
                bad++;
                if (firstBad == null) {
                    firstBad = (d % 2 == 0 ? "all:" : "ids:") + got;
                }
            }
        }
        stop.set(true);
        gc.join();
        release.countDown();
        holder.join();
        System.out.println("dumps " + dumps + " bad " + bad + " firstBad " + (firstBad == null ? "none" : firstBad));
        System.err.println("[hd8c] gcs=" + gcs.get() + " dump_max_ms=" + maxNs / 1_000_000 + " slow40=" + slow);
    }

    /** The holder's locked synchronizers as sorted class names, or `missing`. */
    static String synchronizersOf(ThreadInfo[] infos, long hid) {
        for (ThreadInfo ti : infos) {
            if (ti != null && ti.getThreadId() == hid) {
                LockInfo[] locks = ti.getLockedSynchronizers();
                String[] names = new String[locks.length];
                for (int i = 0; i < locks.length; i++) {
                    names[i] = locks[i].getClassName();
                }
                Arrays.sort(names);
                return Arrays.toString(names);
            }
        }
        return "missing";
    }
}
