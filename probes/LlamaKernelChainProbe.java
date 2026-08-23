import craton.gpu.GpuArray;
import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;

/**
 * Numerical oracle for every kernel of the GPULlama3 inference path.
 *
 * The kernels are ordinary static Java methods, so the same source that
 * the lowering compiles to PTX also runs on the CPU by just calling it.
 * That makes the oracle exact and free: dispatch a kernel to the device,
 * call it on the host with the same inputs, compare. A lowering bug — a
 * mis-recovered index, a dropped branch arm, a phi that carries the
 * wrong register — shows up here as a mismatched lane, on a 64-element
 * tensor, in a second.
 *
 * Two kernels cannot be exact and are reported rather than asserted:
 * {@code softmaxRows} and {@code siluMul} go through
 * {@code ex2.approx.f32}, whose contract is about 2 ULP. Their rows
 * print a relative error instead of a lane count.
 *
 * The dimensions are deliberately tiny and deliberately NOT powers of
 * the same number as each other: a kernel that confuses `headSize` with
 * `kvDim`, or `rows` with `cols`, passes a square fixture.
 */
public class LlamaKernelChainProbe {

    private static final String K = "org/beehive/gpullama3/craton/CratonKernels";

    static int dim = 64;
    static int headSize = 16;
    static int nHeads = 4;
    static int nKvHeads = 2;
    static int kvDim = 32;      // dim * nKvHeads / nHeads
    static int kvMul = 2;
    static int hidden = 96;
    static int ctx = 8;
    static int vocab = 12;

    static int fails;
    static int rows;

    static void cmp(String tag, float[] gpu, float[] cpu) {
        int bad = 0;
        double worstRel = 0.0;
        for (int i = 0; i < cpu.length; i++) {
            if (Float.floatToRawIntBits(gpu[i]) != Float.floatToRawIntBits(cpu[i])) {
                bad++;
                double scale = Math.max(Math.abs((double) cpu[i]), 1e-6);
                worstRel = Math.max(worstRel, Math.abs((double) gpu[i] - cpu[i]) / scale);
            }
        }
        rows++;
        if (bad != 0) {
            fails++;
        }
        System.out.printf("%-14s lanes=%d diff=%d worst_rel=%.3g %s%n",
                tag, cpu.length, bad, worstRel, bad == 0 ? "EXACT" : "DIFFERS");
    }

    /** Same as cmp but only reports — for the two approximate kernels. */
    static void approx(String tag, float[] gpu, float[] cpu) {
        double worstRel = 0.0;
        for (int i = 0; i < cpu.length; i++) {
            double scale = Math.max(Math.abs((double) cpu[i]), 1e-6);
            worstRel = Math.max(worstRel, Math.abs((double) gpu[i] - cpu[i]) / scale);
        }
        rows++;
        System.out.printf("%-14s lanes=%d worst_rel=%.3g APPROX%n", tag, cpu.length, worstRel);
    }

    static float[] noise(int n, int seed) {
        float[] a = new float[n];
        for (int i = 0; i < n; i++) {
            a[i] = (((i * 31 + seed * 17) % 97) - 48) / 64.0f;
        }
        return a;
    }

    /** Row-major packed f16 for a rows x cols matrix. */
    static int[] weights(int r, int c, int seed) {
        short[] h = new short[r * c];
        for (int i = 0; i < h.length; i++) {
            h[i] = Float.floatToFloat16((((i * 13 + seed * 7) % 61) - 30) / 128.0f);
        }
        int[] w = new int[h.length / 2];
        for (int k = 0; k < w.length; k++) {
            w[k] = (h[2 * k] & 0xFFFF) | (h[2 * k + 1] << 16);
        }
        return w;
    }

    static void await(GpuFuture<Void> f) throws Exception {
        f.get();
    }

    public static void main(String[] args) throws Exception {
        try (GpuExecutor exec = GpuExecutor.open()) {

            // ---- transposeF16, then the two matmul halves ----
            int[] wRow = weights(dim, dim, 1);
            int[] wtCpu = new int[wRow.length];
            org.beehive.gpullama3.craton.CratonKernels.transposeF16(wRow, dim, dim, wtCpu);

            GpuArray<int[]> gwRow = GpuArray.wrap(wRow);
            GpuArray<int[]> gwt = GpuArray.wrap(new int[wRow.length]);
            await(exec.submit(K, "transposeF16", "([III[I)V",
                    gwRow, Integer.valueOf(dim), Integer.valueOf(dim), gwt));
            int[] wtGpu = new int[wRow.length];
            gwt.toHost(wtGpu);
            int tbad = 0;
            for (int i = 0; i < wtCpu.length; i++) {
                if (wtCpu[i] != wtGpu[i]) {
                    tbad++;
                }
            }
            rows++;
            if (tbad != 0) {
                fails++;
            }
            System.out.printf("%-14s words=%d diff=%d %s%n", "transposeF16",
                    wtCpu.length, tbad, tbad == 0 ? "EXACT" : "DIFFERS");

            float[] x = noise(dim, 3);
            GpuArray<float[]> gx = GpuArray.wrap(x);

            int chunks = 4;
            float[] partCpu = new float[dim * chunks];
            org.beehive.gpullama3.craton.CratonKernels.matmulSplit(wtCpu, x, chunks, partCpu);
            GpuArray<float[]> gpart = GpuArray.wrap(new float[dim * chunks]);
            await(exec.submit(K, "matmulSplit", "([I[FI[F)V",
                    gwt, gx, Integer.valueOf(chunks), gpart));
            float[] partGpu = new float[dim * chunks];
            gpart.toHost(partGpu);
            cmp("matmulSplit", partGpu, partCpu);

            float[] outCpu = new float[dim];
            org.beehive.gpullama3.craton.CratonKernels.reducePartials(partCpu, chunks, outCpu);
            GpuArray<float[]> gout = GpuArray.wrap(new float[dim]);
            await(exec.submit(K, "reducePartials", "([FI[F)V",
                    gpart, Integer.valueOf(chunks), gout));
            float[] outGpu = new float[dim];
            gout.toHost(outGpu);
            cmp("reducePartials", outGpu, outCpu);

            // ---- embedT off a transposed vocab table ----
            int[] tabRow = weights(vocab, dim, 5);
            int[] tabCol = new int[tabRow.length];
            org.beehive.gpullama3.craton.CratonKernels.transposeF16(tabRow, vocab, dim, tabCol);
            GpuArray<int[]> gtab = GpuArray.wrap(tabCol);
            for (int row : new int[] {0, 1, vocab - 2, vocab - 1}) {
                float[] eCpu = new float[dim];
                org.beehive.gpullama3.craton.CratonKernels.embedT(tabCol, row, vocab >> 1, eCpu);
                GpuArray<float[]> ge = GpuArray.wrap(new float[dim]);
                await(exec.submit(K, "embedT", "([III[F)V",
                        gtab, Integer.valueOf(row), Integer.valueOf(vocab >> 1), ge));
                float[] eGpu = new float[dim];
                ge.toHost(eGpu);
                cmp("embedT[" + row + "]", eGpu, eCpu);
                ge.close();
            }

            // ---- rmsnorm halves ----
            float eps = 1e-5f;
            float[] sCpu = new float[1];
            org.beehive.gpullama3.craton.CratonKernels.rmsScale(x, eps, sCpu);
            GpuArray<float[]> gs = GpuArray.wrap(new float[1]);
            await(exec.submit(K, "rmsScale", "([FF[F)V", gx, Float.valueOf(eps), gs));
            float[] sGpu = new float[1];
            gs.toHost(sGpu);
            cmp("rmsScale", sGpu, sCpu);

            float[] wn = noise(dim, 9);
            float[] rCpu = new float[dim];
            org.beehive.gpullama3.craton.CratonKernels.rmsApply(x, wn, sCpu, rCpu);
            GpuArray<float[]> gwn = GpuArray.wrap(wn);
            GpuArray<float[]> gr = GpuArray.wrap(new float[dim]);
            await(exec.submit(K, "rmsApply", "([F[F[F[F)V", gx, gwn, gs, gr));
            float[] rGpu = new float[dim];
            gr.toHost(rGpu);
            cmp("rmsApply", rGpu, rCpu);

            // ---- rope ----
            int halfHead = headSize >> 1;
            float[] fr = noise(ctx * halfHead, 11);
            float[] fi = noise(ctx * halfHead, 13);
            int pos = 3;
            float[] ropeCpu = new float[dim];
            org.beehive.gpullama3.craton.CratonKernels.rope(x, fr, fi, pos, headSize, ropeCpu);
            GpuArray<float[]> gfr = GpuArray.wrap(fr);
            GpuArray<float[]> gfi = GpuArray.wrap(fi);
            GpuArray<float[]> grope = GpuArray.wrap(new float[dim]);
            await(exec.submit(K, "rope", "([F[F[FII[F)V", gx, gfr, gfi,
                    Integer.valueOf(pos), Integer.valueOf(headSize), grope));
            float[] ropeGpu = new float[dim];
            grope.toHost(ropeGpu);
            cmp("rope", ropeGpu, ropeCpu);

            // ---- copyTo into a KV cache ----
            float[] kv = noise(kvDim, 17);
            float[] cacheCpu = new float[ctx * kvDim];
            org.beehive.gpullama3.craton.CratonKernels.copyTo(kv, pos * kvDim, cacheCpu);
            GpuArray<float[]> gkv = GpuArray.wrap(kv);
            GpuArray<float[]> gcache = GpuArray.wrap(new float[ctx * kvDim]);
            await(exec.submit(K, "copyTo", "([FI[F)V",
                    gkv, Integer.valueOf(pos * kvDim), gcache));
            float[] cacheGpu = new float[ctx * kvDim];
            gcache.toHost(cacheGpu);
            cmp("copyTo", cacheGpu, cacheCpu);

            // ---- attention: scores, softmax, weighted sum ----
            float[] kcache = noise(ctx * kvDim, 19);
            float[] vcache = noise(ctx * kvDim, 23);
            float inv = (float) (1.0 / Math.sqrt(headSize));
            float[] attCpu = new float[nHeads * ctx];
            org.beehive.gpullama3.craton.CratonKernels.attScores(ropeCpu, kcache, pos, headSize, kvMul, kvDim,
                    ctx, inv, attCpu);
            GpuArray<float[]> gkc = GpuArray.wrap(kcache);
            GpuArray<float[]> gatt = GpuArray.wrap(new float[nHeads * ctx]);
            await(exec.submit(K, "attScores", "([F[FIIIIIF[F)V", grope, gkc,
                    Integer.valueOf(pos), Integer.valueOf(headSize), Integer.valueOf(kvMul),
                    Integer.valueOf(kvDim), Integer.valueOf(ctx), Float.valueOf(inv), gatt));
            float[] attGpu = new float[nHeads * ctx];
            gatt.toHost(attGpu);
            cmp("attScores", attGpu, attCpu);

            float[] headsArr = new float[nHeads];
            float[] smCpu = attCpu.clone();
            org.beehive.gpullama3.craton.CratonKernels.softmaxRows(smCpu, pos, ctx, headsArr);
            GpuArray<float[]> gheads = GpuArray.wrap(headsArr);
            await(exec.submit(K, "softmaxRows", "([FII[F)V", gatt,
                    Integer.valueOf(pos), Integer.valueOf(ctx), gheads));
            float[] smGpu = new float[nHeads * ctx];
            gatt.toHost(smGpu);
            approx("softmaxRows", smGpu, smCpu);

            float[] xbCpu = new float[dim];
            org.beehive.gpullama3.craton.CratonKernels.attWeighted(smCpu, vcache, pos, headSize, kvMul, kvDim,
                    ctx, xbCpu);
            GpuArray<float[]> gvc = GpuArray.wrap(vcache);
            GpuArray<float[]> gxb = GpuArray.wrap(new float[dim]);
            await(exec.submit(K, "attWeighted", "([F[FIIIII[F)V", gatt, gvc,
                    Integer.valueOf(pos), Integer.valueOf(headSize), Integer.valueOf(kvMul),
                    Integer.valueOf(kvDim), Integer.valueOf(ctx), gxb));
            float[] xbGpu = new float[dim];
            gxb.toHost(xbGpu);
            approx("attWeighted", xbGpu, xbCpu);

            // ---- residual and SwiGLU ----
            float[] addCpu = noise(dim, 29);
            float[] addSrc = noise(dim, 31);
            GpuArray<float[]> gaddOut = GpuArray.wrap(addCpu.clone());
            GpuArray<float[]> gaddSrc = GpuArray.wrap(addSrc);
            org.beehive.gpullama3.craton.CratonKernels.addInto(addSrc, addCpu);
            await(exec.submit(K, "addInto", "([F[F)V", gaddSrc, gaddOut));
            float[] addGpu = new float[dim];
            gaddOut.toHost(addGpu);
            cmp("addInto", addGpu, addCpu);

            float[] hb = noise(hidden, 37);
            float[] hb2 = noise(hidden, 41);
            float[] siluCpu = new float[hidden];
            org.beehive.gpullama3.craton.CratonKernels.siluMul(hb, hb2, siluCpu);
            GpuArray<float[]> ghb = GpuArray.wrap(hb);
            GpuArray<float[]> ghb2 = GpuArray.wrap(hb2);
            GpuArray<float[]> gsilu = GpuArray.wrap(new float[hidden]);
            await(exec.submit(K, "siluMul", "([F[F[F)V", ghb, ghb2, gsilu));
            float[] siluGpu = new float[hidden];
            gsilu.toHost(siluGpu);
            approx("siluMul", siluGpu, siluCpu);
        }

        System.out.printf("CHAIN rows=%d exact_failures=%d %s%n",
                rows, fails, fails == 0 ? "PASS" : "FAIL");
    }
}
