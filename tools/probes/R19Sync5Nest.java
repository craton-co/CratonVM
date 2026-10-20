// JIT round 19 wave 5, lane sync5: nested synchronized blocks in a synchronized method
// (SYNC5-1, CRATONVM_JIT_SELF_LOCK_BUFFER_RETRY: a nested-block self-locking body overflows its code-buffer estimate on the first compile and the refusal is memoized for good), the recursive self-call of a synchronized
// instance method (SYNCDOOR4-2, CRATONVM_JIT_SELF_LOCK_SELF_CALL) and the static synchronized
// callee eager bind (SYNC5-2, CRATONVM_JIT_SELF_LOCK_EAGER_BIND_STATIC, with
// CRATONVM_JIT_SELF_LOCK_EAGER_BIND=1).
//
// Rows (time, ns per call; distinct non-numeric labels). Every caller warms and measures in the
// SAME method (`drive*`), each shape has its own caller and its own callee class.
//   nest-same      synchronized m() { sync(a) { sync(a) { .. } } }       (same monitor twice)
//   nest-diff      synchronized m() { sync(a) { sync(b) { .. } } }
//   nest-deep      synchronized m() { sync(a) { sync(b) { sync(this) { .. } } } }  (3 deep)
//   nest-throw     the inner block throws every 64th call; the caller catches
//   nest-catch     the inner block throws every 64th call; a catch INSIDE the outer block
//   nest-plain     the unsynchronized twin of nest-diff (method not synchronized)
//   rec-table      synchronized recursive with a try/catch table, same receiver (the R17 `rc`)
//   rec-other      the same, recursing on ANOTHER receiver (must lock that receiver)
//   rec-plain      unsynchronized twin
//   static-loop    static synchronized callee called in a loop (static eager bind)
//   static-plain   unsynchronized static twin
//   arr-loop       synchronized method with an array loop (watch row, R18 `arr`)
//
// Facts (must equal HotSpot in every arm, also with CRATONVM_JIT_THRESHOLD=1 and
// CRATONVM_DEOPT_REAL=0): holdsLock inside each level, monitor free after every shape (another
// thread takes it), exception identity across the nested exits, IMSE inside nested blocks,
// wait/notify inside nested blocks, a deopt (class-hierarchy change) while nested monitors are held.
//
// Arms (orchestrator):
//   A  default
//   N  CRATONVM_JIT_SELF_LOCK_BUFFER_RETRY=1
//   R  CRATONVM_JIT_SELF_LOCK_SELF_CALL=1
//   S  CRATONVM_JIT_SELF_LOCK_EAGER_BIND=1 CRATONVM_JIT_SELF_LOCK_EAGER_BIND_STATIC=1
//   NRS all three
// Engagement: CRATONVM_DBG_JITC=1 | grep -E "self-lock-buffer-retry|self-lock-blocks REFUSED|self-lock-handlers ADMITTED|instance-self-call|self-lock-eager-bind" | grep R19Sync5Nest
public class R19Sync5Nest {
    static final Object SENTINEL_LOCK = new Object();

    // -------- nested blocks --------
    static final class N {
        int o, b, heldInner, heldOuter, heldMethod, caughtInside, imse, waited;
        final Object a = new Object();
        final Object bb = new Object();
        static Shape shape = new Shape();

        int helper(int x) {
            b += x;
            return b & 1023;
        }

        synchronized int nestSame(int x) {
            synchronized (a) {
                synchronized (a) {
                    o += helper(x);
                }
            }
            return o;
        }

        synchronized int nestDiff(int x) {
            synchronized (a) {
                synchronized (bb) {
                    o += helper(x);
                }
            }
            return o;
        }

        synchronized int nestDeep(int x) {
            synchronized (a) {
                synchronized (bb) {
                    synchronized (this) {
                        o += helper(x);
                        if ((x & 4095) == 17 && Thread.holdsLock(a) && Thread.holdsLock(bb) && Thread.holdsLock(this)) heldInner++;
                    }
                }
            }
            return o;
        }

        int plainNest(int x) {
            synchronized (a) {
                synchronized (bb) {
                    o += helper(x);
                }
            }
            return o;
        }

        static final ArithmeticException BOOM = new ArithmeticException("boom");

        synchronized int nestThrow(int x) {
            synchronized (a) {
                synchronized (bb) {
                    if ((x & 63) == 5) throw BOOM;
                    o += helper(x);
                }
            }
            return o;
        }

        synchronized int nestCatch(int x) {
            synchronized (a) {
                try {
                    synchronized (bb) {
                        if ((x & 63) == 9) throw BOOM;
                        o += helper(x);
                    }
                } catch (ArithmeticException e) {
                    if (e == BOOM && Thread.holdsLock(a) && !Thread.holdsLock(bb) && Thread.holdsLock(this)) caughtInside++;
                }
            }
            return o;
        }

        // IMSE inside nested blocks: notify a monitor not held; the held ones stay held.
        synchronized int nestImse(int x) {
            synchronized (a) {
                synchronized (bb) {
                    try {
                        if ((x & 255) == 3) SENTINEL_LOCK.notify();
                        o += helper(x);
                    } catch (IllegalMonitorStateException e) {
                        if (Thread.holdsLock(a) && Thread.holdsLock(bb) && Thread.holdsLock(this)) imse++;
                    }
                }
            }
            return o;
        }

        // A virtual call inside the nested blocks; the receiver class changes after warm-up.
        synchronized int nestDeopt(int x) {
            synchronized (a) {
                synchronized (bb) {
                    o += shape.area(x);
                    if (Thread.holdsLock(a) && Thread.holdsLock(bb) && Thread.holdsLock(this)) heldMethod++;
                }
            }
            return o;
        }

        // wait inside nested blocks: releases only bb (the waited monitor), the others stay held.
        synchronized void nestWait(boolean[] go) throws InterruptedException {
            synchronized (a) {
                synchronized (bb) {
                    while (!go[0]) bb.wait(10_000);
                    if (Thread.holdsLock(a) && Thread.holdsLock(bb) && Thread.holdsLock(this)) waited++;
                }
            }
        }
    }

    static class Shape {
        int area(int x) {
            return x & 7;
        }
    }

    static class Shape2 extends Shape {
        @Override
        int area(int x) {
            return (x & 7) + 1;
        }
    }

    // -------- recursion with a table --------
    static final class R {
        int b, depthHeld, caught;
        R peer;

        int helper(int x) {
            b += x;
            return b & 1023;
        }

        synchronized int rec(int n) {
            try {
                if (n == 0) {
                    if (Thread.holdsLock(this)) depthHeld++;
                    return helper(1);
                }
                return rec(n - 1) + 1;
            } catch (ArithmeticException e) {
                caught++;
                return -1;
            }
        }

        // Alternates receivers: each level must lock ITS receiver.
        synchronized int recOther(int n) {
            try {
                if (n == 0) {
                    if (Thread.holdsLock(this) && Thread.holdsLock(peer)) depthHeld++;
                    return helper(1);
                }
                return peer.recOther(n - 1) + 1;
            } catch (ArithmeticException e) {
                caught++;
                return -1;
            }
        }

        int plainRec(int n) {
            try {
                if (n == 0) return helper(1);
                return plainRec(n - 1) + 1;
            } catch (ArithmeticException e) {
                caught++;
                return -1;
            }
        }

        // A throw from the deepest level crosses every synchronized activation.
        synchronized int recThrow(int n) {
            if (n == 0) throw N.BOOM;
            return recThrow(n - 1) + 1;
        }
    }

    // -------- static synchronized callee --------
    static final class S {
        static int count, held;

        static synchronized int sinc(int x) {
            count += x & 3;
            if ((x & 4095) == 11 && Thread.holdsLock(S.class)) held++;
            return count;
        }

        static int plainInc(int x) {
            count += x & 3;
            return count;
        }
    }

    // -------- array loop --------
    static final class Arr {
        final int[] data = new int[64];

        synchronized int sum(int x) {
            int s = 0;
            for (int i = 0; i < data.length; i++) s += data[i] + x;
            data[x & 63]++;
            return s;
        }
    }

    // -------- drivers: warm and measure in the same method --------
    static long best(long[] t) {
        long b = Long.MAX_VALUE;
        for (long v : t) b = Math.min(b, v);
        return b;
    }

    static long driveNest(String label, int kind, N n, int rounds) throws InterruptedException {
        final int chunk = 1000;
        long check = 0;
        long[] t = new long[rounds];
        for (int phase = 0; phase < 2 + rounds; phase++) {
            int reps = phase == 0 ? 300 : phase == 1 ? 100 : 200;
            if (phase == 1) Thread.sleep(100); // let background compiles publish
            long t0 = System.nanoTime();
            for (int k = 0; k < reps; k++) {
                for (int i = 0; i < chunk; i++) {
                    int x = k * chunk + i;
                    switch (kind) {
                        case 0: check += n.nestSame(x); break;
                        case 1: check += n.nestDiff(x); break;
                        case 2: check += n.nestDeep(x); break;
                        case 3:
                            try {
                                check += n.nestThrow(x);
                            } catch (ArithmeticException e) {
                                if (e == N.BOOM && !Thread.holdsLock(n) && !Thread.holdsLock(n.a) && !Thread.holdsLock(n.bb)) check += 7;
                                else check -= 1_000_000;
                            }
                            break;
                        case 4: check += n.nestCatch(x); break;
                        case 5: check += n.plainNest(x); break;
                        default: check += n.nestImse(x); break;
                    }
                }
            }
            if (phase >= 2) t[phase - 2] = (System.nanoTime() - t0) / ((long) reps * chunk);
        }
        System.out.println("time " + label + " " + best(t) + " ns");
        return check;
    }

    static long driveRec(String label, int kind, R r, int rounds) throws InterruptedException {
        final int chunk = 1000;
        long check = 0;
        long[] t = new long[rounds];
        for (int phase = 0; phase < 2 + rounds; phase++) {
            int reps = phase == 0 ? 300 : phase == 1 ? 100 : 200;
            if (phase == 1) Thread.sleep(100);
            long t0 = System.nanoTime();
            for (int k = 0; k < reps; k++) {
                for (int i = 0; i < chunk; i++) {
                    int d = (k + i) & 7;
                    switch (kind) {
                        case 0: check += r.rec(d); break;
                        case 1: check += r.recOther(d); break;
                        default: check += r.plainRec(d); break;
                    }
                }
            }
            if (phase >= 2) t[phase - 2] = (System.nanoTime() - t0) / ((long) reps * chunk);
        }
        System.out.println("time " + label + " " + best(t) + " ns");
        return check;
    }

    static long driveStatic(String label, boolean sync, int rounds) throws InterruptedException {
        final int chunk = 1000;
        long check = 0;
        long[] t = new long[rounds];
        for (int phase = 0; phase < 2 + rounds; phase++) {
            int reps = phase == 0 ? 300 : phase == 1 ? 100 : 200;
            if (phase == 1) Thread.sleep(100);
            long t0 = System.nanoTime();
            for (int k = 0; k < reps; k++) {
                for (int i = 0; i < chunk; i++) {
                    int x = k * chunk + i;
                    check += sync ? S.sinc(x) : S.plainInc(x);
                }
            }
            if (phase >= 2) t[phase - 2] = (System.nanoTime() - t0) / ((long) reps * chunk);
        }
        System.out.println("time " + label + " " + best(t) + " ns");
        return check;
    }

    static long driveArr(Arr a, int rounds) throws InterruptedException {
        final int chunk = 1000;
        long check = 0;
        long[] t = new long[rounds];
        for (int phase = 0; phase < 2 + rounds; phase++) {
            int reps = phase == 0 ? 100 : phase == 1 ? 50 : 50;
            if (phase == 1) Thread.sleep(100);
            long t0 = System.nanoTime();
            for (int k = 0; k < reps; k++) {
                for (int i = 0; i < chunk; i++) check += a.sum(k + i);
            }
            if (phase >= 2) t[phase - 2] = (System.nanoTime() - t0) / ((long) reps * chunk);
        }
        System.out.println("time arr-loop " + best(t) + " ns");
        return check;
    }

    static boolean freeForAnotherThread(Object lock) throws InterruptedException {
        boolean[] got = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (lock) {
                got[0] = true;
            }
        });
        t.start();
        t.join(10_000);
        return got[0] && !t.isAlive();
    }

    static String state(N n) throws InterruptedException {
        return "held=" + Thread.holdsLock(n) + "," + Thread.holdsLock(n.a) + "," + Thread.holdsLock(n.bb)
                + " free=" + freeForAnotherThread(n) + "," + freeForAnotherThread(n.a) + "," + freeForAnotherThread(n.bb);
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        String[] nestLabels = {"nest-same", "nest-diff", "nest-deep", "nest-throw", "nest-catch", "nest-plain", "nest-imse"};
        for (int kind = 0; kind < nestLabels.length; kind++) {
            N n = new N();
            long check = driveNest(nestLabels[kind], kind, n, rounds);
            System.out.println("fact " + nestLabels[kind] + " check=" + check + " o=" + n.o + " b=" + n.b
                    + " inner-held=" + n.heldInner + " caught-inside=" + n.caughtInside + " imse=" + n.imse + " " + state(n));
        }

        // Deopt while nested monitors are held: warm monomorphic, then a second receiver class.
        N d = new N();
        long dcheck = 0;
        for (int i = 0; i < 300_000; i++) dcheck += d.nestDeopt(i);
        Thread.sleep(100);
        N.shape = new Shape2();
        for (int i = 0; i < 300_000; i++) dcheck += d.nestDeopt(i);
        System.out.println("fact nest-deopt check=" + dcheck + " o=" + d.o + " held-all=" + d.heldMethod + " " + state(d));

        // Exception identity: the same object arrives at the outermost catch, every monitor free.
        N e = new N();
        int same = 0;
        for (int i = 0; i < 64 * 200; i++) {
            try {
                e.nestThrow(i);
            } catch (ArithmeticException ex) {
                if (ex == N.BOOM) same++;
            }
        }
        System.out.println("fact nest-throw-identity same=" + same + " " + state(e));

        // wait/notify inside nested blocks.
        N w = new N();
        boolean[] go = new boolean[1];
        Thread waiter = new Thread(() -> {
            try {
                w.nestWait(go);
            } catch (InterruptedException ie) {
                throw new IllegalStateException(ie);
            }
        });
        waiter.start();
        // The waiter releases only bb while it waits: this thread can take bb, and not a or w.
        Thread.sleep(200);
        boolean aBusy = !freeWithin(w.a, 200);
        boolean wBusy = !freeWithin(w, 200);
        synchronized (w.bb) {
            go[0] = true;
            w.bb.notifyAll();
        }
        waiter.join(30_000);
        System.out.println("fact nest-wait waited=" + w.waited + " a-busy=" + aBusy + " m-busy=" + wBusy
                + " alive=" + waiter.isAlive() + " " + state(w));

        // Recursion with a table.
        String[] recLabels = {"rec-table", "rec-other", "rec-plain"};
        for (int kind = 0; kind < recLabels.length; kind++) {
            R r = new R();
            r.peer = new R();
            r.peer.peer = r;
            long check = driveRec(recLabels[kind], kind, r, rounds);
            System.out.println("fact " + recLabels[kind] + " check=" + check + " b=" + (r.b + r.peer.b)
                    + " depth-held=" + (r.depthHeld + r.peer.depthHeld) + " caught=" + (r.caught + r.peer.caught)
                    + " held=" + Thread.holdsLock(r) + "," + Thread.holdsLock(r.peer)
                    + " free=" + freeForAnotherThread(r) + "," + freeForAnotherThread(r.peer));
        }
        R rt = new R();
        int crossed = 0;
        for (int i = 0; i < 20_000; i++) {
            try {
                rt.recThrow(i & 7);
            } catch (ArithmeticException ex) {
                if (ex == N.BOOM && !Thread.holdsLock(rt)) crossed++;
            }
        }
        System.out.println("fact rec-throw crossed=" + crossed + " free=" + freeForAnotherThread(rt));

        // Static synchronized callee.
        long sc = driveStatic("static-loop", true, rounds);
        long sp = driveStatic("static-plain", false, rounds);
        System.out.println("fact static check=" + (sc + sp) + " count=" + S.count + " held=" + S.held
                + " outside-held=" + Thread.holdsLock(S.class) + " free=" + freeForAnotherThread(S.class));

        Arr arr = new Arr();
        long ac = driveArr(arr, rounds);
        System.out.println("fact arr-loop check=" + ac + " free=" + freeForAnotherThread(arr));

        // Contention through the nested bodies: two threads, one receiver; o and b order-independent.
        N shared = new N();
        Thread[] ts = new Thread[2];
        for (int k = 0; k < ts.length; k++) {
            ts[k] = new Thread(() -> {
                for (int i = 0; i < 200_000; i++) {
                    shared.nestDiff(1);
                    shared.nestSame(1);
                }
            });
            ts[k].start();
        }
        for (Thread th : ts) th.join();
        System.out.println("fact nest-contended o-mod=" + (shared.o & 1023) + " b=" + shared.b + " " + state(shared));
    }

    static boolean freeWithin(Object lock, long millis) throws InterruptedException {
        boolean[] got = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (lock) {
                got[0] = true;
            }
        });
        t.setDaemon(true);
        t.start();
        t.join(millis);
        return got[0];
    }
}
