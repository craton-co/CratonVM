public class EligibleSaxpy {
    public static void saxpy(float a, float[] x, float[] y, float[] out) {
        int n = x.length;
        for (int i = 0; i < n; i++) {
            out[i] = a * x[i] + y[i];
        }
    }
}
