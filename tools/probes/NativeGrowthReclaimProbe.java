// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gc-common w9-d: the per-operation, caller-sized native factories must
// COLLECT before they throw OutOfMemoryError, as HotSpot's Java bodies do
// (docs/internal/gc-common-round-20260923/common-w8b-native-factories-still-single-attempt-FIXED-20260923.md).
// The sibling of NativeFactoryReclaimProbe (w8-b), for the sites w9-d
// converted: ArrayList(int), ArrayList.add's grow, String.toCharArray and
// StringBuilder(int).
//
// Each round keeps what the operation needs live, fills most of the rest of
// the heap with arrays held by one local, drops them all at once, and then
// runs the operation. Live data + dropped garbage + the request exceed the
// heap, so the request can only succeed if something collects the dropped
// garbage first.
//
//   ./target/release/cratonvm --java-home <jdk> -Xmx128m -XX:+UseG1GC \
//       -cp tools/probes NativeGrowthReclaimProbe
//
// Run it on all three collectors and under --compatible and --jdk-only.
// Expected on every one, and on HotSpot: (optional row list: see the end)
//   NativeGrowthReclaimProbe alCapacity=4/4 alAdd=4/4 toCharArray=4/4 sbCapacity=4/4 PROBE-OK
//
// A row reaches the converted native only where that native wins dispatch
// (under --jdk-only the real ArrayList/String bytecode may run instead, which
// collects anyway). A row that shows 4/4 on HotSpot and less here is the
// finding, whichever row it is. Reference arrays are sized for 8-byte
// references; with 4-byte ones (HotSpot's compressed oops) the reference rows
// ask for half as much and pass trivially there.
import java.util.ArrayList;

public class NativeGrowthReclaimProbe {
    static final int ROUNDS = 4;
    static final int PIECES = 1024;

    public static void main(String[] args) {
        long max = Runtime.getRuntime().maxMemory();
        long sink = 0;

        // ArrayList(int): 70% garbage, then a 40% Object[].
        int alCapOk = 0;
        for (int round = 0; round < rounds(args, "alCapacity"); round++) {
            sink += fillAndDrop(max * 70 / 100);
            try {
                ArrayList<Object> l = new ArrayList<>(refs(max * 40 / 100));
                sink += l.size() + 1;
                alCapOk++;
            } catch (OutOfMemoryError e) {
                // counted as a miss
            }
        }

        // ArrayList.add's grow: a full list of ~26% (k refs), 45% garbage,
        // then one add, which needs the old array and a 1.5k-ref one (~40%)
        // live together.
        int alAddOk = 0;
        Object filler = new Object();
        for (int round = 0; round < rounds(args, "alAdd"); round++) {
            try {
                int k = refs(max * 26 / 100);
                ArrayList<Object> l = new ArrayList<>(k);
                for (int i = 0; i < k; i++) {
                    l.add(filler);
                }
                sink += fillAndDrop(max * 45 / 100);
                l.add(filler);
                sink += l.size();
                alAddOk++;
            } catch (OutOfMemoryError e) {
                // counted as a miss
            }
        }

        // String.toCharArray: a String of 15%-of-heap characters (15% as a
        // LATIN1 byte[], 30% if char[]-backed), 58% garbage, then a 30%
        // char[] (two bytes per char).
        int toCharOk = 0;
        for (int round = 0; round < rounds(args, "toCharArray"); round++) {
            try {
                String s = "x".repeat((int) Math.min(Integer.MAX_VALUE - 64, max * 15 / 100));
                sink += fillAndDrop(max * 58 / 100);
                char[] cs = s.toCharArray();
                sink += cs.length;
                toCharOk++;
            } catch (OutOfMemoryError e) {
                // counted as a miss
            }
        }

        // StringBuilder(int): 70% garbage, then a payload of 20%-of-heap
        // characters (40% if two bytes per char, as NativeFactoryReclaimProbe
        // sizes its StringBuilder row).
        int sbOk = 0;
        for (int round = 0; round < rounds(args, "sbCapacity"); round++) {
            sink += fillAndDrop(max * 70 / 100);
            try {
                StringBuilder sb = new StringBuilder((int) Math.min(Integer.MAX_VALUE - 64, max * 40 / 100 / 2));
                sink += sb.capacity();
                sbOk++;
            } catch (OutOfMemoryError e) {
                // counted as a miss
            }
        }

        boolean ok = full(alCapOk, args, "alCapacity") && full(alAddOk, args, "alAdd") && full(toCharOk, args, "toCharArray") && full(sbOk, args, "sbCapacity");
        System.out.println("NativeGrowthReclaimProbe alCapacity=" + row(alCapOk, args, "alCapacity")
                + " alAdd=" + row(alAddOk, args, "alAdd")
                + " toCharArray=" + row(toCharOk, args, "toCharArray")
                + " sbCapacity=" + row(sbOk, args, "sbCapacity")
                + (ok ? " PROBE-OK" : " PROBE-FAIL") + " (sink " + (sink & 1) + ")");
        if (!ok) {
            System.exit(1);
        }
    }

    /** Element count of an 8-byte-reference array of about `bytes` bytes. */
    static int refs(long bytes) {
        return (int) Math.min(Integer.MAX_VALUE - 64, bytes / 8);
    }

    /**
     * Allocate about `bytes` bytes as PIECES arrays held live together, then
     * drop them all at once. Small pieces (about 60-90 KiB at -Xmx128m), so no
     * collector treats them as humongous.
     */
    static long fillAndDrop(long bytes) {
        int pieceBytes = (int) Math.max(1, Math.min(Integer.MAX_VALUE - 64, bytes / PIECES));
        byte[][] junk = new byte[PIECES][];
        long touched = 0;
        for (int i = 0; i < PIECES; i++) {
            junk[i] = new byte[pieceBytes];
            junk[i][pieceBytes - 1] = (byte) i;
            touched += junk[i][pieceBytes - 1];
        }
        return touched;
    }

    // gcd d10/o (2026-09-28): an optional first argument, a comma-separated
    // list of row names (`alCapacity,alAdd,toCharArray,sbCapacity`), runs only
    // those rows, so a failing run can be split into the row that leaves data
    // behind and the row that meets it (`alAdd,sbCapacity`). Unlisted rows
    // print `skipped` and do not count against PROBE-OK. With no argument the
    // probe runs and prints exactly as before (every line number above is
    // unchanged, so `NativeGrowthReclaimProbe.java:95` / `:131` still name the
    // sbCapacity rounds' fill and the fill's `new byte[pieceBytes]`).

    /** ROUNDS when `row` is selected (or no selection was given), else 0. */
    static int rounds(String[] args, String row) {
        return selected(args, row) ? ROUNDS : 0;
    }

    static boolean selected(String[] args, String row) {
        if (args.length == 0) {
            return true;
        }
        for (String name : args[0].split(",")) {
            if (name.trim().equals(row)) {
                return true;
            }
        }
        return false;
    }

    /** A selected row passed every round; an unselected row never fails. */
    static boolean full(int passed, String[] args, String row) {
        return !selected(args, row) || passed == ROUNDS;
    }

    static String row(int passed, String[] args, String row) {
        return selected(args, row) ? passed + "/" + ROUNDS : "skipped";
    }
}
