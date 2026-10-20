// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 94, lane L7: the special door's frameless
// constructor call over a SUPER CHAIN (the "recursive rule over a super
// chain" of i24-L4-proposal-frameless-trivial-constructor-in-the-special-door).
// Since waves 25/28 `new C()` of a direct `Object` subclass with javac's
// default constructor, or a constructor that only stores fields, pushes no
// frame. A class one level further down (`Sub extends Base`, both default)
// still pushes one frame per level above `Object`, because its first call
// names `Base.<init>`, not `Object.<init>`.
// `CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN` (default off) follows the chain
// of default constructors up to `Object`.
//
// Rows (ROUNDS = 400 rounds; one op = BATCH = 1000 allocations):
//   new-depth1-default  `new D1()`, D1 extends Object, default ctor   (control: elided already)
//   new-depth2-default  `new D2()`, D2 extends D1, default ctors      (expected down)
//   new-depth3-default  `new D3()`, D3 extends D2 extends D1          (expected down most)
//   new-depth2-field    `new F2(i)`, F2 extends D1, `this.x = x`      (expected down: field-store
//                       constructor over a one-link chain)
//   new-object          `new Object()`                                (floor)
//
// Run (--nojit: pure interpretation; switch off and on, interleaved, medians):
//   javac -d out L7W94CtorSuperChainBench.java
//   java -Xint -cp out L7W94CtorSuperChainBench                                 (HotSpot reference)
//   cratonvm --jdk-only --nojit -cp out L7W94CtorSuperChainBench
//   CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN=1 cratonvm --jdk-only --nojit -cp out L7W94CtorSuperChainBench
// Census: CRATONVM_DBG_FIELD_SITE=1 prints "[invoke-door] trivial constructors",
// "field-store constructors" and, with the switch on, "constructors elided
// over a super chain: N".
//
// Output: "checksum <row> <n>" on stdout (deterministic); the timing line
// "row <row> min <ns> med <ns> ns/op" on stderr.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// checksum new-depth1-default 400000
// checksum new-depth2-default 400000
// checksum new-depth3-default 400000
// checksum new-depth2-field 199800000
// checksum new-object 400000
// HOTSPOT_EXPECTED_END

import java.util.Arrays;

public class L7W94CtorSuperChainBench {
    static final int ROUNDS = 400;
    static final int WARM = 40;
    static final int BATCH = 1000;

    static class D1 {}

    static class D2 extends D1 {}

    static class D3 extends D2 {}

    static class F2 extends D1 {
        int x;

        F2(int x) {
            this.x = x;
        }
    }

    interface Row {
        long op();
    }

    static long depth1() {
        long s = 0;
        for (int i = 0; i < BATCH; i++) {
            D1 o = new D1();
            s += o != null ? 1 : 0;
        }
        return s;
    }

    static long depth2() {
        long s = 0;
        for (int i = 0; i < BATCH; i++) {
            D2 o = new D2();
            s += o != null ? 1 : 0;
        }
        return s;
    }

    static long depth3() {
        long s = 0;
        for (int i = 0; i < BATCH; i++) {
            D3 o = new D3();
            s += o != null ? 1 : 0;
        }
        return s;
    }

    // Sum of i over 0..999 = 499500 per op.
    static long depth2Field() {
        long s = 0;
        for (int i = 0; i < BATCH; i++) {
            F2 o = new F2(i);
            s += o.x;
        }
        return s;
    }

    static long object() {
        long s = 0;
        for (int i = 0; i < BATCH; i++) {
            Object o = new Object();
            s += o != null ? 1 : 0;
        }
        return s;
    }

    static void time(String row, Row r) {
        for (int i = 0; i < WARM; i++) {
            r.op();
        }
        long[] ns = new long[ROUNDS];
        long checksum = 0;
        for (int i = 0; i < ROUNDS; i++) {
            long t0 = System.nanoTime();
            checksum += r.op();
            ns[i] = System.nanoTime() - t0;
        }
        Arrays.sort(ns);
        System.out.println("checksum " + row + " " + checksum);
        System.err.println("row " + row + " min " + ns[0] + " med " + ns[ROUNDS / 2] + " ns/op");
    }

    public static void main(String[] args) {
        time("new-depth1-default", L7W94CtorSuperChainBench::depth1);
        time("new-depth2-default", L7W94CtorSuperChainBench::depth2);
        time("new-depth3-default", L7W94CtorSuperChainBench::depth3);
        time("new-depth2-field", L7W94CtorSuperChainBench::depth2Field);
        time("new-object", L7W94CtorSuperChainBench::object);
    }
}
