// CratonVM-native twin of bench-tornado/RayTracerTornado.java — same
// branchless 4-sphere primary-ray intersection + diffuse shading, flattened
// to a single canonical counted loop (one thread per pixel, tid = py*width+px)
// to match the offload analyzer's currently-supported loop shape. See
// bench-tornado/RayTracerTornado.java's header comment for what was cut
// versus the real TornadoVM-Ray-Tracer app (reflections, shadows, plane,
// skybox — all need dynamic scene loops / recursive bounces neither
// analyzer admits today).
import craton.gpu.GpuKernel;
import craton.gpu.AdmissionHint;

public class RayTracerKernel {

    @GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
    public static void render(int width, int height,
                               float camX, float camY, float camZ,
                               float sx0, float sy0, float sz0, float sr0, float sc0,
                               float sx1, float sy1, float sz1, float sr1, float sc1,
                               float sx2, float sy2, float sz2, float sr2, float sc2,
                               float sx3, float sy3, float sz3, float sr3, float sc3,
                               int[] out) {

        final float lx = 0.408248f, ly = -0.816497f, lz = 0.408248f;
        final float rdx = 0f, rdy = 0f, rdz = -1f;

        int n = out.length;
        for (int tid = 0; tid < n; tid++) {
            int px = tid % width;
            int py = tid / width;

            float rox = camX + (px - width * 0.5f) * 0.01f;
            float roy = camY + (py - height * 0.5f) * 0.01f;
            float roz = camZ;

            float dx0 = sx0 - rox, dy0 = sy0 - roy, dz0 = sz0 - roz;
            float t0 = dx0 * rdx + dy0 * rdy + dz0 * rdz;
            float px0 = rox + rdx * t0, py0 = roy + rdy * t0, pz0 = roz + rdz * t0;
            float ex0 = sx0 - px0, ey0 = sy0 - py0, ez0 = sz0 - pz0;
            float yy0 = (float) Math.sqrt(ex0 * ex0 + ey0 * ey0 + ez0 * ez0);
            float disc0 = sr0 * sr0 - yy0 * yy0;
            float root0 = disc0 > 0f ? (float) Math.sqrt(disc0) : 1e9f;
            float t1_0 = t0 - root0;
            float hit0 = (yy0 < sr0 && t1_0 > 0f) ? t1_0 : 1e9f;

            float dx1 = sx1 - rox, dy1 = sy1 - roy, dz1 = sz1 - roz;
            float t1 = dx1 * rdx + dy1 * rdy + dz1 * rdz;
            float px1 = rox + rdx * t1, py1 = roy + rdy * t1, pz1 = roz + rdz * t1;
            float ex1 = sx1 - px1, ey1 = sy1 - py1, ez1 = sz1 - pz1;
            float yy1 = (float) Math.sqrt(ex1 * ex1 + ey1 * ey1 + ez1 * ez1);
            float disc1 = sr1 * sr1 - yy1 * yy1;
            float root1 = disc1 > 0f ? (float) Math.sqrt(disc1) : 1e9f;
            float t1_1 = t1 - root1;
            float hit1 = (yy1 < sr1 && t1_1 > 0f) ? t1_1 : 1e9f;

            float dx2 = sx2 - rox, dy2 = sy2 - roy, dz2 = sz2 - roz;
            float t2 = dx2 * rdx + dy2 * rdy + dz2 * rdz;
            float px2 = rox + rdx * t2, py2 = roy + rdy * t2, pz2 = roz + rdz * t2;
            float ex2 = sx2 - px2, ey2 = sy2 - py2, ez2 = sz2 - pz2;
            float yy2 = (float) Math.sqrt(ex2 * ex2 + ey2 * ey2 + ez2 * ez2);
            float disc2 = sr2 * sr2 - yy2 * yy2;
            float root2 = disc2 > 0f ? (float) Math.sqrt(disc2) : 1e9f;
            float t1_2 = t2 - root2;
            float hit2 = (yy2 < sr2 && t1_2 > 0f) ? t1_2 : 1e9f;

            float dx3 = sx3 - rox, dy3 = sy3 - roy, dz3 = sz3 - roz;
            float t3 = dx3 * rdx + dy3 * rdy + dz3 * rdz;
            float px3 = rox + rdx * t3, py3 = roy + rdy * t3, pz3 = roz + rdz * t3;
            float ex3 = sx3 - px3, ey3 = sy3 - py3, ez3 = sz3 - pz3;
            float yy3 = (float) Math.sqrt(ex3 * ex3 + ey3 * ey3 + ez3 * ez3);
            float disc3 = sr3 * sr3 - yy3 * yy3;
            float root3 = disc3 > 0f ? (float) Math.sqrt(disc3) : 1e9f;
            float t1_3 = t3 - root3;
            float hit3 = (yy3 < sr3 && t1_3 > 0f) ? t1_3 : 1e9f;

            float best = Math.min(Math.min(hit0, hit1), Math.min(hit2, hit3));

            // A miss leaves every hit_i at the 1e9f sentinel, so `best == hit_i`
            // is true for ALL FOUR and hitAny would come out 4, not 0 — the
            // background branch below would then be dead code and every miss
            // pixel would shade through 1e9f-scale arithmetic that saturates the
            // clamp. Gate the masks on "something was actually hit" (still
            // branchless: a select, like the rest of the kernel).
            float anyHit = best < 1e9f ? 1f : 0f;
            float m0 = (best == hit0 ? 1f : 0f) * anyHit;
            float m1 = (best == hit1 ? 1f : 0f) * anyHit;
            float m2 = (best == hit2 ? 1f : 0f) * anyHit;
            float m3 = (best == hit3 ? 1f : 0f) * anyHit;
            float hpx = rox + rdx * best, hpy = roy + rdy * best, hpz = roz + rdz * best;
            float nx = (m0 * (hpx - sx0) / sr0) + (m1 * (hpx - sx1) / sr1)
                     + (m2 * (hpx - sx2) / sr2) + (m3 * (hpx - sx3) / sr3);
            float ny = (m0 * (hpy - sy0) / sr0) + (m1 * (hpy - sy1) / sr1)
                     + (m2 * (hpy - sy2) / sr2) + (m3 * (hpy - sy3) / sr3);
            float nz = (m0 * (hpz - sz0) / sr0) + (m1 * (hpz - sz1) / sr1)
                     + (m2 * (hpz - sz2) / sr2) + (m3 * (hpz - sz3) / sr3);

            float diffuse = nx * (-lx) + ny * (-ly) + nz * (-lz);
            diffuse = diffuse > 0f ? diffuse : 0f;

            float baseColor = m0 * sc0 + m1 * sc1 + m2 * sc2 + m3 * sc3;
            float hitAny = m0 + m1 + m2 + m3;
            float shade = (0.15f + 0.85f * diffuse) * baseColor * hitAny;
            shade = shade > 1f ? 1f : (shade < 0f ? 0f : shade);

            int g = (int) (shade * 255f);
            int bg = hitAny > 0.5f ? 0 : 40;
            int r = hitAny > 0.5f ? g : bg;
            int gr = hitAny > 0.5f ? g : bg;
            int b = hitAny > 0.5f ? g : (bg + 20);

            out[tid] = (r << 16) | (gr << 8) | b;
        }
    }

    /// `RayTracerKernel <width> <height> <iters> [dumpPath]`
    ///
    /// With a fourth argument the rendered frame is written to `dumpPath`
    /// as raw little-endian `int32`s (one per pixel, row-major). That is
    /// what makes a checksum disagreement diagnosable: dump the same frame
    /// from the CPU and from `--gpu` and compare pixel by pixel, instead of
    /// staring at two totals that differ by an unexplained amount.
    public static void main(String[] args) throws Exception {
        int width = args.length > 0 ? Integer.parseInt(args[0]) : 1920;
        int height = args.length > 1 ? Integer.parseInt(args[1]) : 1080;
        int n = width * height;
        int[] out = new int[n];

        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 10;
        String dumpPath = args.length > 3 ? args[3] : null;

        // Warm-up: first call pays analyze+lower+PTX-cache-miss (excluded).
        render(width, height, 0f, 0f, 5f,
                -1.5f, 0.5f, 0f, 1.0f, 1.0f,
                1.5f, 0.5f, 0f, 1.0f, 0.6f,
                0f, -1.0f, -1f, 0.8f, 0.3f,
                0f, 2.0f, -2f, 0.6f, 0.9f,
                out);

        long totalNs = 0, bestNs = Long.MAX_VALUE;
        for (int it = 0; it < iters; it++) {
            long t0 = System.nanoTime();
            render(width, height, 0f, 0f, 5f,
                    -1.5f, 0.5f, 0f, 1.0f, 1.0f,
                    1.5f, 0.5f, 0f, 1.0f, 0.6f,
                    0f, -1.0f, -1f, 0.8f, 0.3f,
                    0f, 2.0f, -2f, 0.6f, 0.9f,
                    out);
            long dt = System.nanoTime() - t0;
            totalNs += dt;
            if (dt < bestNs) bestNs = dt;
        }

        long checksum = 0;
        for (int px : out) checksum += px;
        System.out.println("RAYTRACER_RESULT width=" + width + " height=" + height
                + " n=" + n + " mean_ms=" + (totalNs / (double) iters / 1_000_000.0)
                + " best_ms=" + (bestNs / 1_000_000.0)
                + " checksum=" + checksum);

        if (dumpPath != null) {
            byte[] raw = new byte[n * 4];
            for (int i = 0; i < n; i++) {
                int v = out[i];
                raw[i * 4] = (byte) v;
                raw[i * 4 + 1] = (byte) (v >>> 8);
                raw[i * 4 + 2] = (byte) (v >>> 16);
                raw[i * 4 + 3] = (byte) (v >>> 24);
            }
            try (java.io.OutputStream os = new java.io.FileOutputStream(dumpPath)) {
                os.write(raw);
            }
            System.out.println("RAYTRACER_DUMP path=" + dumpPath + " bytes=" + raw.length);
        }
    }
}
