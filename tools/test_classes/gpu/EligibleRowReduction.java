public class EligibleRowReduction {
    // One row-dot per thread: the OUTER loop is the parallel dimension
    // (one CUDA thread per output row) and the INNER loop is a real,
    // sequential per-thread PTX loop carrying a float accumulator.
    //
    // This is the shape a matrix-vector product has, and the shape a
    // rectangular 2-D flattening cannot express: `sum` is loop-carried,
    // so its `j` iterations must run in order on one thread.
    //
    // Both bounds are plain `iload`s of locals populated by
    // `arraylength`, the same idiom every other fixture here uses.
    public static void matmul(float[] w, float[] x, float[] out) {
        int n = x.length;
        int rows = out.length;
        for (int i = 0; i < rows; i++) {
            float sum = 0.0f;
            for (int j = 0; j < n; j++) {
                sum += w[i * n + j] * x[j];
            }
            out[i] = sum;
        }
    }

    // Same shape with an integer accumulator, so the inner loop's phi
    // is an `.s32` rather than an `.f32` register.
    public static void rowSums(int[] a, int[] out, int[] cols) {
        int n = cols.length;
        int rows = out.length;
        for (int i = 0; i < rows; i++) {
            int sum = 0;
            for (int j = 0; j < n; j++) {
                sum = sum + a[i * n + j];
            }
            out[i] = sum;
        }
    }
}
