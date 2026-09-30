// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 6, lane L3: the virtual fast door
// (`execute_invokevirtual_fast_door`) now serves a receiver from the site's
// poly entries instead of declining every call whose receiver differs from
// the primary slot's ("site went polymorphic").
//
// Correctness: each loop runs ONE call site whose receiver class rotates every
// call, over targets that differ in kind -- an override, an inherited body, a
// `synchronized` override, a class that does not override `equals` (Object's
// identity `equals`, a native/intrinsic target) next to ones that do, String
// (native/intrinsic `length`) next to StringBuilder/StringBuffer (bytecode),
// and an interface default next to a class override. A poly entry served for
// the wrong receiver shows up as a wrong sum.
//
// Expected (HotSpot 25), stdout exactly:
//   m=99999600000
//   eq=200000
//   len=1700000
//   twice=12000000
//
// Performance (stderr only): `--nojit`, interleave with the previous build and
// take medians. `CRATONVM_DBG_FIELD_SITE=1` should report
// `door: ... virtual_poly hit=` in the hundreds of thousands, and the
// `virtual: receiver class differs from the cached one` decline row should
// shrink by the same amount.
public class PolyDoorProbe {
    static class Base {
        int m(int x) {
            return x + 1;
        }
    }

    static class Over extends Base {
        @Override
        int m(int x) {
            return x * 2;
        }
    }

    static class Inherit extends Base {}

    static class SyncOver extends Base {
        @Override
        synchronized int m(int x) {
            return x - 3;
        }
    }

    static final class EqAll {
        @Override
        public boolean equals(Object o) {
            return true;
        }

        @Override
        public int hashCode() {
            return 1;
        }
    }

    static final class EqNone {}

    static final class EqSync {
        @Override
        public synchronized boolean equals(Object o) {
            return o instanceof EqSync;
        }

        @Override
        public int hashCode() {
            return 2;
        }
    }

    interface Shape {
        int area();

        default int twice() {
            return 2 * area();
        }
    }

    static final class Sq implements Shape {
        public int area() {
            return 4;
        }
    }

    static final class Rect implements Shape {
        public int area() {
            return 6;
        }
    }

    static final class Circle implements Shape {
        public int area() {
            return 3;
        }

        @Override
        public int twice() {
            return 100;
        }
    }

    static int callM(Base b, int x) {
        return b.m(x);
    }

    static boolean callEq(Object a, Object b) {
        return a.equals(b);
    }

    static int lenOf(CharSequence cs) {
        return cs.length();
    }

    static int twiceOf(Shape s) {
        return s.twice();
    }

    static long runM(Base[] bases) {
        long sum = 0;
        for (int i = 0; i < 400_000; i++) {
            sum += callM(bases[i & 3], i);
        }
        return sum;
    }

    public static void main(String[] args) {
        Base[] bases = {new Base(), new Over(), new Inherit(), new SyncOver()};
        long t0 = System.nanoTime();
        long sumM = runM(bases);
        long t1 = System.nanoTime();
        System.out.println("m=" + sumM);

        EqAll all = new EqAll();
        EqNone none = new EqNone();
        EqSync sync = new EqSync();
        Object[] recv = {all, none, sync, none};
        Object[] arg = {none, none, none, all};
        int eq = 0;
        for (int i = 0; i < 400_000; i++) {
            if (callEq(recv[i & 3], arg[i & 3])) {
                eq++;
            }
        }
        System.out.println("eq=" + eq);

        CharSequence[] cs = {
            "abc", new StringBuilder("hello"), "xy", new StringBuffer("0123456")
        };
        long len = 0;
        for (int i = 0; i < 400_000; i++) {
            len += lenOf(cs[i & 3]);
        }
        System.out.println("len=" + len);

        Shape[] shapes = {new Sq(), new Rect(), new Circle()};
        long twice = 0;
        for (int i = 0; i < 300_000; i++) {
            twice += twiceOf(shapes[i % 3]);
        }
        System.out.println("twice=" + twice);

        // A second timed pass after warm-up, for the A/B.
        long t2 = System.nanoTime();
        long again = runM(bases);
        long t3 = System.nanoTime();
        if (again != sumM) {
            System.out.println("MISMATCH m second pass=" + again);
        }
        System.err.println(
                "[PolyDoorProbe] m-loop first="
                        + (t1 - t0) / 400_000
                        + " ns/call, warm="
                        + (t3 - t2) / 400_000
                        + " ns/call");
    }
}
