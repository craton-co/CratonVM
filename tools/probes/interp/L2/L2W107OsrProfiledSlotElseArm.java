// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i5 wave 107, lane L2 (review of
// `CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK`; page
// docs/internal/fixed-bugs/interpreter-i107-L2-osr-profiled-slot-fact-answered-undominated-sites-FIXED-20261016.md).
//
// `b` is written once (`Sh b = make(..)`), and the inner loop's `b.v()` site
// sees one class (A) while the OSR compile is planned. The OSR build tests
// that class at the inner header and, before wave 107, then wrote the
// SLOT-WIDE fact "local b is exactly A" for the rest of the walk. The `else`
// arm of the outer body is laid out after the inner loop, so it was walked
// with that fact, but no test dominates it: once `make` returns a B for odd
// `o` the compiled `else` arm ran A.v() on a B (A's field read off a B).
//
// HotSpot runs B.v() (102) there. Every count is exact.
//
// Run (JIT on, both modes; also with CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK=0,
// same stdout):
//   javac -d out L2W107OsrProfiledSlotElseArm.java
//   cratonvm -cp out L2W107OsrProfiledSlotElseArm
//
// HOTSPOT_EXPECTED_BEGIN
// run1 5675000
// run2 5675000
// run3 5675000
// flipped 814000
// HOTSPOT_EXPECTED_END
public class L2W107OsrProfiledSlotElseArm {
    interface Sh {
        int v();
    }

    static final class A implements Sh {
        int x = 1;

        public int v() {
            return x;
        }
    }

    static final class B implements Sh {
        long pad = 0x5555_0000_0000L;
        int y = 2;

        public int v() {
            return y + 100;
        }
    }

    // Loaded before any compile, so no class-hierarchy answer for `Sh`
    // names A alone; called from here only, so `run`'s profiles stay A.
    static final Sh PRELOADED = new B();

    static Sh make(int o, int flipAt) {
        if (o >= flipAt && (o & 1) == 1) {
            return new B();
        }
        return new A();
    }

    static long run(int outer, int inner, int flipAt) {
        long s = 0;
        for (int o = 0; o < outer; o++) {
            Sh b = make(o, flipAt);
            if ((o & 1) == 0) {
                for (int j = 0; j < inner; j++) {
                    s += b.v();
                }
            } else {
                s += b.v() * 7L;
            }
        }
        return s;
    }

    public static void main(String[] args) {
        if (PRELOADED.v() != 102) {
            System.out.println("preload wrong");
        }
        // evens 20000 x 100 x 1; odds below 30000: 15000 x 7; odds from
        // 30000: 5000 x 102 x 7.
        System.out.println("run1 " + run(40000, 100, 30000));
        System.out.println("run2 " + run(40000, 100, 30000));
        System.out.println("run3 " + run(40000, 100, 30000));
        // evens 1000 x 100; odds 1000 x 714.
        System.out.println("flipped " + run(2000, 100, 0));
    }
}
