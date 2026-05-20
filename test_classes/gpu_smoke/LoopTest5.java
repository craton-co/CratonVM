public class LoopTest5 {
    public static int add(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) out[i] = a[i] + b[i];
        return out[n-1];
    }
    public static void main(String[] args) throws Exception {
        int n = 100;  // small, avoid OSR
        int[] a = new int[n]; int[] b = new int[n]; int[] out = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = i * 2; }
        System.out.println("step1");
        int r0 = add(a, b, out);
        System.out.println("step2 r0=" + r0);
        for (int k = 0; k < 600; k++) { add(a, b, out); }  // trigger normal JIT compilation
        System.out.println("step3 out=" + out[n-1]);
        System.out.println("DONE");
    }
}
