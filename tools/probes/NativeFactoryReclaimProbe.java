// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gc-common w8-b: a native array factory must COLLECT before it throws
// OutOfMemoryError, as HotSpot does
// (docs/internal/gc-common-round-20260923/common-w3c-native-callback-allocations-cannot-collect-FIXED-20260923.md).
//
// Each round fills about 70% of the heap with arrays held live by one local,
// drops them all at once, and then asks a NATIVE factory for about 40% of the
// heap. The two together exceed the heap, so the request can only succeed if
// something collects the dropped 70% first. HotSpot collects inside the
// factory; before w8-b CratonVM's natives got one no-GC attempt and threw.
//
//   ./target/release/cratonvm --java-home <jdk> -Xmx128m -XX:+UseG1GC \
//       -cp tools/probes NativeFactoryReclaimProbe
//
// Run it on all three collectors. Expected on every one, and on HotSpot:
//   NativeFactoryReclaimProbe newInstance=8/8 ensureCapacity=8/8 PROBE-OK
//
// `Array.newInstance(Class,int)` reaches the genuine JDK native
// `Array.newArray` in every mode. `StringBuilder.ensureCapacity` reaches the
// native growth path only where that native wins dispatch; a row that shows
// 8/8 on HotSpot and less here is the finding, whichever row it is.
public class NativeFactoryReclaimProbe {
    static final int ROUNDS = 8;

    public static void main(String[] args) {
        long max = Runtime.getRuntime().maxMemory();
        // Small pieces (about 90 KiB at -Xmx128m), so no collector treats them
        // as humongous and the fill itself fits.
        int garbagePieces = 1024;
        int pieceBytes = (int) Math.min(Integer.MAX_VALUE - 64, (max * 7 / 10) / garbagePieces);
        int requestInts = (int) Math.min(Integer.MAX_VALUE - 64, (max * 4 / 10) / 4);
        int requestChars = (int) Math.min(Integer.MAX_VALUE - 64, (max * 4 / 10) / 2);
        long sink = 0;

        int newInstanceOk = 0;
        for (int round = 0; round < ROUNDS; round++) {
            sink += fillAndDrop(garbagePieces, pieceBytes);
            try {
                int[] big = (int[]) java.lang.reflect.Array.newInstance(int.class, requestInts);
                sink += big.length;
                newInstanceOk++;
            } catch (OutOfMemoryError e) {
                // counted as a miss below
            }
        }

        int ensureOk = 0;
        for (int round = 0; round < ROUNDS; round++) {
            sink += fillAndDrop(garbagePieces, pieceBytes);
            try {
                StringBuilder sb = new StringBuilder();
                sb.ensureCapacity(requestChars);
                sink += sb.capacity();
                ensureOk++;
            } catch (OutOfMemoryError e) {
                // counted as a miss below
            }
        }

        boolean ok = newInstanceOk == ROUNDS && ensureOk == ROUNDS;
        System.out.println("NativeFactoryReclaimProbe newInstance=" + newInstanceOk + "/" + ROUNDS
                + " ensureCapacity=" + ensureOk + "/" + ROUNDS
                + (ok ? " PROBE-OK" : " PROBE-FAIL") + " (sink " + (sink & 1) + ")");
        if (!ok) {
            System.exit(1);
        }
    }

    /** Allocate `pieces` arrays held live together, then drop them all at once. */
    static long fillAndDrop(int pieces, int pieceBytes) {
        byte[][] junk = new byte[pieces][];
        long touched = 0;
        for (int i = 0; i < pieces; i++) {
            junk[i] = new byte[pieceBytes];
            junk[i][pieceBytes - 1] = (byte) i;
            touched += junk[i][pieceBytes - 1];
        }
        return touched;
    }
}
