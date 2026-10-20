import java.lang.invoke.VarHandle;
import java.lang.management.LockInfo;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.nio.ByteBuffer;
import java.nio.channels.Pipe;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.ReentrantReadWriteLock;

/**
 * nio21 wave 3, lane L4: the `ACC_NATIVE` triples compiled code used to dispatch generically
 * (page `r21-jdk-only-volatile-unsafe-refersto0-and-carrier-thread-are-generic-native-calls-FIXED-20261012.md`)
 * and the dump-time `lockedSynchronizers` census that replaced the per-lock owner index (page
 * `r21-aqs-owner-index-is-a-native-call-per-lock-FIXED-20261008.md`).
 *
 * Conformance (default, or `-Dnio21.rows=conf`) prints answers only, to diff against HotSpot 25 in
 * both modes and with each switch `=0`. It drives, from hot compiled loops:
 *   Unsafe.get/putIntVolatile    AtomicIntegerFieldUpdater get / set / getAndAdd (getAndAddInt)
 *   Unsafe.get/putLongVolatile   AtomicLongFieldUpdater get / set / lazySet / getAndAdd, AtomicLong
 *   Unsafe.putReferenceVolatile  AtomicReferenceFieldUpdater set, ConcurrentHashMap setTabAt
 *   Unsafe.compareAndSetReference AtomicReferenceFieldUpdater compareAndSet / getAndSet, CHM casTabAt
 *   Unsafe.fullFence             VarHandle.fullFence
 *   Reference.refersTo0          WeakReference / PhantomReference.refersTo, ThreadLocal.get
 *   Thread.currentCarrierThread  Pipe I/O through sun.nio.ch.Util's carrier-thread-local buffers
 * plus the same atomics raced by four threads, and `aqs` rows: ThreadMXBean lockedSynchronizers
 * (class names sorted; HotSpot's order is heap order) for the current thread, another thread, a
 * pool worker, reentrancy, release, and the doors that must report none.
 *
 * Bench rows (`-Dnio21.rows=bench` for all, or a list) print `<row> <N> ns/op`; `-Dnio21.ops=N`
 * sets the op count (default 5_000_000; `pipe` and `dump` run ops/1000 and ops/50000):
 *   ainc refset lfu ifu rcas chm fence refersto tlget pipe lock dump
 */
public class Nio21UnsafeVolatile {
    static String rows = System.getProperty("nio21.rows", "conf");
    static long ops = Long.getLong("nio21.ops", 5_000_000L);

    static boolean on(String r) {
        List<String> l = Arrays.asList(rows.split(","));
        return l.contains(r) || (l.contains("bench") && !r.equals("conf") && !r.equals("aqs"));
    }

    static void p(String s) {
        System.out.println(s);
    }

    static final class Box {
        volatile int i;
        volatile long l;
        volatile Object r;
    }

    static final AtomicIntegerFieldUpdater<Box> I = AtomicIntegerFieldUpdater.newUpdater(Box.class, "i");
    static final AtomicLongFieldUpdater<Box> L = AtomicLongFieldUpdater.newUpdater(Box.class, "l");
    static final AtomicReferenceFieldUpdater<Box, Object> R =
            AtomicReferenceFieldUpdater.newUpdater(Box.class, Object.class, "r");

    static final Object[] OBJS = new Object[8];

    static {
        for (int k = 0; k < OBJS.length; k++) OBJS[k] = Integer.valueOf(1000 + k);
    }

    static int idx(Object o) {
        if (o == null) return -1;
        for (int k = 0; k < OBJS.length; k++) if (OBJS[k] == o) return k;
        return -2;
    }

    // ---- hot bodies, out of line so each tiers up on its own ----

    static long intRound(Box b, int k) {
        I.set(b, k);
        int g = I.get(b);
        int prev = I.getAndAdd(b, 3);
        int inc = I.incrementAndGet(b);
        return (long) g + prev + inc + I.get(b);
    }

    static long longRound(Box b, int k) {
        long v = k * 0x1_0000_0001L;
        L.set(b, v);
        long g = L.get(b);
        long prev = L.getAndAdd(b, -7L);
        L.lazySet(b, L.get(b) ^ 0x5555L);
        boolean cas = L.compareAndSet(b, L.get(b), g);
        return g + prev + L.get(b) + (cas ? 1 : 0);
    }

    static long refRound(Box b, int k) {
        Object o = OBJS[k & 7];
        R.set(b, o);
        boolean c1 = R.compareAndSet(b, o, OBJS[(k + 1) & 7]);
        boolean c2 = R.compareAndSet(b, o, null);
        Object prev = R.getAndSet(b, OBJS[(k + 2) & 7]);
        R.set(b, null);
        boolean c3 = R.compareAndSet(b, null, OBJS[(k + 3) & 7]);
        return (c1 ? 1 : 0) + (c2 ? 2 : 0) + (c3 ? 4 : 0) + 8L * idx(prev) + 64L * idx(R.get(b));
    }

    static long atomicRound(AtomicLong al, AtomicInteger ai, int k) {
        long a = al.incrementAndGet();
        long b = al.getAndAdd(k);
        int c = ai.getAndIncrement();
        int d = ai.addAndGet(-1);
        return a + b + c + d;
    }

    static int fenceRound(int k) {
        VarHandle.fullFence();
        VarHandle.acquireFence();
        VarHandle.releaseFence();
        VarHandle.storeStoreFence();
        VarHandle.loadLoadFence();
        return k & 1;
    }

    static int refersRound(WeakReference<Object> w, PhantomReference<Object> ph, Object a, Object c) {
        return (w.refersTo(a) ? 1 : 0) + (w.refersTo(c) ? 2 : 0) + (ph.refersTo(a) ? 4 : 0)
                + (w.refersTo(null) ? 8 : 0) + (ph.refersTo(c) ? 16 : 0);
    }

    static final ThreadLocal<Integer>[] TLS = newLocals(8);

    @SuppressWarnings("unchecked")
    static ThreadLocal<Integer>[] newLocals(int n) {
        ThreadLocal<Integer>[] t = new ThreadLocal[n];
        for (int k = 0; k < n; k++) {
            final int v = k;
            t[k] = ThreadLocal.withInitial(() -> v * 10);
        }
        return t;
    }

    static long tlRound(int k) {
        long s = 0;
        for (ThreadLocal<Integer> t : TLS) s += t.get();
        TLS[k & 7].set((k & 7) * 10);
        return s;
    }

    static long chmRound(ConcurrentHashMap<Integer, Integer> m, int k) {
        Integer key = k & 1023;
        m.put(key, k);
        Integer got = m.get(key);
        long r = got == null ? -1 : got;
        if ((k & 1) == 0) m.remove(key);
        return r;
    }

    static long pipeRound(Pipe pipe, ByteBuffer out, ByteBuffer in, int k) throws Exception {
        out.clear();
        for (int j = 0; j < 64; j++) out.put((byte) (k + j));
        out.flip();
        while (out.hasRemaining()) pipe.sink().write(out);
        in.clear();
        int got = 0;
        while (got < 64) got += pipe.source().read(in);
        in.flip();
        long s = 0;
        while (in.hasRemaining()) s += in.get();
        return s;
    }

    // ---- conformance ----

    static void conf() throws Exception {
        int n = 200_000;
        Box b = new Box();
        long isum = 0, lsum = 0, rsum = 0, asum = 0, fsum = 0;
        for (int k = 0; k < n; k++) isum += intRound(b, k);
        p("int " + b.i + " " + isum);
        for (int k = 0; k < n; k++) lsum += longRound(b, k);
        p("long " + b.l + " " + lsum);
        for (int k = 0; k < n; k++) rsum += refRound(b, k);
        p("ref " + idx(b.r) + " " + rsum);
        AtomicLong al = new AtomicLong();
        AtomicInteger ai = new AtomicInteger();
        for (int k = 0; k < n; k++) asum += atomicRound(al, ai, k);
        p("atomic " + al.get() + " " + ai.get() + " " + asum);
        for (int k = 0; k < n; k++) fsum += fenceRound(k);
        p("fence " + fsum);

        ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        long csum = 0;
        for (int k = 0; k < n; k++) csum += chmRound(m, k);
        long msum = 0;
        for (Integer v : m.values()) msum += v;
        p("chm " + m.size() + " " + csum + " " + msum);

        Object a = new Object(), c = new Object();
        WeakReference<Object> w = new WeakReference<>(a);
        PhantomReference<Object> ph = new PhantomReference<>(a, new ReferenceQueue<>());
        long hits = 0;
        for (int k = 0; k < n; k++) hits += refersRound(w, ph, a, c);
        p("refersTo " + hits + " " + w.refersTo(a) + " " + w.refersTo(c) + " " + w.refersTo(null)
                + " " + ph.refersTo(a) + " " + ph.refersTo(null));
        System.gc();
        p("refersTo-after-gc " + w.refersTo(a) + " " + ph.refersTo(a));
        w.clear();
        ph.clear();
        p("refersTo-cleared " + w.refersTo(a) + " " + w.refersTo(null) + " " + ph.refersTo(a)
                + " " + ph.refersTo(null));
        Reference.reachabilityFence(a);

        long tsum = 0;
        for (int k = 0; k < n; k++) tsum += tlRound(k);
        p("threadlocal " + tsum);

        Pipe pipe = Pipe.open();
        ByteBuffer out = ByteBuffer.allocate(64), in = ByteBuffer.allocate(64);
        long psum = 0;
        for (int k = 0; k < 5_000; k++) psum += pipeRound(pipe, out, in, k);
        pipe.sink().close();
        pipe.source().close();
        p("pipe " + psum);

        // Four threads race the same cells: lost updates would show here.
        Box shared = new Box();
        AtomicLong sharedAl = new AtomicLong();
        ReentrantLock lock = new ReentrantLock();
        long[] guarded = new long[1];
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            ts[t] = new Thread(() -> {
                for (int k = 0; k < 100_000; k++) {
                    L.getAndIncrement(shared);
                    I.getAndAdd(shared, 2);
                    sharedAl.incrementAndGet();
                    Object prev;
                    do {
                        prev = R.get(shared);
                    } while (!R.compareAndSet(shared, prev, OBJS[k & 7]));
                    if ((k & 15) == 0) {
                        lock.lock();
                        try {
                            guarded[0]++;
                        } finally {
                            lock.unlock();
                        }
                    }
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        p("race " + shared.l + " " + shared.i + " " + sharedAl.get() + " " + (idx(shared.r) >= 0) + " "
                + guarded[0]);
    }

    static String names(ThreadInfo ti) {
        if (ti == null) return "null";
        List<String> l = new ArrayList<>();
        for (LockInfo li : ti.getLockedSynchronizers()) l.add(li.getClassName());
        Collections.sort(l);
        return l.toString();
    }

    static ThreadInfo find(ThreadInfo[] all, long id) {
        for (ThreadInfo ti : all) if (ti != null && ti.getThreadId() == id) return ti;
        return null;
    }

    static void aqs() throws Exception {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        p("aqs supported " + mx.isSynchronizerUsageSupported());
        long me = Thread.currentThread().threadId();
        ReentrantLock lock = new ReentrantLock();
        ReentrantLock fair = new ReentrantLock(true);
        ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
        p("aqs none " + names(mx.getThreadInfo(new long[] {me}, false, true)[0]));
        lock.lock();
        fair.lock();
        rw.writeLock().lock();
        rw.readLock().lock();
        p("aqs held " + names(mx.getThreadInfo(new long[] {me}, false, true)[0]));
        p("aqs held-no-flag " + mx.getThreadInfo(new long[] {me}, false, false)[0].getLockedSynchronizers().length);
        p("aqs dump-all " + names(find(mx.dumpAllThreads(false, true), me)));
        p("aqs dump-all-no-flag " + find(mx.dumpAllThreads(false, false), me).getLockedSynchronizers().length);
        p("aqs single " + mx.getThreadInfo(me).getLockedSynchronizers().length);
        lock.lock();
        p("aqs reentrant " + names(mx.getThreadInfo(new long[] {me}, false, true)[0]) + " " + lock.getHoldCount());
        lock.unlock();
        lock.unlock();
        p("aqs after-one " + names(mx.getThreadInfo(new long[] {me}, false, true)[0]));
        rw.readLock().unlock();
        rw.writeLock().unlock();
        fair.unlock();
        p("aqs after-all " + names(mx.getThreadInfo(new long[] {me}, false, true)[0]));

        ReentrantLock other = new ReentrantLock();
        CountDownLatch held = new CountDownLatch(1), release = new CountDownLatch(1);
        Thread t = new Thread(() -> {
            other.lock();
            try {
                held.countDown();
                release.await();
            } catch (InterruptedException e) {
                // fall through to unlock
            } finally {
                other.unlock();
            }
        }, "nio21-holder");
        t.start();
        held.await();
        p("aqs other " + names(mx.getThreadInfo(new long[] {t.threadId()}, false, true)[0]) + " mine "
                + names(mx.getThreadInfo(new long[] {me}, false, true)[0]));
        release.countDown();
        t.join();

        ExecutorService pool = Executors.newFixedThreadPool(1);
        CountDownLatch inTask = new CountDownLatch(1), outTask = new CountDownLatch(1);
        long[] wid = new long[1];
        pool.execute(() -> {
            wid[0] = Thread.currentThread().threadId();
            inTask.countDown();
            try {
                outTask.await();
            } catch (InterruptedException e) {
                // done
            }
        });
        inTask.await();
        p("aqs worker " + names(mx.getThreadInfo(new long[] {wid[0]}, false, true)[0]));
        outTask.countDown();
        pool.shutdown();
        p("aqs pool-done " + pool.awaitTermination(10, TimeUnit.SECONDS));
    }

    // ---- bench ----

    interface Body {
        long run(int k) throws Exception;
    }

    static void bench(String row, long n, Body body) throws Exception {
        long sink = 0;
        long warm = Math.min(n, 200_000);
        for (int k = 0; k < warm; k++) sink += body.run(k);
        long t0 = System.nanoTime();
        for (int k = 0; k < n; k++) sink += body.run(k);
        long dt = System.nanoTime() - t0;
        double per = n == 0 ? 0 : (double) dt / n;
        p(row + " " + (Math.round(per * 10) / 10.0) + " ns/op");
        if (sink == 42) System.err.print("");
    }

    public static void main(String[] args) throws Exception {
        if (on("conf")) conf();
        if (on("conf") || on("aqs")) aqs();
        Box b = new Box();
        AtomicLong al = new AtomicLong();
        if (on("ainc")) bench("ainc", ops, k -> al.incrementAndGet());
        if (on("refset")) bench("refset", ops, k -> {
            R.set(b, OBJS[k & 7]);
            return 0;
        });
        if (on("lfu")) bench("lfu", ops, k -> {
            L.set(b, k);
            return L.getAndIncrement(b);
        });
        if (on("ifu")) bench("ifu", ops, k -> I.getAndAdd(b, 1));
        if (on("rcas")) bench("rcas", ops, k -> R.compareAndSet(b, R.get(b), OBJS[k & 7]) ? 1 : 0);
        ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        if (on("chm")) bench("chm", ops, k -> chmRound(m, k));
        if (on("fence")) bench("fence", ops, k -> {
            VarHandle.fullFence();
            return k;
        });
        Object a = new Object();
        WeakReference<Object> w = new WeakReference<>(a);
        if (on("refersto")) bench("refersto", ops, k -> w.refersTo(a) ? 1 : 0);
        if (on("tlget")) bench("tlget", ops, Nio21UnsafeVolatile::tlRound);
        if (on("pipe")) {
            Pipe pipe = Pipe.open();
            ByteBuffer out = ByteBuffer.allocate(64), in = ByteBuffer.allocate(64);
            bench("pipe", Math.max(1, ops / 1000), k -> pipeRound(pipe, out, in, k));
        }
        ReentrantLock lock = new ReentrantLock();
        if (on("lock")) bench("lock", ops, k -> {
            lock.lock();
            try {
                return k;
            } finally {
                lock.unlock();
            }
        });
        if (on("dump")) {
            ThreadMXBean mx = ManagementFactory.getThreadMXBean();
            long[] ids = {Thread.currentThread().threadId()};
            bench("dump", Math.max(1, ops / 50_000), k -> mx.getThreadInfo(ids, false, true)[0].getThreadId());
        }
        Reference.reachabilityFence(a);
    }
}
