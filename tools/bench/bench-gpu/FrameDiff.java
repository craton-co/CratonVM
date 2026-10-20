// Pixel-by-pixel comparison of two frames dumped by RayTracerKernel /
// RayTracerTornado's optional `dumpPath` argument (raw little-endian
// int32, one packed 0xRRGGBB per pixel, row-major).
//
// A checksum disagreement between two rendering paths says only "these
// two totals differ". This says *how* they differ, which is what tells a
// rounding artefact apart from a logic bug:
//
//   * how many pixels differ at all, and by how much per channel;
//   * whether the differing pixels sit on sphere silhouettes (a
//     boundary/rounding story) or are scattered across whole filled
//     regions (a structural story);
//   * the signed checksum delta, decomposed.
//
// Usage: FrameDiff <width> <height> <reference.bin> <candidate.bin>
public class FrameDiff {

    public static void main(String[] args) throws Exception {
        if (args.length < 4) {
            System.err.println("usage: FrameDiff <width> <height> <reference.bin> <candidate.bin>");
            System.exit(2);
        }
        int width = Integer.parseInt(args[0]);
        int height = Integer.parseInt(args[1]);
        int[] ref = read(args[2], width * height);
        int[] cand = read(args[3], width * height);

        int differing = 0;
        int maxChannelDelta = 0;
        long sumAbsChannelDelta = 0;
        long refSum = 0, candSum = 0;
        // Bucket the per-channel absolute delta so "everything is off by
        // one" and "a few pixels are off by 200" are distinguishable.
        int[] deltaBuckets = new int[6]; // 1, 2, 3-4, 5-8, 9-32, >32
        // Silhouette test: a differing pixel is "on an edge" if any of its
        // four neighbours in the REFERENCE frame has a different value.
        // Rounding artefacts cluster on edges; a structural error does not.
        int differingOnEdge = 0;
        int minX = Integer.MAX_VALUE, minY = Integer.MAX_VALUE;
        int maxX = -1, maxY = -1;
        String firstExample = null;

        for (int i = 0; i < ref.length; i++) {
            refSum += ref[i];
            candSum += cand[i];
            if (ref[i] == cand[i]) continue;
            differing++;
            int x = i % width, y = i / width;
            if (x < minX) minX = x;
            if (x > maxX) maxX = x;
            if (y < minY) minY = y;
            if (y > maxY) maxY = y;
            int worst = 0;
            for (int shift = 0; shift <= 16; shift += 8) {
                int d = Math.abs(((ref[i] >>> shift) & 0xFF) - ((cand[i] >>> shift) & 0xFF));
                sumAbsChannelDelta += d;
                if (d > worst) worst = d;
            }
            if (worst > maxChannelDelta) maxChannelDelta = worst;
            deltaBuckets[bucket(worst)]++;
            if (isEdge(ref, width, height, x, y)) differingOnEdge++;
            if (firstExample == null) {
                firstExample = String.format("(%d,%d) ref=%06X cand=%06X", x, y, ref[i], cand[i]);
            }
        }

        System.out.println("FRAMEDIFF n=" + ref.length
                + " differing=" + differing
                + " (" + pct(differing, ref.length) + ")"
                + " max_channel_delta=" + maxChannelDelta
                + " sum_abs_channel_delta=" + sumAbsChannelDelta
                + " checksum_ref=" + refSum
                + " checksum_cand=" + candSum
                + " checksum_delta=" + (candSum - refSum));
        if (differing == 0) {
            System.out.println("FRAMEDIFF verdict=BIT_IDENTICAL");
            return;
        }
        System.out.println("FRAMEDIFF delta_histogram"
                + " d=1:" + deltaBuckets[0]
                + " d=2:" + deltaBuckets[1]
                + " d=3-4:" + deltaBuckets[2]
                + " d=5-8:" + deltaBuckets[3]
                + " d=9-32:" + deltaBuckets[4]
                + " d>32:" + deltaBuckets[5]);
        System.out.println("FRAMEDIFF on_silhouette=" + differingOnEdge
                + " (" + pct(differingOnEdge, differing) + " of differing pixels)"
                + " bbox=[" + minX + ".." + maxX + "]x[" + minY + ".." + maxY + "]"
                + " first=" + firstExample);
        // The verdict a human would otherwise have to eyeball. "Rounding"
        // requires BOTH a small magnitude and edge clustering; either one
        // alone is not enough to rule out a real logic difference.
        boolean smallMagnitude = maxChannelDelta <= 2;
        boolean edgeClustered = differingOnEdge * 2 >= differing;
        String verdict = smallMagnitude && edgeClustered
                ? "ROUNDING_AT_SILHOUETTES"
                : smallMagnitude
                        ? "SMALL_MAGNITUDE_BUT_NOT_EDGE_CLUSTERED"
                        : "STRUCTURAL";
        System.out.println("FRAMEDIFF verdict=" + verdict);
    }

    private static int bucket(int d) {
        if (d <= 1) return 0;
        if (d == 2) return 1;
        if (d <= 4) return 2;
        if (d <= 8) return 3;
        if (d <= 32) return 4;
        return 5;
    }

    private static boolean isEdge(int[] frame, int width, int height, int x, int y) {
        int here = frame[y * width + x];
        if (x > 0 && frame[y * width + x - 1] != here) return true;
        if (x + 1 < width && frame[y * width + x + 1] != here) return true;
        if (y > 0 && frame[(y - 1) * width + x] != here) return true;
        return y + 1 < height && frame[(y + 1) * width + x] != here;
    }

    private static String pct(long part, long whole) {
        return whole == 0 ? "n/a" : String.format("%.4f%%", 100.0 * part / whole);
    }

    private static int[] read(String path, int expectedPixels) throws Exception {
        byte[] raw = java.nio.file.Files.readAllBytes(java.nio.file.Path.of(path));
        if (raw.length != expectedPixels * 4) {
            throw new IllegalArgumentException(path + ": expected " + (expectedPixels * 4)
                    + " bytes for " + expectedPixels + " pixels, got " + raw.length);
        }
        int[] out = new int[expectedPixels];
        for (int i = 0; i < expectedPixels; i++) {
            out[i] = (raw[i * 4] & 0xFF)
                    | ((raw[i * 4 + 1] & 0xFF) << 8)
                    | ((raw[i * 4 + 2] & 0xFF) << 16)
                    | ((raw[i * 4 + 3] & 0xFF) << 24);
        }
        return out;
    }
}
