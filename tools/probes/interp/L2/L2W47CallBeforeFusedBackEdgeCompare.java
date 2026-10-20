// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L2: a loop whose bottom test compares a
// call's result against an int constant -- `do { ... } while (next() < K)`,
// the call's successor being the constant the single-pass tier fuses with the
// backward `if_icmplt` (`x64/arith.rs::try_const_compare_peephole`). Before
// wave 47 the call's post-call exit site took the constant's bci, where the
// fused conditional back edge files its own exit map, so the back-edge poll
// kept no exit; now the site is refused there
// (`x64/op_invoke.rs::FUSED_BACK_EDGE_CONSTANT_POST_CALL_SITES_REFUSED`) and
// the back edge leaves through its map. Unit test (the positive control):
// `x64::tests::a_call_before_a_fused_backward_compare_leaves_the_bci_to_the_back_edge`.
//
// This probe is the correctness guard of the change: three loop shapes of
// that form (an `iconst`, a `bipush` and a `sipush` constant), each hot
// enough to be compiled at its method entry and through OSR, must compute
// what the interpreter computes.
//
// HotSpot 25 prints (the same with -Xint):
//     iconst=5000000 calls=5000000
//     bipush=5000000 calls=5000000
//     sipush=5000000 calls=5000000
//     osr=40000000
public class L2W47CallBeforeFusedBackEdgeCompare {
    static int calls;
    static int cursor;

    static int tick5() {
        calls++;
        return (cursor++ % 5 == 4) ? 5 : 1;
    }

    static int tick100() {
        calls++;
        return (cursor++ % 100 == 99) ? 100 : 7;
    }

    static int tick1000() {
        calls++;
        return (cursor++ % 1000 == 999) ? 1000 : 99;
    }

    /** `iconst_5; if_icmplt`: runs until tick5() answers 5. */
    static int iconstLoop() {
        int n = 0;
        do {
            n++;
        } while (tick5() < 5);
        return n;
    }

    /** `bipush 100; if_icmplt`. */
    static int bipushLoop() {
        int n = 0;
        do {
            n++;
        } while (tick100() < 100);
        return n;
    }

    /** `sipush 1000; if_icmplt`. */
    static int sipushLoop() {
        int n = 0;
        do {
            n++;
        } while (tick1000() < 1000);
        return n;
    }

    /** One long activation: the OSR shape. */
    static long osrLoop() {
        long sum = 0;
        int i = 0;
        cursor = 0;
        do {
            sum += 2;
            i++;
        } while (tick1000() < 1000 || i < 20_000_000);
        return sum;
    }

    public static void main(String[] args) {
        String[] names = {"iconst", "bipush", "sipush"};
        for (int shape = 0; shape < 3; shape++) {
            calls = 0;
            cursor = 0;
            long total = 0;
            int runs = shape == 0 ? 1_000_000 : shape == 1 ? 50_000 : 5_000;
            for (int r = 0; r < runs; r++) {
                total += shape == 0 ? iconstLoop() : shape == 1 ? bipushLoop() : sipushLoop();
            }
            System.out.println(names[shape] + "=" + total + " calls=" + calls);
        }
        System.out.println("osr=" + osrLoop());
    }
}
