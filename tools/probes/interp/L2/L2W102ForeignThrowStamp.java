// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L2 -- page
// i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range
// (`CRATONVM_JIT_THROW_STAMP_OWNER`, default ON).
//
// A compiled callee's throw-site stamp names ITS bci. When it reaches the
// caller's drain unreplaced, the caller used to range-check its own exception
// table against that foreign pc, and here the callee's `athrow` bci lands
// inside the caller's `try` (which starts at bci 0 and catches the very same
// class), so a stamp taken as the caller's own would run the caller's handler
// for an exception thrown by a call OUTSIDE the `try`. HotSpot (JVMS 2.10):
// the exception escapes the caller.
//
// Each shape is warmed past every JIT threshold with a throw every 1000th
// call, so the throws are taken interpreted, from single-pass code and from
// optimizing code alike. Run by default, with `--nojit`, with
// `CRATONVM_BG_COMPILE=0`, and with `CRATONVM_JIT_THROW_STAMP_OWNER=0` (the
// old behaviour; it should still print HotSpot's lines, since every exit is
// believed to stamp its own bci -- a difference there names an exit that does
// not). For the census: `CRATONVM_DBG_JITC=1 ... 2>&1 | grep 'foreign throw
// stamp refused'` (any line names a drain that met a foreign stamp).
//
// Expected (HotSpot 25):
//   athrow caught=0 escaped=60 sum=<x>
//   implicit caught=0 escaped=60 sum=<y>
//   nested caught=0 escaped=60 sum=<z>
//   inside caught=60 escaped=0 sum=<w>
public class L2W102ForeignThrowStamp {
    static final int ITER = 60_000;

    // The callee's `athrow` sits at a small bci, inside the caller's try.
    static int thrower(int k) {
        if (k % 1000 == 999) {
            throw new IllegalStateException("t");
        }
        return k & 7;
    }

    // An implicit exception at a small bci (an AIOOBE from a compiled bounds
    // check) of the same class the caller catches.
    static final int[] SMALL = new int[4];

    static int implicitThrower(int k) {
        return SMALL[k % 1000 == 999 ? 4 : k & 3];
    }

    static int safe(int k) {
        return k & 3;
    }

    static int athrowCaller(int k) {
        int r = 0;
        try {
            r = safe(k);
            r += safe(k + 1);
        } catch (IllegalStateException e) {
            r = -1000;
        }
        r += thrower(k);
        return r;
    }

    static int implicitCaller(int k) {
        int r = 0;
        try {
            r = safe(k);
            r += safe(k + 1);
        } catch (ArrayIndexOutOfBoundsException e) {
            r = -1000;
        }
        r += implicitThrower(k);
        return r;
    }

    // Two levels: the stamp of `thrower` passes through `middle`.
    static int middle(int k) {
        return thrower(k) + 1;
    }

    static int nestedCaller(int k) {
        int r = 0;
        try {
            r = safe(k);
            r += safe(k + 1);
        } catch (IllegalStateException e) {
            r = -1000;
        }
        r += middle(k);
        return r;
    }

    // Control: the throwing call IS inside the try.
    static int insideCaller(int k) {
        int r = 0;
        try {
            r = safe(k);
            r += thrower(k);
        } catch (IllegalStateException e) {
            r = -1000;
        }
        return r;
    }

    interface Shape {
        int call(int k);
    }

    static void run(String name, Shape s) {
        int caught = 0;
        int escaped = 0;
        long sum = 0;
        for (int k = 0; k < ITER; k++) {
            try {
                int r = s.call(k);
                if (r < 0) {
                    caught++;
                }
                sum += r;
            } catch (RuntimeException e) {
                escaped++;
            }
        }
        System.out.println(name + " caught=" + caught + " escaped=" + escaped + " sum=" + sum);
    }

    public static void main(String[] args) {
        run("athrow", L2W102ForeignThrowStamp::athrowCaller);
        run("implicit", L2W102ForeignThrowStamp::implicitCaller);
        run("nested", L2W102ForeignThrowStamp::nestedCaller);
        run("inside", L2W102ForeignThrowStamp::insideCaller);
    }
}
