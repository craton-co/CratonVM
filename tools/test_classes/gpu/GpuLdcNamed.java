// A kernel-SHAPED method (static, primitive-array params, void return)
// whose body still cannot be analyzed: the `ldc` targets a String, which
// has no GPU-representable immediate form even with the pool available.
// It must land in the NAMED half of the analyzer-refusal census.
public class GpuLdcNamed {
    static void stringConst(int[] in, int[] out) {
        String s = "not-a-number";
        for (int i = 0; i < out.length; i++) { out[i] = in[i] + s.length(); }
    }
    static void drive(int iters, int[] a, int[] b) {
        for (int k = 0; k < iters; k++) stringConst(a, b);
    }
    public static void main(String[] args) {
        int n = 262144;
        int[] a = new int[n], b = new int[n];
        drive(40, a, b);
        System.out.println("sum=" + b[0]);
    }
}
