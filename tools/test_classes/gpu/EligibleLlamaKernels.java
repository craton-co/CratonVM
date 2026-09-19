


/**
 * Every GPU kernel of the CratonVM inference path, written in the Java
 * subset that CratonVM's bytecode-to-PTX lowering accepts.
 *
 * The rules those methods obey are enforced by the analyzer, not by
 * convention:
 *
 * <ul>
 *   <li>static, primitive or primitive-array parameters only;</li>
 *   <li>exactly one OUTER counted loop {@code for (i = 0; i < bound; i++)}
 *       whose bound is a local holding some parameter's {@code .length}.
 *       That loop is the parallel dimension: one CUDA thread per
 *       iteration, and the launch grid is sized from that same length;</li>
 *   <li>inner loops are ordinary sequential loops the thread runs
 *       itself, which is what makes a per-row reduction expressible;</li>
 *   <li>forward branches only inside the body — no break, no return;</li>
 *   <li>no calls except the curated intrinsic table (Math.sqrt,
 *       Math.exp, Float.float16ToFloat).</li>
 * </ul>
 *
 * <h2>Weight layout</h2>
 *
 * Half-precision weights travel as {@code int[]} with two f16 lanes to
 * a word. The GGUF tensor's own little-endian byte image is already
 * that, row-major, which is why loading is a single bulk copy per
 * tensor — but row-major is the wrong layout to READ. One thread per
 * output row means consecutive threads are a whole row apart, so a
 * warp's 32 loads are 32 separate memory transactions. Every weight is
 * therefore transposed on the device once, at load, into a
 * column-major packed form where consecutive threads read consecutive
 * words. Measured on an RTX 2060 at 32768 rows: 9.4 GB/s row-major
 * against 99.4 GB/s column-major.
 *
 * <h2>Splitting</h2>
 *
 * One thread per output row is also too few threads. A 2048-row
 * projection is 64 warps, which on 30 SMs is two warps each and no way
 * to hide a global-load latency. {@link #matmulSplit} gives thread
 * {@code t} output row {@code t % rows} and chunk {@code t / rows} of
 * the summed dimension, so the launch is {@code chunks} times wider;
 * {@link #reducePartials} then sums the per-chunk partials.
 *
 * That changes the ORDER of the summation, so a split result is not
 * bit-identical to a sequential one. It is the only kernel here that
 * is not; {@code -Dllama.craton.compare=true} measures the difference
 * against the CPU forward pass.
 *
 * <h2>Races</h2>
 *
 * Anything that would race is written to a separate output array
 * rather than in place. There are no barriers inside a kernel; the only
 * ordering guarantee is between kernels on the same stream.
 */
public class EligibleLlamaKernels {

    private EligibleLlamaKernels() {
    }

    /**
     * Transpose a row-major packed-f16 matrix into the column-major
     * packed layout every matmul here reads. One thread per output
     * word; {@code rows} must be even.
     */
    public static void transposeF16(int[] src, int rows, int cols, int[] out) {
        int words = out.length;
        int halfRows = rows >> 1;
        for (int k = 0; k < words; k++) {
            int j = k / halfRows;
            int wi = k - j * halfRows;
            int i0 = wi << 1;
            int flatA = i0 * cols + j;
            int wordA = src[flatA >> 1];
            int lo = wordA;
            if ((flatA & 1) != 0) {
                lo = wordA >> 16;
            }
            int flatB = flatA + cols;
            int wordB = src[flatB >> 1];
            int hi = wordB;
            if ((flatB & 1) != 0) {
                hi = wordB >> 16;
            }
            out[k] = (lo & 0xFFFF) | (hi << 16);
        }
    }

    /**
     * partial[c * rows + i] = sum over chunk c of W[i][j] * x[j], with
     * W column-major packed f16 and rows = partial.length / chunks.
     */
    public static void matmulSplit(int[] wt, float[] x, int chunks, float[] partial) {
        int total = partial.length;
        int n = x.length;
        int rows = total / chunks;
        int halfRows = rows >> 1;
        int per = n / chunks;
        for (int t = 0; t < total; t++) {
            int c = t / rows;
            int i = t - c * rows;
            int wi = i >> 1;
            int sel = i & 1;
            int j0 = c * per;
            int j1 = j0 + per;
            float sum = 0.0f;
            for (int j = j0; j < j1; j++) {
                int packed = wt[j * halfRows + wi];
                int bits = packed;
                if (sel != 0) {
                    bits = packed >> 16;
                }
                sum += Float.float16ToFloat((short) bits) * x[j];
            }
            partial[t] = sum;
        }
    }

    /** out[i] = sum_c partial[c * out.length + i]. */
    public static void reducePartials(float[] partial, int chunks, float[] out) {
        int rows = out.length;
        for (int i = 0; i < rows; i++) {
            float s = 0.0f;
            for (int c = 0; c < chunks; c++) {
                s += partial[c * rows + i];
            }
            out[i] = s;
        }
    }

    /**
     * out[j] = table[row][j], the table being column-major packed f16.
     * {@code halfRows} is the table's row count over two — it cannot be
     * derived from any argument's length here, unlike in the matmuls.
     */
    public static void embedT(int[] table, int row, int halfRows, float[] out) {
        int n = out.length;
        int wi = row >> 1;
        int sel = row & 1;
        for (int j = 0; j < n; j++) {
            int packed = table[j * halfRows + wi];
            int bits = packed;
            if (sel != 0) {
                bits = packed >> 16;
            }
            out[j] = Float.float16ToFloat((short) bits);
        }
    }

    /**
     * scaleOut[0] = 1 / sqrt(mean(x^2) + eps). One thread: the whole
     * reduction is sequential, which for a 2048-element vector costs a
     * few microseconds and avoids needing a cross-thread float
     * reduction, whose summation order would not be Java's.
     */
    public static void rmsScale(float[] x, float eps, float[] scaleOut) {
        int one = scaleOut.length;
        for (int t = 0; t < one; t++) {
            int n = x.length;
            float ss = 0.0f;
            for (int i = 0; i < n; i++) {
                ss += x[i] * x[i];
            }
            ss = ss / n + eps;
            scaleOut[0] = (float) (1.0 / Math.sqrt(ss));
        }
    }

    /** out[i] = w[i] * (scale[0] * x[i]). */
    public static void rmsApply(float[] x, float[] w, float[] scale, float[] out) {
        int n = out.length;
        float s = scale[0];
        for (int i = 0; i < n; i++) {
            out[i] = w[i] * (s * x[i]);
        }
    }

    /**
     * RoPE, out of place: rotating in place would race, because the
     * thread that owns lane i reads its pair partner's lane too.
     */
    public static void rope(float[] vin, float[] fr, float[] fi, int pos, int headSize, float[] vout) {
        int n = vout.length;
        int halfHead = headSize >> 1;
        for (int i = 0; i < n; i++) {
            int hd = i - (i / headSize) * headSize;
            int pair = hd >> 1;
            float fcr = fr[pos * halfHead + pair];
            float fci = fi[pos * halfHead + pair];
            int partner = i + 1;
            if ((hd & 1) != 0) {
                partner = i - 1;
            }
            float a = vin[i];
            float b = vin[partner];
            float r = a * fcr - b * fci;
            if ((hd & 1) != 0) {
                r = b * fci + a * fcr;
            }
            vout[i] = r;
        }
    }

    /** dst[dstOff + i] = src[i]. */
    public static void copyTo(float[] src, int dstOff, float[] dst) {
        int n = src.length;
        for (int i = 0; i < n; i++) {
            dst[dstOff + i] = src[i];
        }
    }

    /**
     * att[h * ctx + t] = dot(q[h], keyCache[t][h / kvMul]) * invSqrt,
     * for every head h and every timestep t &lt;= pos. One thread per
     * (h, t) slot of the whole attention buffer; the slots past pos do
     * nothing, which costs a guard rather than a launch.
     */
    public static void attScores(float[] q, float[] kc, int pos, int headSize, int kvMul, int kvDim,
                                 int ctx, float invSqrt, float[] att) {
        int total = att.length;
        for (int s = 0; s < total; s++) {
            int h = s / ctx;
            int t = s - h * ctx;
            if (t <= pos) {
                int qOff = h * headSize;
                int kOff = t * kvDim + (h / kvMul) * headSize;
                float acc = 0.0f;
                for (int d = 0; d < headSize; d++) {
                    acc += q[qOff + d] * kc[kOff + d];
                }
                att[s] = acc * invSqrt;
            }
        }
    }

    /**
     * In-place softmax of each head's 0..pos row of att. One thread per
     * head — heads is a length-numberOfHeads array used only for its
     * length, because the parallel loop's bound has to be some
     * parameter's length and att's is heads * ctx.
     */
    public static void softmaxRows(float[] att, int pos, int ctx, float[] heads) {
        int nh = heads.length;
        for (int h = 0; h < nh; h++) {
            int off = h * ctx;
            float max = att[off];
            for (int t = 1; t <= pos; t++) {
                float v = att[off + t];
                if (v > max) {
                    max = v;
                }
            }
            float sum = 0.0f;
            for (int t = 0; t <= pos; t++) {
                float e = (float) Math.exp((double) (att[off + t] - max));
                att[off + t] = e;
                sum += e;
            }
            float inv = 1.0f / sum;
            for (int t = 0; t <= pos; t++) {
                att[off + t] = att[off + t] * inv;
            }
        }
    }

    /** xb[h * headSize + d] = sum_t att[h][t] * valueCache[t][h / kvMul][d]. */
    public static void attWeighted(float[] att, float[] vc, int pos, int headSize, int kvMul, int kvDim,
                                   int ctx, float[] xb) {
        int n = xb.length;
        for (int i = 0; i < n; i++) {
            int h = i / headSize;
            int d = i - h * headSize;
            int attOff = h * ctx;
            int vBase = (h / kvMul) * headSize + d;
            float acc = 0.0f;
            for (int t = 0; t <= pos; t++) {
                acc += att[attOff + t] * vc[t * kvDim + vBase];
            }
            xb[i] = acc;
        }
    }

    /** out[i] = out[i] + a[i]. */
    public static void addInto(float[] a, float[] out) {
        int n = out.length;
        for (int i = 0; i < n; i++) {
            out[i] = out[i] + a[i];
        }
    }

    /** out[i] = silu(hb[i]) * hb2[i], silu(v) = v / (1 + exp(-v)). */
    public static void siluMul(float[] hb, float[] hb2, float[] out) {
        int n = out.length;
        for (int i = 0; i < n; i++) {
            float v = hb[i];
            float e = (float) Math.exp((double) (-v));
            out[i] = (v / (1.0f + e)) * hb2[i];
        }
    }
}
