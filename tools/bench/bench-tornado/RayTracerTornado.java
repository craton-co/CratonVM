// Simplified twin of TornadoVM-Ray-Tracer's primary-ray sphere intersection +
// shading kernel (apps/TornadoVM-Ray-Tracer/.../renderer/RayTracer.java +
// utils/BodyOps.java's sphere-intersection branch), reduced to a shape both
// TornadoVM's @Parallel and CratonVM's GPU offload analyzer can accept:
// four fixed spheres (unrolled, no loop over a scene array), branchless
// closest-hit selection (no `if` in the hot path), single diffuse term
// against a fixed light direction. No reflections, shadows, plane or
// skybox — those need per-pixel recursive bouncing / dynamic-length scene
// loops that neither analyzer currently admits. See
// docs/gpu/COMPARISON.md and bench-gpu/results/ for the methodology this
// twin follows (VectorAddTornado.java is the canonical example).
//
// One thread per pixel (row-major, width*height threads). Orthographic
// camera looking down -Z (keeps the per-pixel ray direction constant,
// so the kernel is pure elementwise — no per-thread trig for a
// perspective frustum). Output: one packed 0xRRGGBB int per pixel.

import uk.ac.manchester.tornado.api.ImmutableTaskGraph;
import uk.ac.manchester.tornado.api.TaskGraph;
import uk.ac.manchester.tornado.api.TornadoExecutionPlan;
import uk.ac.manchester.tornado.api.annotations.Parallel;
import uk.ac.manchester.tornado.api.enums.DataTransferMode;
import uk.ac.manchester.tornado.api.types.arrays.FloatArray;
import uk.ac.manchester.tornado.api.types.arrays.IntArray;

public class RayTracerTornado {

    // sph: 4 spheres x 5 values (x, y, z, radius, colorIntensity), flattened.
    // TornadoVM's TaskGraph.task() caps at 15 args, so the 20 sphere scalars
    // are bundled into one array and unpacked into the same local names the
    // hot-path math below already used (constant indices, resolved at
    // compile time -- no dynamic array indexing in the loop).
    public static void render(int width, int height,
                               float camX, float camY, float camZ,
                               FloatArray sph,
                               IntArray out) {

        float sx0 = sph.get(0), sy0 = sph.get(1), sz0 = sph.get(2), sr0 = sph.get(3), sc0 = sph.get(4);
        float sx1 = sph.get(5), sy1 = sph.get(6), sz1 = sph.get(7), sr1 = sph.get(8), sc1 = sph.get(9);
        float sx2 = sph.get(10), sy2 = sph.get(11), sz2 = sph.get(12), sr2 = sph.get(13), sc2 = sph.get(14);
        float sx3 = sph.get(15), sy3 = sph.get(16), sz3 = sph.get(17), sr3 = sph.get(18), sc3 = sph.get(19);

        // Fixed light direction (normalized), pointing down-forward-right.
        final float lx = 0.408248f, ly = -0.816497f, lz = 0.408248f;
        // Ray direction is constant across all pixels (orthographic, looking -Z).
        final float rdx = 0f, rdy = 0f, rdz = -1f;

        for (@Parallel int py = 0; py < height; py++) {
            for (@Parallel int px = 0; px < width; px++) {

                float rox = camX + (px - width * 0.5f) * 0.01f;
                float roy = camY + (py - height * 0.5f) * 0.01f;
                float roz = camZ;

                // --- sphere 0 ---
                float dx0 = sx0 - rox, dy0 = sy0 - roy, dz0 = sz0 - roz;
                float t0 = dx0 * rdx + dy0 * rdy + dz0 * rdz;
                float px0 = rox + rdx * t0, py0 = roy + rdy * t0, pz0 = roz + rdz * t0;
                float ex0 = sx0 - px0, ey0 = sy0 - py0, ez0 = sz0 - pz0;
                float yy0 = (float) Math.sqrt(ex0 * ex0 + ey0 * ey0 + ez0 * ez0);
                float disc0 = sr0 * sr0 - yy0 * yy0;
                float root0 = disc0 > 0f ? (float) Math.sqrt(disc0) : 1e9f;
                float t1_0 = t0 - root0;
                float hit0 = (yy0 < sr0 && t1_0 > 0f) ? t1_0 : 1e9f;

                // --- sphere 1 ---
                float dx1 = sx1 - rox, dy1 = sy1 - roy, dz1 = sz1 - roz;
                float t1 = dx1 * rdx + dy1 * rdy + dz1 * rdz;
                float px1 = rox + rdx * t1, py1 = roy + rdy * t1, pz1 = roz + rdz * t1;
                float ex1 = sx1 - px1, ey1 = sy1 - py1, ez1 = sz1 - pz1;
                float yy1 = (float) Math.sqrt(ex1 * ex1 + ey1 * ey1 + ez1 * ez1);
                float disc1 = sr1 * sr1 - yy1 * yy1;
                float root1 = disc1 > 0f ? (float) Math.sqrt(disc1) : 1e9f;
                float t1_1 = t1 - root1;
                float hit1 = (yy1 < sr1 && t1_1 > 0f) ? t1_1 : 1e9f;

                // --- sphere 2 ---
                float dx2 = sx2 - rox, dy2 = sy2 - roy, dz2 = sz2 - roz;
                float t2 = dx2 * rdx + dy2 * rdy + dz2 * rdz;
                float px2 = rox + rdx * t2, py2 = roy + rdy * t2, pz2 = roz + rdz * t2;
                float ex2 = sx2 - px2, ey2 = sy2 - py2, ez2 = sz2 - pz2;
                float yy2 = (float) Math.sqrt(ex2 * ex2 + ey2 * ey2 + ez2 * ez2);
                float disc2 = sr2 * sr2 - yy2 * yy2;
                float root2 = disc2 > 0f ? (float) Math.sqrt(disc2) : 1e9f;
                float t1_2 = t2 - root2;
                float hit2 = (yy2 < sr2 && t1_2 > 0f) ? t1_2 : 1e9f;

                // --- sphere 3 ---
                float dx3 = sx3 - rox, dy3 = sy3 - roy, dz3 = sz3 - roz;
                float t3 = dx3 * rdx + dy3 * rdy + dz3 * rdz;
                float px3 = rox + rdx * t3, py3 = roy + rdy * t3, pz3 = roz + rdz * t3;
                float ex3 = sx3 - px3, ey3 = sy3 - py3, ez3 = sz3 - pz3;
                float yy3 = (float) Math.sqrt(ex3 * ex3 + ey3 * ey3 + ez3 * ez3);
                float disc3 = sr3 * sr3 - yy3 * yy3;
                float root3 = disc3 > 0f ? (float) Math.sqrt(disc3) : 1e9f;
                float t1_3 = t3 - root3;
                float hit3 = (yy3 < sr3 && t1_3 > 0f) ? t1_3 : 1e9f;

                // Closest hit, branchless (min-reduction over 4 candidates).
                float best = Math.min(Math.min(hit0, hit1), Math.min(hit2, hit3));

                // Normal at the hit point of whichever sphere won (weighted
                // pick — at most one of the four masks is 1.0; two distinct
                // spheres never yield the same t for one pixel, and ties are a
                // don't-care here).
                //
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
                float hitAny = m0 + m1 + m2 + m3; // 0 = background, 1 = a sphere
                float shade = (0.15f + 0.85f * diffuse) * baseColor * hitAny;
                shade = shade > 1f ? 1f : (shade < 0f ? 0f : shade);

                int g = (int) (shade * 255f);
                int bg = hitAny > 0.5f ? 0 : 40; // faint background gray
                int r = hitAny > 0.5f ? g : bg;
                int gr = hitAny > 0.5f ? g : bg;
                int b = hitAny > 0.5f ? g : (bg + 20);

                out.set(py * width + px, (r << 16) | (gr << 8) | b);
            }
        }
    }

    public static void main(String[] args) {
        final int width = args.length > 0 ? Integer.parseInt(args[0]) : 640;
        final int height = args.length > 1 ? Integer.parseInt(args[1]) : 480;
        final int iters = args.length > 2 ? Integer.parseInt(args[2]) : 10;
        // Optional 4th arg: dump the frame as raw little-endian int32s, the
        // same format RayTracerKernel writes, so the two toolchains' output
        // can be diffed pixel by pixel rather than only by checksum.
        final String dumpPath = args.length > 3 ? args[3] : null;

        IntArray out = new IntArray(width * height);
        FloatArray sph = new FloatArray(20);
        float[] sphVals = {
                -1.5f, 0.5f, 0f, 1.0f, 1.0f,
                1.5f, 0.5f, 0f, 1.0f, 0.6f,
                0f, -1.0f, -1f, 0.8f, 0.3f,
                0f, 2.0f, -2f, 0.6f, 0.9f,
        };
        for (int i = 0; i < sphVals.length; i++) sph.set(i, sphVals[i]);

        TaskGraph tg = new TaskGraph("s0")
                .transferToDevice(DataTransferMode.FIRST_EXECUTION, sph)
                .task("t0", RayTracerTornado::render, width, height,
                        0f, 0f, 5f, sph, out)
                .transferToHost(DataTransferMode.EVERY_EXECUTION, out);

        ImmutableTaskGraph itg = tg.snapshot();

        try (TornadoExecutionPlan plan = new TornadoExecutionPlan(itg)) {
            plan.execute(); // warm-up: JIT + PTX kernel + device alloc excluded from timing

            long totalNs = 0;
            long bestNs = Long.MAX_VALUE;
            for (int it = 0; it < iters; it++) {
                long t0 = System.nanoTime();
                plan.execute();
                long dt = System.nanoTime() - t0;
                totalNs += dt;
                if (dt < bestNs) bestNs = dt;
            }
            long meanNs = totalNs / iters;

            long checksum = 0;
            for (int i = 0; i < out.getSize(); i++) checksum += out.get(i);

            System.out.println("RAYTRACER_RESULT width=" + width + " height=" + height
                    + " n=" + (width * height) + " mean_ms=" + (meanNs / 1_000_000.0)
                    + " best_ms=" + (bestNs / 1_000_000.0) + " checksum=" + checksum);

            if (dumpPath != null) {
                int n = out.getSize();
                byte[] raw = new byte[n * 4];
                for (int i = 0; i < n; i++) {
                    int v = out.get(i);
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
        } catch (Exception e) {
            e.printStackTrace();
            System.exit(2);
        }
    }
}
