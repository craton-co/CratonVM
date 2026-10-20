// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 25, lane fuzz10: volatile stores into freshly allocated
// objects that are then published through a STATIC field and read by a
// second thread. The writer and the reader run in lockstep (the writer builds
// round r only after the reader acknowledged r - 1); results are counted by
// the reader and printed by main only after join(), so the output is
// deterministic on a correct VM.
//
// Shapes (20000 rounds each unless noted):
//   P0 volatile long / double / Object stores into a fresh Box, plain field
//      before them, published through a static VOLATILE field; the reader also
//      checks the volatile long is not torn (high and low halves both r)
//   P1 the same Box published through a PLAIN static field followed by a
//      volatile static sequence store; the reader spins on the sequence
//   P2 published with VarHandle setRelease into a static array slot, read
//      with getAcquire
//   P3 four fresh Boxes per round, all with volatile stores, only the one at
//      r % 4 published (the others die): an escape that depends on a branch
//   P4 published FIRST through a plain static field (reader reads it with
//      VarHandle getOpaque), then the plain payload, then the volatile flag
//      LAST; the reader spins on the flag, then must see the payload
//   P5 a superclass constructor publishes `this` to a static volatile at its
//      end; the subclass constructor then stores its plain q and volatile w;
//      the reader sees the super's fields at once, spins on w, then checks q
//   P6 (2000 rounds) a fresh Box with volatile stores handed to a NEW thread
//      through its constructor; Thread.start / join are the only edges
// Each writer also reads its own fresh object back right after building it
// ("self": store-to-load forwarding across an elided fence).
//
// Risk targeted: CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED (a volatile
// store into a not-yet-escaped allocation needs no fence; wave 24 adds the
// JVMTI-locals gate): an escape the analysis misses (a static store, a
// VarHandle release store, a branch-dependent escape, `this` published by a
// superclass constructor, a capture by a new thread).
//
// Run:
//   javac -d out R21Fz10FreshVolPub1.java
//   java -Xmx256m -cp out R21Fz10FreshVolPub1                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz10FreshVolPub1              # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz10FreshVolPub1
//   cratonvm --nojit -Xmx256m -cp out R21Fz10FreshVolPub1
//   (extra arm: CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED=1)
//
// HotSpot 25 expectation (JLS 17.4.4-17.4.5 happens-before, 17.7 atomic
// volatile long / double; not yet run):
//   P0 rounds=20000 bad=0 stuck=0 self=0
//   P1 rounds=20000 bad=0 stuck=0 self=0
//   P2 rounds=20000 bad=0 stuck=0 self=0
//   P3 rounds=20000 bad=0 stuck=0 self=0
//   P4 rounds=20000 bad=0 stuck=0 self=0
//   P5 rounds=20000 bad=0 stuck=0 self=0
//   P6 rounds=2000 bad=0 stuck=0 self=0
// A non-zero stuck can be a starved reader on a loaded host: rerun before
// filing. A non-zero bad or self is never noise.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class R21Fz10FreshVolPub1 {
    static final int ROUNDS = 20_000;
    static final long SPIN_LIMIT = 200_000_000L;

    static final class Box {
        int round;
        int plain;
        volatile long lv;
        volatile double dv;
        volatile Object ref;
        volatile int flag;

        Box(int r) {
            this.round = r;
        }
    }

    static long pack(int r) {
        return ((long) r << 32) | (r & 0xFFFFFFFFL);
    }

    static boolean full(Box b, int r) {
        Object ref = b.ref;
        return b.round == r && b.plain == r && b.lv == pack(r) && b.dv == r + 0.5
                && ref instanceof int[] a && a.length == 1 && a[0] == r;
    }

    static Box fill(int r) {
        Box b = new Box(r);
        b.plain = r;
        b.lv = pack(r);
        b.dv = r + 0.5;
        b.ref = new int[] {r};
        return b;
    }

    // ---- publication fields ----
    static volatile Box p0;
    static Box p1;
    static volatile int seq1;
    static final Box[] P2 = new Box[1];
    static final VarHandle AE = MethodHandles.arrayElementVarHandle(Box[].class);
    static volatile Box p3;
    static Box p4;
    static final VarHandle P4VH;
    static volatile Sup p5;

    static {
        try {
            P4VH = MethodHandles.lookup().findStaticVarHandle(R21Fz10FreshVolPub1.class, "p4", Box.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static class Sup {
        int round;
        int plain;
        volatile long lv;

        Sup(int r) {
            round = r;
            plain = r;
            lv = pack(r);
            p5 = this;
        }
    }

    static final class Sub extends Sup {
        int q;
        volatile int w;

        Sub(int r) {
            super(r);
            q = r;
            w = r;
        }
    }

    // ---- lockstep harness ----
    static volatile int ack;
    static int self;

    interface Writer {
        void write(int r);
    }

    interface Reader {
        /** 0 ok, 1 bad, 2 stuck. */
        int read(int r);
    }

    static void spinWait(long s) {
        if ((s & 1023) == 0) {
            Thread.yield();
        } else {
            Thread.onSpinWait();
        }
    }

    static void shape(String name, Writer w, Reader rd) throws InterruptedException {
        ack = 0;
        self = 0;
        final int[] res = new int[3];
        Thread t = new Thread(() -> {
            for (int r = 1; r <= ROUNDS; r++) {
                res[rd.read(r)]++;
                ack = r;
            }
        }, "reader-" + name);
        t.start();
        int writerStuck = 0;
        for (int r = 1; r <= ROUNDS; r++) {
            w.write(r);
            long s = 0;
            while (ack < r) {
                if (++s > SPIN_LIMIT) {
                    writerStuck++;
                    break;
                }
                spinWait(s);
            }
        }
        t.join();
        System.out.println(name + " rounds=" + (res[0] + res[1] + res[2]) + " bad=" + res[1] + " stuck="
                + (res[2] + writerStuck) + " self=" + self);
    }

    // ---- writers (one per shape, hot) ----
    static void w0(int r) {
        Box b = fill(r);
        if (!full(b, r)) {
            self++;
        }
        p0 = b;
    }

    static void w1(int r) {
        Box b = fill(r);
        if (!full(b, r)) {
            self++;
        }
        p1 = b;
        seq1 = r;
    }

    static void w2(int r) {
        Box b = fill(r);
        if (!full(b, r)) {
            self++;
        }
        AE.setRelease(P2, 0, b);
    }

    static void w3(int r) {
        Box keep = null;
        for (int k = 0; k < 4; k++) {
            Box b = fill(r);
            b.flag = k;
            if (k == (r & 3)) {
                keep = b;
            }
        }
        if (keep == null || !full(keep, r) || keep.flag != (r & 3)) {
            self++;
        }
        p3 = keep;
    }

    static void w4(int r) {
        Box b = new Box(r);
        p4 = b;
        b.plain = r;
        b.lv = pack(r);
        b.dv = r + 0.5;
        b.ref = new int[] {r};
        b.flag = r;
        if (!full(b, r) || b.flag != r) {
            self++;
        }
    }

    static void w5(int r) {
        Sub s = new Sub(r);
        if (s.round != r || s.plain != r || s.lv != pack(r) || s.q != r || s.w != r) {
            self++;
        }
    }

    // ---- readers ----
    static int r0(int r) {
        long s = 0;
        Box b;
        while ((b = p0) == null || b.round != r) {
            if (++s > SPIN_LIMIT) {
                return 2;
            }
            spinWait(s);
        }
        return full(b, r) ? 0 : 1;
    }

    static int r1(int r) {
        long s = 0;
        while (seq1 != r) {
            if (++s > SPIN_LIMIT) {
                return 2;
            }
            spinWait(s);
        }
        Box b = p1;
        return b != null && full(b, r) ? 0 : 1;
    }

    static int r2(int r) {
        long s = 0;
        Box b;
        while ((b = (Box) AE.getAcquire(P2, 0)) == null || b.round != r) {
            if (++s > SPIN_LIMIT) {
                return 2;
            }
            spinWait(s);
        }
        return full(b, r) ? 0 : 1;
    }

    static int r3(int r) {
        long s = 0;
        Box b;
        while ((b = p3) == null || b.round != r) {
            if (++s > SPIN_LIMIT) {
                return 2;
            }
            spinWait(s);
        }
        return full(b, r) && b.flag == (r & 3) ? 0 : 1;
    }

    static int r4(int r) {
        long s = 0;
        Box b;
        while ((b = (Box) P4VH.getOpaque()) == null || b.flag != r) {
            if (++s > SPIN_LIMIT) {
                return 2;
            }
            spinWait(s);
        }
        return full(b, r) ? 0 : 1;
    }

    static int r5(int r) {
        long s = 0;
        Sup b;
        while ((b = p5) == null || b.round != r) {
            if (++s > SPIN_LIMIT) {
                return 2;
            }
            spinWait(s);
        }
        if (b.plain != r || b.lv != pack(r) || !(b instanceof Sub sub)) {
            return 1;
        }
        while (sub.w != r) {
            if (++s > SPIN_LIMIT) {
                return 2;
            }
            spinWait(s);
        }
        return sub.q == r ? 0 : 1;
    }

    static final class Checker extends Thread {
        final Box b;
        final int r;
        int verdict = -1;

        Checker(Box b, int r) {
            this.b = b;
            this.r = r;
        }

        @Override
        public void run() {
            verdict = full(b, r) ? 0 : 1;
        }
    }

    public static void main(String[] args) throws InterruptedException {
        shape("P0", R21Fz10FreshVolPub1::w0, R21Fz10FreshVolPub1::r0);
        shape("P1", R21Fz10FreshVolPub1::w1, R21Fz10FreshVolPub1::r1);
        shape("P2", R21Fz10FreshVolPub1::w2, R21Fz10FreshVolPub1::r2);
        shape("P3", R21Fz10FreshVolPub1::w3, R21Fz10FreshVolPub1::r3);
        shape("P4", R21Fz10FreshVolPub1::w4, R21Fz10FreshVolPub1::r4);
        shape("P5", R21Fz10FreshVolPub1::w5, R21Fz10FreshVolPub1::r5);

        int bad = 0;
        int selfBad = 0;
        int rounds = 2000;
        for (int r = 1; r <= rounds; r++) {
            Box b = fill(r);
            if (!full(b, r)) {
                selfBad++;
            }
            Checker c = new Checker(b, r);
            c.start();
            c.join();
            if (c.verdict != 0) {
                bad++;
            }
        }
        System.out.println("P6 rounds=" + rounds + " bad=" + bad + " stuck=0 self=" + selfBad);
    }
}
