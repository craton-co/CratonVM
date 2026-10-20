// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s1/dg: a LIVE array past 1 << 28 elements across ZGC slides.
//
// `NativeGrowthReclaimProbe` on a default heap made ZGC's rewrite pass log
//   zgc relocate: rewrite target is not walkable ... kind=Array class_id=0
//   num_slots=273366446 array_length=273366446 sizable=false
// That header is a REAL array (primitive arrays carry class id 0, and
// `alloc_array` mirrors the length into num_slots): `alloc_size` caps array
// length at 1 << 28 (268,435,456) as a corruption screen while the allocator
// admits up to Integer.MAX_VALUE, so the slide skipped it as unwalkable. For a
// primitive array the skip loses nothing; for a reference array the slot walk
// also omitted EVERY element, so a slide left them naming moved objects' old
// addresses (dangling). This probe builds both shapes deterministically.
//
// Modes:
//   prim  -- a live byte[(1<<28)+4096] beside a churned linked list (-Xmx1g)
//   refs  -- a live Object[(1<<28)+4096] whose elements name small objects
//            that later slides move (-Xmx4g; HotSpot: 1 GB with compressed
//            oops, 2 GB without)
// Rows (both `--jdk-only` and `--compatible`, and `--nojit`):
//   cratonvm -Xmx1g -cp tools/probes Zs1DgUnwalkableSurvivor prim 8
//   cratonvm -Xmx4g -cp tools/probes Zs1DgUnwalkableSurvivor refs 8
//   HotSpot twin: java -XX:+UseZGC -Xmx1g / -Xmx4g, same arguments.
// Expected (every arm and HotSpot):
//   Zs1DgUnwalkableSurvivor mode=prim rounds=8 ok=8 bad=0 big=7
//   Zs1DgUnwalkableSurvivor mode=refs rounds=8 ok=8 bad=0 big=7
// plus, on stderr of the cratonvm runs, NO "rewrite target is not walkable"
// and no "elements are OMITTED" line. Kill switch
// CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS=0 is the positive control: prim prints
// the same stdout but the guard lines come back; refs is expected to print
// bad>0 (or crash) and the "OMITTED" line.
public class Zs1DgUnwalkableSurvivor {
    static final int OVER = (1 << 28) + 4096;
    static final int NODES = 20000;

    static final class Node {
        final int id;
        Node next;
        long pad0, pad1;

        Node(int id) {
            this.id = id;
        }
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "prim";
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        int ok = 0;
        int bad = 0;
        int big;
        if (mode.equals("refs")) {
            Object[] arr = new Object[OVER];
            int[] slots = {0, 1, (1 << 28) - 1, 1 << 28, OVER / 2, OVER - 2, OVER - 1};
            for (int r = 0; r < rounds; r++) {
                churn(r);
                for (int k = 0; k < slots.length; k++) {
                    // garbage between the targets, so the targets move
                    for (int g = 0; g < 2000; g++) {
                        sinkHole = new Node(-g);
                    }
                    arr[slots[k]] = new Node(r * 100 + k);
                }
                churn(r + 1);
                System.gc();
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
            big = arr.length - OVER + 7;
        } else {
            byte[] arr = new byte[OVER];
            arr[OVER - 1] = 7;
            for (int r = 0; r < rounds; r++) {
                Node head = churnList(r);
                System.gc();
                if (check(head, r)) {
                    ok++;
                } else {
                    bad++;
                }
            }
            big = arr[OVER - 1];
        }
        System.out.println("Zs1DgUnwalkableSurvivor mode=" + mode + " rounds=" + rounds
                + " ok=" + ok + " bad=" + bad + " big=" + big);
    }

    static Object sinkHole;

    static void churn(int seed) {
        for (int i = 0; i < 200000; i++) {
            sinkHole = new Node(seed ^ i);
        }
    }

    /** A list of NODES survivors interleaved with ten times as much garbage. */
    static Node churnList(int r) {
        Node head = null;
        for (int i = 0; i < NODES; i++) {
            for (int g = 0; g < 10; g++) {
                sinkHole = new Node(-1);
            }
            Node n = new Node(r * NODES + i);
            n.next = head;
            head = n;
        }
        return head;
    }

    static boolean check(Node head, int r) {
        int expect = r * NODES + NODES - 1;
        int count = 0;
        for (Node n = head; n != null; n = n.next) {
            if (n.id != expect) {
                return false;
            }
            expect--;
            count++;
        }
        return count == NODES;
    }
}
