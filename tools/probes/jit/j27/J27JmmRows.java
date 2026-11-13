// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 5 (lane JMMHUNT): Java-memory-model facts of compiled code on
// x86-64, litmus style. Every row is a count of FORBIDDEN outcomes (or a
// total that synchronization makes exact), so the expected answer is fixed.
// Page: docs/feature-designs/jit-ir-optimizations.md
//
// Rows:
//   dekker-*        store-buffering: `x = 1; r1 = y` || `y = 1; r2 = x` with
//                   volatile fields / volatile statics / VarHandle
//                   setVolatile+getVolatile / AtomicInteger / plain fields with
//                   VarHandle.fullFence() between. r1 == r2 == 0 is forbidden
//                   (the StoreLoad fence after a volatile store).
//   mp-*            message passing: `data = 42; ready = true` ||
//                   `f = ready; d = data`. f && d != 42 is forbidden (volatile;
//                   VarHandle setRelease / getAcquire).
//   final-publish   a racy (plain) publication of an object with final fields,
//                   read through getOpaque: a published object whose final
//                   fields (or the array they reach) read stale is forbidden
//                   (JLS 17.5 freeze).
//   spin-*-stuck    rounds (of j27.jmm.spins) whose spinner thread did not see
//                   the release within 10 s: a volatile flag; a PLAIN field and
//                   a PLAIN static with `Thread.onSpinWait()` in the loop body
//                   (HotSpot's C2 compiles onSpinWait as a MemBar, so the read
//                   stays in the loop: the j27 JMMHUNT fix, switch
//                   CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER); a plain field with
//                   Thread.yield() (a call). A plain flag with an EMPTY body is
//                   deliberately absent: HotSpot hoists that read too.
//   volatile-*-torn a volatile long / double written with two values that
//                   differ in both halves: any other value read is a torn
//                   access (JLS 17.7).
//   counter-*       4 threads x 100 000 increments: AtomicInteger, VarHandle
//                   getAndAdd, synchronized block (the lock-coarsening shape),
//                   synchronized method, ReentrantLock, a compareAndSet loop.
//   ping-pong       5 000 wait/notifyAll hand-offs on one monitor.
//
//   javac -d <dir> J27JmmRows.java
//   java -cp <dir> J27JmmRows > hs.txt
//   cratonvm --cp <dir> J27JmmRows | diff - hs.txt
//     x {--real-jdk, --compatible} x {default, CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER=0}
//   -Dj27.jmm.rounds=N (litmus rounds, default 20000), -Dj27.jmm.spins=N
//   (spin rounds, default 40), -Dj27.jmm.timing=true adds `time` lines.
//   engagement: CRATONVM_DBG_JITC=1 2>&1 | grep 'ir-on-spin-wait'
//
// Expected HotSpot 25 output (facts):
//   fact dekker-volatile-field 0
//   fact dekker-volatile-static 0
//   fact dekker-vh-volatile 0
//   fact dekker-atomic 0
//   fact dekker-fullfence 0
//   fact mp-volatile 0
//   fact mp-acquire-release 0
//   fact final-publish 0
//   fact spin-volatile-stuck 0
//   fact spin-onspinwait-field-stuck 0
//   fact spin-onspinwait-static-stuck 0
//   fact spin-yield-stuck 0
//   fact volatile-long-torn 0
//   fact volatile-double-torn 0
//   fact counter-atomic 400000
//   fact counter-vh-getandadd 400000
//   fact counter-sync-block 400000
//   fact counter-sync-method 400000
//   fact counter-reentrantlock 400000
//   fact counter-cas-loop 400000
//   fact ping-pong 5000
//   fact done
// CratonVM before the fix (--real-jdk, or CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER=0):
// expected `fact spin-onspinwait-field-stuck 1` and/or `...-static-stuck 1`
// once the optimizing tier compiled the spinner (page above).

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantLock;

public class J27JmmRows {
    static final int ROUNDS = Integer.getInteger("j27.jmm.rounds", 20000);
    static final int SPINS = Integer.getInteger("j27.jmm.spins", 40);
    static final boolean TIMING = Boolean.getBoolean("j27.jmm.timing");
    static final int THREADS = 4;
    static final int PER_THREAD = 100_000;
    static final int PING_PONG = 5000;

    static VarHandle vh(Class<?> owner, String name, Class<?> type) {
        try {
            return MethodHandles.lookup().findVarHandle(owner, name, type);
        } catch (ReflectiveOperationException e) {
            throw new AssertionError(e);
        }
    }

    // ── litmus harness ───────────────────────────────────────────────────

    abstract static class Litmus {
        int r1;
        int r2;

        abstract void reset();

        abstract void a();

        abstract void b();

        /** Store buffering: both reads saw the initial value. */
        boolean forbidden() {
            return r1 == 0 && r2 == 0;
        }
    }

    static final class Harness {
        volatile int round = -1;
        volatile int done1 = -1;
        volatile int done2 = -1;
    }

    static void actor(Harness h, Litmus t, boolean first, int rounds) {
        for (int i = 0; i < rounds; i++) {
            while (h.round != i) {
                Thread.onSpinWait();
            }
            if (first) {
                t.a();
                h.done1 = i;
            } else {
                t.b();
                h.done2 = i;
            }
        }
    }

    static int runLitmus(Litmus t, int rounds) throws InterruptedException {
        Harness h = new Harness();
        Thread ta = new Thread(() -> actor(h, t, true, rounds), "litmus-a");
        Thread tb = new Thread(() -> actor(h, t, false, rounds), "litmus-b");
        ta.setDaemon(true);
        tb.setDaemon(true);
        ta.start();
        tb.start();
        int bad = 0;
        for (int i = 0; i < rounds; i++) {
            t.reset();
            h.round = i;
            while (h.done1 != i || h.done2 != i) {
                Thread.onSpinWait();
            }
            if (t.forbidden()) {
                bad++;
            }
        }
        ta.join();
        tb.join();
        return bad;
    }

    // ── store buffering (Dekker) ─────────────────────────────────────────

    static final class DekkerVolatile extends Litmus {
        volatile int x;
        volatile int y;

        void reset() {
            x = 0;
            y = 0;
            r1 = -1;
            r2 = -1;
        }

        void a() {
            x = 1;
            r1 = y;
        }

        void b() {
            y = 1;
            r2 = x;
        }
    }

    static volatile int SX;
    static volatile int SY;

    static final class DekkerStatic extends Litmus {
        void reset() {
            SX = 0;
            SY = 0;
            r1 = -1;
            r2 = -1;
        }

        void a() {
            SX = 1;
            r1 = SY;
        }

        void b() {
            SY = 1;
            r2 = SX;
        }
    }

    static final class DekkerVh extends Litmus {
        static final VarHandle X = vh(DekkerVh.class, "x", int.class);
        static final VarHandle Y = vh(DekkerVh.class, "y", int.class);
        int x;
        int y;

        void reset() {
            X.setVolatile(this, 0);
            Y.setVolatile(this, 0);
            r1 = -1;
            r2 = -1;
        }

        void a() {
            X.setVolatile(this, 1);
            r1 = (int) Y.getVolatile(this);
        }

        void b() {
            Y.setVolatile(this, 1);
            r2 = (int) X.getVolatile(this);
        }
    }

    static final class DekkerAtomic extends Litmus {
        final AtomicInteger x = new AtomicInteger();
        final AtomicInteger y = new AtomicInteger();

        void reset() {
            x.set(0);
            y.set(0);
            r1 = -1;
            r2 = -1;
        }

        void a() {
            x.set(1);
            r1 = y.get();
        }

        void b() {
            y.set(1);
            r2 = x.get();
        }
    }

    static final class DekkerFence extends Litmus {
        int x;
        int y;

        void reset() {
            x = 0;
            y = 0;
            r1 = -1;
            r2 = -1;
        }

        void a() {
            x = 1;
            VarHandle.fullFence();
            r1 = y;
        }

        void b() {
            y = 1;
            VarHandle.fullFence();
            r2 = x;
        }
    }

    // ── message passing ──────────────────────────────────────────────────

    static final class MpVolatile extends Litmus {
        int data;
        volatile boolean ready;

        void reset() {
            data = 0;
            ready = false;
            r1 = -1;
            r2 = -1;
        }

        void a() {
            data = 42;
            ready = true;
        }

        void b() {
            boolean f = ready;
            int d = data;
            r1 = f ? 1 : 0;
            r2 = d;
        }

        @Override
        boolean forbidden() {
            return r1 == 1 && r2 != 42;
        }
    }

    static final class MpAcqRel extends Litmus {
        static final VarHandle READY = vh(MpAcqRel.class, "ready", boolean.class);
        int data;
        boolean ready;

        void reset() {
            data = 0;
            READY.setVolatile(this, false);
            r1 = -1;
            r2 = -1;
        }

        void a() {
            data = 42;
            READY.setRelease(this, true);
        }

        void b() {
            boolean f = (boolean) READY.getAcquire(this);
            int d = data;
            r1 = f ? 1 : 0;
            r2 = d;
        }

        @Override
        boolean forbidden() {
            return r1 == 1 && r2 != 42;
        }
    }

    // ── final-field publication ──────────────────────────────────────────

    static final class FinalBox {
        final int a;
        final int[] arr;

        FinalBox(int v) {
            a = v;
            arr = new int[] {v, v + 1};
        }
    }

    static final class FinalPub extends Litmus {
        static final VarHandle BOX = vh(FinalPub.class, "box", FinalBox.class);
        FinalBox box;

        void reset() {
            box = null;
            r1 = -1;
            r2 = -1;
        }

        void a() {
            box = new FinalBox(7);
        }

        void b() {
            FinalBox seen = null;
            for (int k = 0; k < 1_000_000 && seen == null; k++) {
                seen = (FinalBox) BOX.getOpaque(this);
            }
            if (seen == null) {
                r1 = 2; // not published in time: no answer, not counted
            } else {
                r1 = (seen.a == 7 && seen.arr[0] == 7 && seen.arr[1] == 8) ? 1 : 0;
            }
        }

        @Override
        boolean forbidden() {
            return r1 == 0;
        }
    }

    // ── spin-wait termination ────────────────────────────────────────────

    static final class Flag {
        boolean plain;
        volatile boolean vol;
    }

    static boolean STATIC_PLAIN;

    static int spinVolatile(Flag f) {
        int n = 0;
        while (!f.vol) {
            n++;
        }
        return n;
    }

    static int spinOnSpinWaitField(Flag f) {
        int n = 0;
        while (!f.plain) {
            Thread.onSpinWait();
            n++;
        }
        return n;
    }

    static int spinOnSpinWaitStatic() {
        int n = 0;
        while (!STATIC_PLAIN) {
            Thread.onSpinWait();
            n++;
        }
        return n;
    }

    static int spinYield(Flag f) {
        int n = 0;
        while (!f.plain) {
            Thread.yield();
            n++;
        }
        return n;
    }

    static void spinKind(int kind, Flag f) {
        switch (kind) {
            case 0 -> spinVolatile(f);
            case 1 -> spinOnSpinWaitField(f);
            case 2 -> spinOnSpinWaitStatic();
            default -> spinYield(f);
        }
    }

    static void release(int kind, Flag f) {
        switch (kind) {
            case 0 -> f.vol = true;
            case 2 -> STATIC_PLAIN = true;
            default -> f.plain = true;
        }
    }

    /** Rounds whose spinner was still spinning 10 s after its release (stops at the first). */
    static int spinRounds(int kind, int rounds) throws InterruptedException {
        for (int r = 0; r < rounds; r++) {
            Flag f = new Flag();
            STATIC_PLAIN = false;
            Thread t = new Thread(() -> spinKind(kind, f), "spin-" + kind);
            t.setDaemon(true);
            t.start();
            // Mostly short spins (interpreted, then compiled once hot); every
            // eighth round spins long enough for an OSR or method compile.
            Thread.sleep((r % 8 == 7) ? 40 : 2);
            release(kind, f);
            t.join(10_000);
            if (t.isAlive()) {
                return 1;
            }
        }
        return 0;
    }

    // ── volatile long / double atomicity ─────────────────────────────────

    static final long LA = 0x0123_4567_89AB_CDEFL;
    static final long LB = 0xFEDC_BA98_7654_3210L;
    static volatile long VL = LA;
    static volatile double VD = Double.longBitsToDouble(LA);

    static long[] tornCounts(int n) throws InterruptedException {
        Thread w = new Thread(() -> {
            for (int i = 0; i < n; i++) {
                boolean even = (i & 1) == 0;
                VL = even ? LB : LA;
                VD = Double.longBitsToDouble(even ? LB : LA);
            }
        }, "torn-writer");
        long[] torn = new long[2];
        Thread r = new Thread(() -> {
            long tl = 0;
            long td = 0;
            for (int i = 0; i < n; i++) {
                long l = VL;
                if (l != LA && l != LB) {
                    tl++;
                }
                long d = Double.doubleToRawLongBits(VD);
                if (d != LA && d != LB) {
                    td++;
                }
            }
            torn[0] = tl;
            torn[1] = td;
        }, "torn-reader");
        w.start();
        r.start();
        w.join();
        r.join();
        return torn;
    }

    // ── counters ─────────────────────────────────────────────────────────

    static final AtomicInteger ATOMIC = new AtomicInteger();
    static final AtomicLong CAS = new AtomicLong();
    static final Object LOCK = new Object();
    static final ReentrantLock RLOCK = new ReentrantLock();
    static int syncBlock;
    static int syncMethod;
    static int rlocked;
    static int vhCount;
    static final VarHandle VH_COUNT = staticVh();

    static VarHandle staticVh() {
        try {
            return MethodHandles.lookup().findStaticVarHandle(J27JmmRows.class, "vhCount", int.class);
        } catch (ReflectiveOperationException e) {
            throw new AssertionError(e);
        }
    }

    static synchronized void bumpSyncMethod() {
        syncMethod++;
    }

    static void countWork() {
        for (int i = 0; i < PER_THREAD; i++) {
            ATOMIC.incrementAndGet();
            VH_COUNT.getAndAdd(1);
            synchronized (LOCK) {
                syncBlock++;
            }
            bumpSyncMethod();
            RLOCK.lock();
            try {
                rlocked++;
            } finally {
                RLOCK.unlock();
            }
            long v;
            do {
                v = CAS.get();
            } while (!CAS.compareAndSet(v, v + 1));
        }
    }

    static void counters() throws InterruptedException {
        Thread[] ts = new Thread[THREADS];
        for (int k = 0; k < THREADS; k++) {
            ts[k] = new Thread(J27JmmRows::countWork, "count-" + k);
            ts[k].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        int vh;
        synchronized (LOCK) {
            vh = (int) VH_COUNT.getVolatile();
        }
        fact("counter-atomic", ATOMIC.get());
        fact("counter-vh-getandadd", vh);
        synchronized (LOCK) {
            fact("counter-sync-block", syncBlock);
        }
        fact("counter-sync-method", readSyncMethod());
        RLOCK.lock();
        try {
            fact("counter-reentrantlock", rlocked);
        } finally {
            RLOCK.unlock();
        }
        fact("counter-cas-loop", CAS.get());
    }

    static synchronized int readSyncMethod() {
        return syncMethod;
    }

    // ── wait / notify ────────────────────────────────────────────────────

    static final Object PP = new Object();
    static int ppTurn;
    static int ppCount;

    static void pingPong(int me, int n) {
        for (int i = 0; i < n; i++) {
            synchronized (PP) {
                while (ppTurn != me) {
                    try {
                        PP.wait();
                    } catch (InterruptedException e) {
                        throw new AssertionError(e);
                    }
                }
                if (me == 1) {
                    ppCount++;
                }
                ppTurn = 1 - me;
                PP.notifyAll();
            }
        }
    }

    // ── driver ───────────────────────────────────────────────────────────

    static void fact(String name, long value) {
        System.out.println("fact " + name + " " + value);
    }

    static void litmus(String name, Litmus t) throws InterruptedException {
        long t0 = System.nanoTime();
        int bad = runLitmus(t, ROUNDS);
        fact(name, bad);
        if (TIMING) {
            System.out.println("time " + name + " " + (System.nanoTime() - t0) / 1_000_000 + " ms");
        }
    }

    public static void main(String[] args) throws Exception {
        Thread watchdog = new Thread(() -> {
            try {
                Thread.sleep(600_000);
            } catch (InterruptedException e) {
                return;
            }
            System.out.println("fact watchdog fired");
            System.out.flush();
            Runtime.getRuntime().halt(3);
        }, "watchdog");
        watchdog.setDaemon(true);
        watchdog.start();

        litmus("dekker-volatile-field", new DekkerVolatile());
        litmus("dekker-volatile-static", new DekkerStatic());
        litmus("dekker-vh-volatile", new DekkerVh());
        litmus("dekker-atomic", new DekkerAtomic());
        litmus("dekker-fullfence", new DekkerFence());
        litmus("mp-volatile", new MpVolatile());
        litmus("mp-acquire-release", new MpAcqRel());
        litmus("final-publish", new FinalPub());

        fact("spin-volatile-stuck", spinRounds(0, SPINS));
        fact("spin-onspinwait-field-stuck", spinRounds(1, SPINS));
        fact("spin-onspinwait-static-stuck", spinRounds(2, SPINS));
        fact("spin-yield-stuck", spinRounds(3, SPINS));

        long[] torn = tornCounts(2_000_000);
        fact("volatile-long-torn", torn[0]);
        fact("volatile-double-torn", torn[1]);

        counters();

        Thread p0 = new Thread(() -> pingPong(0, PING_PONG), "ping");
        Thread p1 = new Thread(() -> pingPong(1, PING_PONG), "pong");
        p0.start();
        p1.start();
        p0.join();
        p1.join();
        synchronized (PP) {
            fact("ping-pong", ppCount);
        }
        System.out.println("fact done");
    }
}
