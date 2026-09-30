// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 28, lane L7: constructors answered without a
// frame by the non-virtual door (vm/src/runtime/interpreter/invoke_fast.rs):
//
//   * TRIVIAL_CTOR_ELISION (wave 25) -- javac's default constructor of a
//     direct Object subclass. Wave 28 found it had never engaged on the real
//     build (its shape test read the padded body's length) and fixed that;
//   * FIELD_CTOR_ELISION (wave 28) -- a constructor of a direct Object
//     subclass that calls Object.<init> and then only stores parameters or
//     small constants into its own fields (`Point(int a, int b) { this.a = a;
//     this.b = b; }`, `int x = 7;`).
//
// Each row runs its constructor ITERS times in one loop (the first calls take
// the frame and fill the field sites; the rest are answered without one) and
// prints a checksum of every field it stored, so a store that went to the
// wrong field, the wrong width, the wrong object, or not at all changes the
// line. Rows cover: int / boolean / byte / char / short / float / reference
// fields from parameters, constants (iconst_m1, bipush, sipush, fconst,
// aconst_null), final fields, a subclass calling the constructor through
// super(...) (another receiver class: framed), a long parameter (declined:
// local numbering), reference fields surviving collections, and
// constructors called at the stack limit.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W28FieldCtorProbe
//      and again without --nojit and with --compatible.
// Positive control (the rule must engage): CRATONVM_DBG_FIELD_SITE=1 prints
//   [invoke-door] trivial constructors: elided=N ...        (N > 0)
//   [invoke-door] field-store constructors: elided=M ...    (M > 0)
// on stderr at exit under --nojit.
//
// HotSpot 25 (25.0.3) prints, with and without -Xint:
//   point 49999500000 24999750000
//   init 700000 -1 -100 30000
//   narrow true -1 65535 -2 1.5 2.0
//   refs 100000 0 100000
//   finals 4999950000 99999
//   default 100000
//   sub 4999950000 0 100000
//   long 700000 4999950000
//   gc 200000 200000
//   soe caught trivial=true field=true
public class L7W28FieldCtorProbe {
    static final int ITERS = 100_000;

    static final class Point {
        int a;
        int b;

        Point(int a, int b) {
            this.a = a;
            this.b = b;
        }
    }

    static final class Init {
        int seven = 7;
        int minusOne = -1;
        int minus100 = -100;
        int big = 30000;
    }

    static final class Narrow {
        boolean z;
        byte b;
        char c;
        short s;
        float f;
        float two;

        Narrow(boolean z, byte b, char c, short s, float f) {
            this.z = z;
            this.b = b;
            this.c = c;
            this.s = s;
            // Four stores only: `two` is set below by a separate constructor.
        }

        Narrow(float f) {
            this.f = f;
            this.two = 2.0f;
        }
    }

    static final class Refs {
        Object o;
        String s;
        Object n;

        Refs(Object o, String s) {
            this.o = o;
            this.s = s;
            this.n = null;
        }
    }

    static final class Finals {
        final int x;
        final long unused = 0L; // a long constant: the whole constructor is framed

        Finals(int x) {
            this.x = x;
        }
    }

    static final class FinalInt {
        final int x;
        final Object self;

        FinalInt(int x, Object self) {
            this.x = x;
            this.self = self;
        }
    }

    static final class Plain {
        int tag;
    }

    static class Base {
        int a;
        int b;

        Base(int a, int b) {
            this.a = a;
            this.b = b;
        }
    }

    static final class Sub extends Base {
        Sub(int a) {
            super(a, 0);
        }
    }

    static final class WithLong {
        int b;
        int c;

        WithLong(long a, int b) {
            this.b = b;
            this.c = (int) a;
        }
    }

    static final class Cell {
        Object ref;
        int n;

        Cell(Object ref, int n) {
            this.ref = ref;
            this.n = n;
        }
    }

    static int depthTrivial;
    static int depthField;

    static void recurseTrivial(int d) {
        depthTrivial = d;
        new Plain();
        recurseTrivial(d + 1);
    }

    static void recurseField(int d) {
        depthField = d;
        new Point(d, d);
        recurseField(d + 1);
    }

    public static void main(String[] args) {
        long sa = 0, sb = 0;
        for (int i = 0; i < ITERS; i++) {
            Point p = new Point(i * 10, i * 5);
            sa += p.a;
            sb += p.b;
        }
        System.out.println("point " + sa + " " + sb);

        long seven = 0;
        Init last = null;
        for (int i = 0; i < ITERS; i++) {
            last = new Init();
            seven += last.seven;
        }
        System.out.println("init " + seven + " " + last.minusOne + " " + last.minus100 + " " + last.big);

        Narrow n = null;
        Narrow nf = null;
        for (int i = 0; i < ITERS; i++) {
            n = new Narrow((i & 1) == 1, (byte) -1, (char) 0xFFFF, (short) -2, 9.0f);
            nf = new Narrow(1.5f);
        }
        System.out.println("narrow " + n.z + " " + n.b + " " + (int) n.c + " " + n.s + " " + nf.f + " " + nf.two);

        Object token = new Object();
        long same = 0, nulls = 0, strs = 0;
        for (int i = 0; i < ITERS; i++) {
            Refs r = new Refs(token, (i & 1) == 0 ? "even" : "odd");
            if (r.o == token) same++;
            if (r.n != null) nulls++;
            if (r.s.length() >= 3) strs++;
        }
        System.out.println("refs " + same + " " + nulls + " " + strs);

        long fx = 0, fs = 0;
        for (int i = 0; i < ITERS; i++) {
            fx += new Finals(i).x;
            FinalInt fi = new FinalInt(i, token);
            if (fi.self == token) fs = fi.x;
        }
        System.out.println("finals " + fx + " " + fs);

        long tags = 0;
        for (int i = 0; i < ITERS; i++) {
            Plain p = new Plain();
            tags += p.tag + 1;
        }
        System.out.println("default " + tags);

        long subA = 0, subB = 0, subN = 0;
        for (int i = 0; i < ITERS; i++) {
            Base b = (i & 1) == 0 ? new Sub(i) : new Base(i, 0);
            subA += b.a;
            subB += b.b;
            subN++;
        }
        System.out.println("sub " + subA + " " + subB + " " + subN);

        long lb = 0, lc = 0;
        for (int i = 0; i < ITERS; i++) {
            WithLong w = new WithLong(((long) i << 32) | 7L, i);
            lb += w.b;
            lc += w.c;
        }
        System.out.println("long " + lc + " " + lb);

        Cell[] keep = new Cell[1000];
        long alive = 0, sum = 0;
        for (int round = 0; round < 2; round++) {
            for (int i = 0; i < ITERS; i++) {
                keep[i % keep.length] = new Cell(new int[] {i}, i);
                if (i % 20_000 == 0) {
                    System.gc();
                }
            }
            for (int i = 0; i < ITERS; i++) {
                Cell c = keep[i % keep.length];
                if (c.ref instanceof int[] arr && arr[0] == c.n) {
                    alive++;
                }
                sum++;
            }
        }
        System.out.println("gc " + alive + " " + sum);

        boolean trivialSoe = false, fieldSoe = false;
        try {
            recurseTrivial(0);
        } catch (StackOverflowError e) {
            trivialSoe = true;
        }
        try {
            recurseField(0);
        } catch (StackOverflowError e) {
            fieldSoe = true;
        }
        System.out.println("soe caught trivial=" + trivialSoe + " field=" + fieldSoe);
    }
}
