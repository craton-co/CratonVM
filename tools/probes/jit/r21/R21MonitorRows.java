// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 4, lane monitors: monitors in compiled code, as facts a
// HotSpot 25 run can be diffed against, plus timing rows.
//
// Run (both modes, then HotSpot 25):
//
//   cratonvm -Xmx1g -Djdk.virtualThreadScheduler.parallelism=2 \
//            -Djdk.virtualThreadScheduler.maxPoolSize=2 R21MonitorRows.java
//   cratonvm --compatible  (same flags)
//   java     (same flags)
//
// Arms for the round-21 switches (each default ON; `=0` restores the defect):
//   CRATONVM_JIT_MONITOR_BALANCE_GATE=0        the single-pass tier compiles
//                                              unbalanced monitor bytecode
//   CRATONVM_JIT_MONITOR_BALANCE_OPERANDS=0    ... including exitWrong (exit of
//                                              another local at the same depth)
//   CRATONVM_JIT_MONITOR_BALANCE_TRAPS=0       ... and implicit traps under a
//                                              lock with no catch-all
//   CRATONVM_JIT_MONITOR_EXIT_NULL_IMSE_TEXT=0 the table's text on a refused
//                                              compiled monitorexit
//   CRATONVM_MONITOREXIT_FOREIGN_HOLD_IMSE=0   an interpreted exitOnly releases
//                                              a COMPILED caller's hold
//                                              (exit-caller-held still-held<N
//                                              once `unbalanced` is OSR'd)
//
// Fact lines (`fact ...`) must equal HotSpot's byte for byte. Expected on
// HotSpot 25:
//
//   fact imse wait msg=current thread is not owner n=20000
//   fact imse wait1 msg=current thread is not owner n=20000
//   fact imse notify msg=current thread is not owner n=20000
//   fact imse notifyAll msg=current thread is not owner n=20000
//   fact iae wait-negative msg=timeout value is negative n=20000
//   fact iae wait-nanos msg=nanosecond timeout value out of range n=20000
//   fact holdslock-deopt bad=0 n=200000 held-after=false
//   fact pingpong a=20000 b=20000 turn=0
//   fact nested-throw thrown=12500 sum=<deterministic> heldA=false heldB=false other-thread=true
//   fact unbalanced enter-only imse=30000 msg=null held-after=false
//   fact unbalanced exit-only imse=30000 msg=null
//   fact unbalanced exit-caller-held imse=30000 msg=null still-held=30000
//   fact unbalanced exit-wrong-object imse=30000 msg=null held-after=false
//   fact vthread-monitor-unmount done=true count=4
//   fact counts sync=<N> reentrant=<N> sbuf-len=<N> method=<N>
//
// The `unbalanced` rows use a class built here byte by byte (javac cannot emit
// unbalanced monitors): `enterOnly(o)` is `aload_0; monitorenter; return`,
// `exitOnly(o)` is `aload_0; monitorexit; return`, `exitWrong(a, b)` is
// `aload_0; monitorenter; aload_1; monitorexit; return`. HotSpot never
// compiles them (unbalanced monitors), so every call takes the interpreter's
// structured-locking checks. Before round 21 CratonVM's single-pass tier
// compiled the first two: enter-only then returned normally with the lock
// held (imse < 30000, held-after=true), exit-only released the caller's hold
// in exit-caller-held, and exitWrong unwound with `a` held. See the page
// docs/internal/fixed-bugs/r21-single-pass-compiles-unbalanced-monitor-bytecode-FIXED-20261018.md
// (to retire to docs/internal/fixed-bugs/ once verified).
//
// Timing lines `time <row> <N> ns/op` (median of 9 reps after 3 warm-ups):
// sync (uncontended block on a shared lock), reentrant3 (3-deep same lock),
// sbuf (StringBuffer.append(char)), syncmethod (synchronized instance method
// call). HotSpot 25 lightweight locking: roughly 15-25 ns for sync, a few ns
// more for reentrant3 (C2 elides the nested pairs), sbuf and syncmethod are
// often 2-6 ns (C2 coarsens / inlines).

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.concurrent.locks.LockSupport;

public class R21MonitorRows {
    static volatile int sink;

    public static void main(String[] args) throws Throwable {
        imseTexts();
        holdsLockAfterDeopt();
        pingPong();
        nestedThrow();
        unbalanced();
        vthreadUnmount();
        timing();
    }

    // ---- IllegalMonitorStateException / IllegalArgumentException texts ----

    static String waitOnce(Object o, int kind) {
        try {
            switch (kind) {
                case 0 -> o.wait();
                case 1 -> o.wait(1L);
                case 2 -> o.notify();
                case 3 -> o.notifyAll();
                case 4 -> { synchronized (o) { o.wait(-1L); } }
                default -> { synchronized (o) { o.wait(1L, 1_000_000); } }
            }
            return "none";
        } catch (IllegalMonitorStateException | IllegalArgumentException e) {
            return e.getMessage();
        } catch (InterruptedException e) {
            return "interrupted";
        }
    }

    static void imseTexts() {
        String[] names = {"imse wait", "imse wait1", "imse notify", "imse notifyAll",
                          "iae wait-negative", "iae wait-nanos"};
        Object o = new Object();
        for (int kind = 0; kind < names.length; kind++) {
            String msg = null;
            int n = 0;
            for (int i = 0; i < 20000; i++) {
                String m = waitOnce(o, kind);
                if (!"none".equals(m)) {
                    n++;
                    if (msg == null) msg = m;
                }
            }
            System.out.println("fact " + names[kind] + " msg=" + msg + " n=" + n);
        }
    }

    // ---- holdsLock inside a synchronized method across a deopt ----

    interface Shape { int area(int i); }
    static final class Sq implements Shape { public int area(int i) { return i * i; } }
    static final class Tri implements Shape { public int area(int i) { return i * 3; } }
    static final class Circ implements Shape { public int area(int i) { return i + 7; } }

    static final class Box {
        Shape s = new Sq();
        int n;
        int bad;

        synchronized int step(int i) {
            int r = s.area(i);
            if (!Thread.holdsLock(this)) bad++;
            n++;
            return r;
        }
    }

    static int drive(Box b, int from, int to) {
        int acc = 0;
        for (int i = from; i < to; i++) acc += b.step(i);
        return acc;
    }

    static void holdsLockAfterDeopt() {
        Box b = new Box();
        int acc = drive(b, 0, 100000);
        b.s = new Tri(); // a new receiver class at a monomorphic site
        acc += drive(b, 100000, 150000);
        b.s = new Circ();
        acc += drive(b, 150000, 200000);
        sink = acc;
        System.out.println("fact holdslock-deopt bad=" + b.bad + " n=" + b.n
                + " held-after=" + Thread.holdsLock(b));
    }

    // ---- wait / notifyAll hand-off between two threads ----

    static final Object PP = new Object();
    static int turn;

    static int pingPongSide(int me, int rounds) throws InterruptedException {
        int done = 0;
        for (int i = 0; i < rounds; i++) {
            synchronized (PP) {
                while (turn != me) PP.wait();
                turn = 1 - me;
                done++;
                PP.notifyAll();
            }
        }
        return done;
    }

    static void pingPong() throws InterruptedException {
        int[] got = new int[2];
        Thread b = new Thread(() -> {
            try { got[1] = pingPongSide(1, 20000); } catch (InterruptedException e) { }
        });
        b.start();
        got[0] = pingPongSide(0, 20000);
        b.join();
        System.out.println("fact pingpong a=" + got[0] + " b=" + got[1] + " turn=" + turn);
    }

    // ---- an exception thrown out of nested synchronized blocks ----

    static int nested(Object a, Object b, int i) {
        synchronized (a) {
            synchronized (b) {
                if ((i & 7) == 0) throw new IllegalStateException("i" + (i & 0xff));
                return i ^ (i >>> 3);
            }
        }
    }

    static void nestedThrow() throws InterruptedException {
        Object a = new Object(), b = new Object();
        int thrown = 0;
        long sum = 0;
        for (int i = 0; i < 100000; i++) {
            try {
                sum += nested(a, b, i);
            } catch (IllegalStateException e) {
                thrown++;
                sum += e.getMessage().length();
            }
        }
        boolean[] other = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (a) { synchronized (b) { other[0] = true; } }
        });
        t.setDaemon(true);
        t.start();
        t.join(10000);
        System.out.println("fact nested-throw thrown=" + thrown + " sum=" + sum
                + " heldA=" + Thread.holdsLock(a) + " heldB=" + Thread.holdsLock(b)
                + " other-thread=" + other[0]);
    }

    // ---- unbalanced monitor bytecode (a hand-built class) ----

    static void utf(DataOutputStream d, String s) throws IOException {
        d.writeByte(1);
        d.writeUTF(s);
    }

    static void method(DataOutputStream d, int name, int desc, int maxLocals, byte[] code)
            throws IOException {
        d.writeShort(0x0009); // public static
        d.writeShort(name);
        d.writeShort(desc);
        d.writeShort(1);
        d.writeShort(7); // "Code"
        d.writeInt(12 + code.length);
        d.writeShort(1); // max_stack
        d.writeShort(maxLocals);
        d.writeInt(code.length);
        d.write(code);
        d.writeShort(0); // exception table
        d.writeShort(0); // attributes
    }

    static byte[] unbalancedClass() throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream d = new DataOutputStream(bos);
        d.writeInt(0xCAFEBABE);
        d.writeShort(0);
        d.writeShort(52);
        d.writeShort(11); // constant pool count: entries 1..10
        utf(d, "R21MonUnbal");               // 1
        d.writeByte(7); d.writeShort(1);     // 2 Class R21MonUnbal
        utf(d, "java/lang/Object");          // 3
        d.writeByte(7); d.writeShort(3);     // 4 Class Object
        utf(d, "enterOnly");                 // 5
        utf(d, "(Ljava/lang/Object;)V");     // 6
        utf(d, "Code");                      // 7
        utf(d, "exitOnly");                  // 8
        utf(d, "exitWrong");                 // 9
        utf(d, "(Ljava/lang/Object;Ljava/lang/Object;)V"); // 10
        d.writeShort(0x0021); // public super
        d.writeShort(2);
        d.writeShort(4);
        d.writeShort(0); // interfaces
        d.writeShort(0); // fields
        d.writeShort(3); // methods
        method(d, 5, 6, 1, new byte[] {0x2a, (byte) 0xc2, (byte) 0xb1});
        method(d, 8, 6, 1, new byte[] {0x2a, (byte) 0xc3, (byte) 0xb1});
        method(d, 9, 10, 2,
                new byte[] {0x2a, (byte) 0xc2, 0x2b, (byte) 0xc3, (byte) 0xb1});
        d.writeShort(0); // class attributes
        d.flush();
        return bos.toByteArray();
    }

    static final class Loader extends ClassLoader {
        Loader() { super(R21MonitorRows.class.getClassLoader()); }
        Class<?> define(byte[] b) { return defineClass("R21MonUnbal", b, 0, b.length); }
    }

    static String callOne(MethodHandle mh, Object o) {
        try {
            mh.invokeExact(o);
            return "none";
        } catch (IllegalMonitorStateException e) {
            return String.valueOf(e.getMessage());
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static String callTwo(MethodHandle mh, Object a, Object b) {
        try {
            mh.invokeExact(a, b);
            return "none";
        } catch (IllegalMonitorStateException e) {
            return String.valueOf(e.getMessage());
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static void unbalanced() throws Throwable {
        Class<?> c = new Loader().define(unbalancedClass());
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType one = MethodType.methodType(void.class, Object.class);
        MethodHandle enterOnly = l.findStatic(c, "enterOnly", one);
        MethodHandle exitOnly = l.findStatic(c, "exitOnly", one);
        MethodHandle exitWrong = l.findStatic(c, "exitWrong",
                MethodType.methodType(void.class, Object.class, Object.class));
        final int n = 30000;

        Object o = new Object();
        int imse = 0;
        String msg = "unset";
        for (int i = 0; i < n; i++) {
            String m = callOne(enterOnly, o);
            if (!"none".equals(m)) { imse++; msg = m; }
        }
        System.out.println("fact unbalanced enter-only imse=" + imse + " msg=" + msg
                + " held-after=" + Thread.holdsLock(o));

        Object p = new Object();
        imse = 0;
        msg = "unset";
        for (int i = 0; i < n; i++) {
            String m = callOne(exitOnly, p);
            if (!"none".equals(m)) { imse++; msg = m; }
        }
        System.out.println("fact unbalanced exit-only imse=" + imse + " msg=" + msg);

        Object q = new Object();
        imse = 0;
        msg = "unset";
        int stillHeld = 0;
        for (int i = 0; i < n; i++) {
            synchronized (q) {
                String m = callOne(exitOnly, q);
                if (!"none".equals(m)) { imse++; msg = m; }
                if (Thread.holdsLock(q)) stillHeld++;
            }
        }
        System.out.println("fact unbalanced exit-caller-held imse=" + imse + " msg=" + msg
                + " still-held=" + stillHeld);

        Object a = new Object(), b = new Object();
        imse = 0;
        msg = "unset";
        for (int i = 0; i < n; i++) {
            String m = callTwo(exitWrong, a, b);
            if (!"none".equals(m)) { imse++; msg = m; }
        }
        System.out.println("fact unbalanced exit-wrong-object imse=" + imse + " msg=" + msg
                + " held-after=" + Thread.holdsLock(a));
    }

    // ---- a virtual thread blocked in synchronized must not stall the rest (JEP 491) ----

    static final Object VT_LOCK = new Object();
    static volatile boolean vtRelease;
    static int vtCount;

    static void vthreadUnmount() throws InterruptedException {
        Thread[] owner = new Thread[1];
        owner[0] = Thread.ofVirtual().unstarted(() -> {
            synchronized (VT_LOCK) {
                while (!vtRelease) LockSupport.park();
            }
        });
        owner[0].start();
        Thread.sleep(50);
        Thread[] blockers = new Thread[4];
        for (int i = 0; i < blockers.length; i++) {
            blockers[i] = Thread.ofVirtual().start(() -> {
                synchronized (VT_LOCK) { vtCount++; }
            });
        }
        Thread.sleep(50);
        Thread unparker = Thread.ofVirtual().start(() -> {
            vtRelease = true;
            LockSupport.unpark(owner[0]);
        });
        boolean done = true;
        long deadline = System.nanoTime() + 20_000_000_000L;
        for (Thread t : new Thread[] {unparker, owner[0], blockers[0], blockers[1],
                                      blockers[2], blockers[3]}) {
            long left = (deadline - System.nanoTime()) / 1_000_000L;
            if (left <= 0 || !t.join(java.time.Duration.ofMillis(left))) done = false;
        }
        int count;
        synchronized (VT_LOCK) { count = vtCount; }
        System.out.println("fact vthread-monitor-unmount done=" + done + " count=" + count);
    }

    // ---- timing ----

    static final Object LOCK = new Object();
    static int syncCounter;

    static int syncBlock(int n) {
        int x = 0;
        for (int i = 0; i < n; i++) {
            synchronized (LOCK) { syncCounter++; x += i; }
        }
        return x;
    }

    static int reentrant3(int n) {
        int x = 0;
        for (int i = 0; i < n; i++) {
            synchronized (LOCK) {
                synchronized (LOCK) {
                    synchronized (LOCK) { syncCounter++; x ^= i; }
                }
            }
        }
        return x;
    }

    static int sbuf(StringBuffer sb, int n) {
        for (int i = 0; i < n; i++) {
            if (sb.length() >= 4096) sb.setLength(0);
            sb.append((char) ('a' + (i & 15)));
        }
        return sb.length();
    }

    static final class Counter {
        int v;
        synchronized int inc(int d) { v += d; return v; }
    }

    static int syncMethod(Counter c, int n) {
        int x = 0;
        for (int i = 0; i < n; i++) x += c.inc(i & 3);
        return x;
    }

    interface Row { int run(int n); }

    static long timeRow(String name, Row r, int n) {
        for (int w = 0; w < 3; w++) sink = r.run(n);
        long[] t = new long[9];
        for (int k = 0; k < t.length; k++) {
            long s = System.nanoTime();
            sink = r.run(n);
            t[k] = System.nanoTime() - s;
        }
        Arrays.sort(t);
        double ns = (double) t[t.length / 2] / n;
        System.out.println("time " + name + " " + String.format("%.2f", ns) + " ns/op");
        return n;
    }

    static void timing() {
        final int n = 2_000_000;
        syncCounter = 0;
        long ops = timeRow("sync", R21MonitorRows::syncBlock, n);
        int syncAfter = syncCounter;
        syncCounter = 0;
        timeRow("reentrant3", R21MonitorRows::reentrant3, n);
        int reentrantAfter = syncCounter;
        StringBuffer sb = new StringBuffer();
        timeRow("sbuf", k -> sbuf(sb, k), n);
        Counter c = new Counter();
        timeRow("syncmethod", k -> syncMethod(c, k), n);
        // 12 runs of n each per row (3 warm-ups + 9 reps).
        System.out.println("fact counts sync=" + syncAfter + " reentrant=" + reentrantAfter
                + " sbuf-len=" + sb.length() + " method=" + c.v + " ops=" + ops);
    }
}
