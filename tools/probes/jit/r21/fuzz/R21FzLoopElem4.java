// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: cell promotions entered through OSR
// (`CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE`,
// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY`, the static promotion).
//
// Risk targeted: the promotions say an ordinary OSR entry stub at a promoted
// header is WITHHELD (the value phi and the pre-header read are live and
// unnamed), and that a method-entry body whose loop is entered only through
// OSR keeps running its older body. A stub that is NOT withheld would seed
// the cell's phi with a stale or zero value. So every kernel here is ONE
// invocation of a long loop (millions of trips: the OSR door fires inside
// it), with the accumulator in an array element / a receiver field / a
// static, and a second, outer loop around it (a nested OSR header). Then a
// deopt trigger (a late subclass that overrides the receiver's step method,
// which the hot kernel inlined) and the same rows again.
//
// Run:
//   javac -d out R21FzLoopElem4.java
//   java -Xmx256m -cp out R21FzLoopElem4                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzLoopElem4
//   CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1 cratonvm -Xmx256m -cp out R21FzLoopElem4
//   CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1 cratonvm -Xmx256m -cp out R21FzLoopElem4
//   (both together), cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzLoopElem4 {
    static final int N = 3_000_000;
    static int sAcc;

    int fAcc;
    long fLong;

    int step(int i) {
        return (i * 31) ^ (i >>> 3);
    }

    static final class Over extends R21FzLoopElem4 {
        @Override
        int step(int i) {
            return i & 0xFF;
        }
    }

    static int osrElem(int n) {
        int[] a = new int[5];
        for (int i = 0; i < n; i++) {
            a[3] += (i * 31) ^ (i >>> 3);
        }
        return a[3] ^ a[0];
    }

    static long osrElemNested(int outer, int inner) {
        long[] a = new long[2];
        long t = 0;
        for (int o = 0; o < outer; o++) {
            for (int i = 0; i < inner; i++) {
                a[1] += i + o;
            }
            t += a[1] & 0xFFFF;
        }
        return t ^ a[1];
    }

    int osrField(int n) {
        for (int i = 0; i < n; i++) {
            fAcc += step(i);
        }
        return fAcc;
    }

    long osrFieldLong(int n) {
        for (int i = 0; i < n; i++) {
            fLong += (long) i * i;
        }
        return fLong;
    }

    static int osrStatic(int n) {
        for (int i = 0; i < n; i++) {
            sAcc += i ^ 0x5A5A;
        }
        return sAcc;
    }

    /** OSR entered with a local that is NOT this (local 0 overwritten in bytecode is not expressible; use a parameter). */
    static int osrOther(R21FzLoopElem4 o, int n) {
        for (int i = 0; i < n; i++) {
            o.fAcc += i & 7;
        }
        return o.fAcc;
    }

    static void rows(String tag, R21FzLoopElem4 obj) {
        System.out.println("row " + tag + " osrElem " + osrElem(N));
        System.out.println("row " + tag + " osrElemNested " + osrElemNested(300, 10_000));
        obj.fAcc = 0;
        System.out.println("row " + tag + " osrField " + obj.osrField(N) + " " + obj.fAcc);
        obj.fLong = 0;
        System.out.println("row " + tag + " osrFieldLong " + obj.osrFieldLong(N) + " " + obj.fLong);
        sAcc = 0;
        System.out.println("row " + tag + " osrStatic " + osrStatic(N) + " " + sAcc);
        obj.fAcc = 1;
        System.out.println("row " + tag + " osrOther " + osrOther(obj, N) + " " + obj.fAcc);
        try {
            osrOther(null, 1);
            System.out.println("row " + tag + " osrOther-null no throw");
        } catch (NullPointerException e) {
            System.out.println("row " + tag + " osrOther-null NPE");
        }
        System.out.println("row " + tag + " osrOther-null-zero " + zeroTripNull());
    }

    static int zeroTripNull() {
        try {
            return osrOther(null, 0);
        } catch (NullPointerException e) {
            return -1;
        }
    }

    public static void main(String[] args) {
        R21FzLoopElem4 base = new R21FzLoopElem4();
        rows("first", base);
        rows("second", base);
        R21FzLoopElem4 over = new Over();
        rows("over", over);
        rows("base-again", base);
    }
}
