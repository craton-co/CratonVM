public class LoopTest2 {
    public static int add(int[] a, int[] b, int[] out) {
        // Simple loop without the SIMD ewise pattern
        int n = a.length;
        int s = 0;
        for (int i = 0; i < n; i++) s += a[i];
        return s;
    }
    public static void main(String[] args) throws Exception {
        int n = 4096;
        int[] a = new int[n]; int[] b = new int[n]; int[] out = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = i * 2; }
        System.out.println("step1");
        int r0 = add(a, b, out);
        System.out.println("step2 r0=" + r0);
        for (int k = 0; k < 3; k++) { add(a, b, out); }
        System.out.println("step3");
        System.out.println("DONE");
    }
}
