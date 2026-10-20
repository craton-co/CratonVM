public class EligibleSplitMatmul {
    // Split-K matrix-vector product: thread `t` owns output row
    // `t % rows` and chunk `t / rows` of the summed dimension, so the
    // launch has `chunks` times as many threads as there are rows.
    public static void matmulColSplit(int[] wt, float[] x, int chunks, float[] partial) {
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

    // Second half of the split: sum the per-chunk partials per row.
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
}
