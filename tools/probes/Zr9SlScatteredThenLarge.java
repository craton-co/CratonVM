// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zgc round zr, wave z9, lane sl (2026-10-13).
//
// The near-OOM fallback row for CRATONVM_ZGC_SELECTIVE_RELOCATE. The selective
// arm leaves a sparse page in place when its free bytes are in holes of at
// least HOLE_KIB (default 64 KiB): those holes serve TLAB chunks, but not a
// request larger than the hole. This probe builds exactly that heap and then
// asks for more large arrays than fit without compaction:
//
//   1. <keeps> small survivors, each followed by ~<holeKiB> KiB of short-lived
//      garbage, so after a collection the low end is a run of sparse pages
//      with long holes between the survivors;
//   2. then <largeKiB>-KiB arrays, all kept, until <largeMb> MiB are live.
//
// With -Xmx64m and the defaults (400 survivors over ~37.5 MiB, 40 MiB of
// 128 KiB arrays) the large arrays need the space the scattered survivors
// occupy, so the run only completes if a collection after a refused request
// compacts those pages (the selective arm's after-failure bypass). HotSpot
// ZGC 25.0.3 (laptop, -XX:+UseZGC -Xmx64m) prints
// `ZR9SL scattered keeps=400 large=320 sum=394096 ok`; an OutOfMemoryError
// here is the failure this row exists to catch. (128 KiB, not 256: on
// HotSpot a 256 KiB array at -Xmx64m takes a 2 MiB large page each, so
// HotSpot itself OOMEs at 31 of them.)
//
// Usage: Zr9SlScatteredThenLarge [keeps=400] [holeKiB=96] [largeKiB=128] [largeMb=40]
// Output: ZR9SL scattered keeps=<k> large=<n> sum=<s> ok
//    or:  ZR9SL scattered keeps=<k> large=<n> OOME
import java.util.ArrayList;

public class Zr9SlScatteredThenLarge {
    static Object sink;

    public static void main(String[] args) {
        final int keeps = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        final int holeKiB = args.length > 1 ? Integer.parseInt(args[1]) : 96;
        final int largeKiB = args.length > 2 ? Integer.parseInt(args[2]) : 128;
        final int largeMb = args.length > 3 ? Integer.parseInt(args[3]) : 40;

        long[][] kept = new long[keeps][];
        final int garbagePerHole = holeKiB * 1024 / 80; // byte[64] ~ 80 bytes
        for (int i = 0; i < keeps; i++) {
            kept[i] = new long[] {i, 3L * i + 1};
            for (int g = 0; g < garbagePerHole; g++) {
                sink = new byte[64];
            }
        }
        sink = null;
        System.gc();

        ArrayList<byte[]> large = new ArrayList<>();
        final long want = (long) largeMb * 1024 * 1024;
        long have = 0;
        boolean oome = false;
        try {
            while (have < want) {
                byte[] a = new byte[largeKiB * 1024];
                a[0] = (byte) large.size();
                a[a.length - 1] = (byte) (large.size() * 7);
                large.add(a);
                have += a.length;
            }
        } catch (OutOfMemoryError e) {
            oome = true;
        }
        long sum = 0;
        for (long[] k : kept) {
            sum += k[0] + k[1];
        }
        for (int i = 0; i < large.size(); i++) {
            byte[] a = large.get(i);
            sum += (a[0] & 0xff) + (a[a.length - 1] & 0xff);
        }
        final int n = large.size();
        large = null;
        if (oome) {
            System.out.println("ZR9SL scattered keeps=" + keeps + " large=" + n + " OOME");
        } else {
            System.out.println(
                    "ZR9SL scattered keeps=" + keeps + " large=" + n + " sum=" + sum + " ok");
        }
    }
}
