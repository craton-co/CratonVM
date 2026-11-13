public class EligibleHalfPrecisionMatmul {
    // A matrix-vector product whose matrix is half-precision, carried
    // two lanes to an `int` so it can travel through the existing
    // int-array residency path. Lane `2j` is the low half of word `j`
    // and lane `2j+1` the high half, which is what a little-endian
    // bulk copy of an f16 tensor into an `int[]` produces.
    //
    // Exercises `Float.float16ToFloat` (the one non-`Math` entry in the
    // intrinsic table) inside a sequential inner loop.
    public static void matmul(int[] w, float[] x, float[] out) {
        int half = x.length >> 1;
        int rows = out.length;
        for (int i = 0; i < rows; i++) {
            float sum = 0.0f;
            int base = i * half;
            for (int j = 0; j < half; j++) {
                int packed = w[base + j];
                sum += Float.float16ToFloat((short) packed) * x[j << 1]
                     + Float.float16ToFloat((short) (packed >> 16)) * x[(j << 1) + 1];
            }
            out[i] = sum;
        }
    }
}
