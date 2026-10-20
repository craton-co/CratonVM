// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s2/rf: arrays past 1 << 28 elements (legal up to Integer.MAX_VALUE,
// refused by ZGC's alloc_size element cap) on the two consumers zs s1/dg left
// open (docs/internal/gc/zgc-zs1dg-over-long-array-residual-consumers-20261014.md):
//
//   dead  -- row 1: drop one byte[(1<<28)+4096] per iteration on a heap that
//            holds three. The per-object sweep (young cycles, the fallback arm
//            when the complement sweep refuses, e.g. CRATONVM_ZGC_SWEEP_DEAD_RUNS=0)
//            retained every such dead array, so a later allocation threw
//            OutOfMemoryError. HotSpot ZGC serves every iteration.
//   young -- row 3: a long-lived Object[(1<<28)+4096] (promoted by three
//            System.gc() calls) receives fresh objects at both ends and the
//            middle, then small garbage drives young collections. Under
//            CRATONVM_ZGC_GENERATIONAL=1 CRATONVM_ZGC_GEN_ELEMENT_CARDS=1 the
//            element card of such an array was DROPPED by the young cycle
//            without rooting its young target, so the stored object was freed
//            while the array still named it.
//
// Every line on stdout is deterministic (no timings):
//   Zs2RfOverLong mode=dead iterations=12 served=12 oome=false
//   Zs2RfOverLong mode=young rounds=8 ok=8 bad=0
// `young [rounds] [garbage-objects-per-round]` (default 8, 3000000).
public class Zs2RfOverLong {
    static final int OVER = (1 << 28) + 4096;

    static final class Node {
        final int id;
        long pad0, pad1;

        Node(int id) {
            this.id = id;
        }
    }

    static byte[] holder;
    static Object[] arr;
    static Object sinkHole;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "dead";
        if (mode.equals("young")) {
            young(args.length > 1 ? Integer.parseInt(args[1]) : 8,
                    args.length > 2 ? Integer.parseInt(args[2]) : 3_000_000);
        } else {
            dead(args.length > 1 ? Integer.parseInt(args[1]) : 12);
        }
    }

    static void dead(int iterations) {
        int served = 0;
        boolean oome = false;
        for (int i = 0; i < iterations; i++) {
            try {
                holder = new byte[OVER];
                holder[OVER - 1] = (byte) i;
                served++;
                holder = null;
            } catch (OutOfMemoryError e) {
                holder = null;
                oome = true;
                break;
            }
        }
        System.out.println("Zs2RfOverLong mode=dead iterations=" + iterations
                + " served=" + served + " oome=" + oome);
    }

    static void young(int rounds, int garbage) {
        arr = new Object[OVER];
        // Old by the time the stores below run (generational mode promotes a
        // survivor of three collections; harmless elsewhere).
        System.gc();
        System.gc();
        System.gc();
        int[] slots = {0, 1, (1 << 28) - 1, 1 << 28, OVER / 2, OVER - 2, OVER - 1};
        int ok = 0;
        int bad = 0;
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < slots.length; k++) {
                arr[slots[k]] = new Node(r * 100 + k);
            }
            // `garbage` small objects (default 3M, ~96 MB): young collections,
            // no System.gc().
            for (int g = 0; g < garbage; g++) {
                sinkHole = new Node(-g);
            }
            boolean good = true;
            for (int k = 0; k < slots.length; k++) {
                Object o = arr[slots[k]];
                if (!(o instanceof Node) || ((Node) o).id != r * 100 + k) {
                    good = false;
                }
            }
            if (good) {
                ok++;
            } else {
                bad++;
            }
        }
        System.out.println("Zs2RfOverLong mode=young rounds=" + rounds + " ok=" + ok + " bad=" + bad);
    }
}
