public class LoopTest4 {
    public static int add(int[] a) {
        int n = a.length;
        int s = 0;
        for (int i = 0; i < n; i++) s += a[i];
        return s;
    }
    public static void main(String[] args) throws Exception {
        int n = 100;  // small, no OSR
        int[] a = new int[n];
        for (int i = 0; i < n; i++) a[i] = i;
        System.out.println("step1");
        int r0 = add(a);
        System.out.println("step2 r0=" + r0);
        for (int k = 0; k < 600; k++) { add(a); }  // trigger normal JIT via call count
        System.out.println("step3");
        System.out.println("DONE");
    }
}
