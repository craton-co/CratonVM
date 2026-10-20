// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 28, lane L7: constructors answered without a
// frame by the non-virtual door (vm/src/runtime/interpreter/invoke_fast.rs).
//
//   commit "the frameless trivial constructor engages on the real build":
//     TRIVIAL_CTOR_ELISION (wave 25) read the padded body's length and never
//     engaged; now it does.
//   commit "field-store constructors answered without a frame":
//     FIELD_CTOR_ELISION, a direct Object subclass's constructor that only
//     stores parameters or small constants into its own fields.
//
// Rows (ns per `new`, stderr), --nojit:
//
//   new-object       CONTROL: `new Object()` (frameless since wave 20). Flat.
//   new-default      `new Plain()`, javac's default constructor. EXPECTED
//                    down toward new-object after the first commit.
//   new-field-init   `new WithInit()` (`int x = 7;`). EXPECTED down after the
//                    second commit.
//   new-point        `new Point(i, j)` (two parameter stores, the
//                    InvokeDoorCostBench `ctor` shape). EXPECTED down after
//                    the second commit.
//   new-sub-default  `new SubPlain()` (its nested `Plain.<init>` is trivial).
//                    EXPECTED down a little after the first commit.
//   new-sub-field    CONTROL: `Child(int v) { super(); this.v = v; }` under a
//                    non-Object parent: the field-store screen matches its
//                    bytes, the memoized verdict refuses it, the frame is
//                    pushed. Flat within a few ns (the screen and one memo
//                    probe are its whole cost).
//   new-long-param   CONTROL: a field-store constructor with a `long`
//                    parameter (declined: local numbering). Flat within a few
//                    ns.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W28FramelessCtorBench
//      on fat-LTO builds, interleaved: the commit before the first against
//      the first, and the first against the second; pinned, 5 rounds,
//      medians. With CRATONVM_DBG_FIELD_SITE=1 the exit census must show
//      `trivial constructors: elided=N` (N >= 2_000_000: new-default and
//      new-sub-default's nested `Plain.<init>`) and
//      `field-store constructors: elided=M` (M >= 2_000_000: new-field-init
//      and new-point, less the first framed call of each, which fills the
//      field sites; new-long-param and new-sub-field are refused by the
//      memoized per-method verdict before the census, since L7b).
//
// stdout is deterministic, identical on HotSpot 25 (25.0.3), with and without
// -Xint. For scale, HotSpot 25 -Xint on the i7-8550U box: new-object 47,
// new-default 94, new-field-init 79, new-point 93, new-sub-default 106,
// new-sub-field 101, new-long-param 117 ns/new:
//   new-object 1500000
//   new-default 500000
//   new-field-init 7500000
//   new-point 749999000000
//   new-sub-default 500000
//   new-sub-field 499999500000
//   new-long-param 499999500000
public class L7W28FramelessCtorBench {
    static final int WARMUP = 20_000;
    static final int ITERS = 1_000_000;

    static class Plain {
        int tag;
    }

    static final class SubPlain extends Plain {
    }

    static final class WithInit {
        int x = 7;
    }

    static final class Point {
        int a;
        int b;

        Point(int a, int b) {
            this.a = a;
            this.b = b;
        }
    }

    static class Parent {
        int p;
    }

    static final class Child extends Parent {
        int v;

        Child(int v) {
            super();
            this.v = v;
        }
    }

    static final class LongParam {
        int v;

        LongParam(long ignored, int v) {
            this.v = v;
        }
    }

    interface Row {
        long run(int iters);
    }

    static void row(String name, Row r) {
        r.run(WARMUP);
        long t0 = System.nanoTime();
        long sum = r.run(ITERS);
        long ns = System.nanoTime() - t0;
        System.out.println(name + " " + sum);
        System.err.printf(java.util.Locale.ROOT, "%-16s %8.1f ns/new%n", name, (double) ns / ITERS);
    }

    static long newObject(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Object o = new Object();
            s += (o != null ? 1 : 0) + (i & 1);
        }
        return s;
    }

    static long newDefault(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += new Plain().tag + (i & 1);
        }
        return s;
    }

    static long newFieldInit(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += new WithInit().x + (i & 1);
        }
        return s;
    }

    static long newPoint(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Point p = new Point(i, i >>> 1);
            s += p.a + p.b;
        }
        return s;
    }

    static long newSubDefault(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += new SubPlain().tag + (i & 1);
        }
        return s;
    }

    static long newSubField(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += new Child(i).v;
        }
        return s;
    }

    static long newLongParam(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += new LongParam(i, i).v;
        }
        return s;
    }

    public static void main(String[] args) {
        row("new-object", L7W28FramelessCtorBench::newObject);
        row("new-default", L7W28FramelessCtorBench::newDefault);
        row("new-field-init", L7W28FramelessCtorBench::newFieldInit);
        row("new-point", L7W28FramelessCtorBench::newPoint);
        row("new-sub-default", L7W28FramelessCtorBench::newSubDefault);
        row("new-sub-field", L7W28FramelessCtorBench::newSubField);
        row("new-long-param", L7W28FramelessCtorBench::newLongParam);
    }
}
