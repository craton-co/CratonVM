// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: LOOPS21-3, the fresh-object promotion's
// static feed (`CRATONVM_JIT_IR_FRESH_PROMOTE_STATIC_FEED`, default ON, `=0`
// restores the old `direct_static_fields` list) and the static promotion
// (default ON) it now runs beside.
//
// Risk targeted: with the feed, the fresh promotion admits a loop that READS a
// direct static; the static's value must still be read per trip when the
// same loop WRITES it (or a static that aliases it by inheritance: `Sub.X`
// names `Base.X`), and a static of a class that was not initialized when the
// method was compiled must stay helper-routed (its `<clinit>` runs inside the
// loop on first touch, with a visible side effect). Rows:
//   * fresh accumulator reading a non-final static the loop never writes;
//   * fresh accumulator reading a static the loop also writes (through the
//     subclass name);
//   * fresh accumulator reading a static of a lazily initialized holder whose
//     <clinit> prints and changes another static;
//   * the static promotion of a counter interleaved with a fresh accumulator;
//   * a long static and a long fresh field (64-bit write-back compare).
// The static changes between rows (the compiled body must not have folded
// it), rows hot and after a deopt (a late subclass of the accumulator type).
//
// Run:
//   javac -d out R21FzLoopStatic1.java
//   java -Xmx256m -cp out R21FzLoopStatic1                                # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzLoopStatic1
//   CRATONVM_JIT_IR_FRESH_PROMOTE_STATIC_FEED=0 cratonvm -Xmx256m -cp out R21FzLoopStatic1
//   CRATONVM_JIT_IR_LOOP_STATIC_HOIST=0 CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE=0 CRATONVM_JIT_IR_LICM_STATIC_READ_NOT_BARRIER=0 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzLoopStatic1 {
    static final int WARM = 60_000;

    static class Base {
        static int X = 3;
        static long LX = 5;
    }

    static class Sub extends Base {
    }

    static int scale = 7;
    static int counter;
    static String log = "";

    static class Acc {
        int v;
        int w;
        long l;
    }

    static class AccSub extends Acc {
        int extra;
    }

    static final class Lazy {
        static int K;

        static {
            K = 11;
            log += "[Lazy.<clinit>]";
            scale += 1;
        }
    }

    static Acc readOnly(int n, int bias) {
        Acc a = new Acc();
        for (int i = 0; i < n; i++) {
            a.v += scale * i;
            a.w += bias;
        }
        return a;
    }

    static Acc readWritten(int n) {
        Acc a = new Acc();
        for (int i = 0; i < n; i++) {
            a.v += Base.X;
            Sub.X += 1;
        }
        return a;
    }

    static boolean useLazy;

    static Acc lazyRead(int n) {
        Acc a = new Acc();
        for (int i = 0; i < n; i++) {
            a.v += i;
            if (useLazy && i == 2) {
                a.w += Lazy.K;
            }
            a.w += scale;
        }
        return a;
    }

    static Acc counterAndFresh(int n) {
        Acc a = new Acc();
        for (int i = 0; i < n; i++) {
            counter += 1;
            a.v += counter;
        }
        return a;
    }

    static Acc longs(int n) {
        Acc a = new Acc();
        for (int i = 0; i < n; i++) {
            a.l += Base.LX << 31;
            Base.LX += 1L << 33;
        }
        return a;
    }

    static String show(Acc a) {
        return a.v + "," + a.w + "," + a.l;
    }

    static void rows(String tag) {
        System.out.println("row " + tag + " readOnly " + show(readOnly(13, 2)) + " scale=" + scale);
        scale += 5;
        System.out.println("row " + tag + " readOnly2 " + show(readOnly(13, -1)) + " scale=" + scale);
        System.out.println("row " + tag + " readWritten " + show(readWritten(9)) + " X=" + Base.X);
        System.out.println("row " + tag + " lazy " + show(lazyRead(5)) + " log=" + log + " scale=" + scale);
        System.out.println("row " + tag + " counter " + show(counterAndFresh(17)) + " counter=" + counter);
        System.out.println("row " + tag + " longs " + show(longs(6)) + " LX=" + Base.LX);
    }

    public static void main(String[] args) {
        rows("cold");
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            int n = 3 + (r & 7);
            h = h * 31 + readOnly(n, r).v;
            h = h * 31 + readWritten(n).v;
            h = h * 31 + lazyRead(n).w;
            h = h * 31 + counterAndFresh(n).v;
            h = h * 31 + longs(n).l;
            if ((r & 4095) == 0) {
                scale++;
            }
        }
        System.out.println("warm hash " + h + " scale=" + scale + " X=" + Base.X + " counter=" + counter);
        rows("hot");
        useLazy = true;
        rows("lazy-on");
        AccSub late = new AccSub();
        late.extra = 1;
        System.out.println("late " + late.extra + " " + show(late));
        rows("after");
    }
}
