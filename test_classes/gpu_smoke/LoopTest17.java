public class LoopTest17 {
    static int dummy = 0;
    public static int sum2(int[] a) {
        int n = a.length;
        dummy = 1;  // non-IR force AFTER reading array
        return n;
    }
    public static void main(String[] args) throws Exception {
        int[] a = new int[100];
        System.out.println("step1");
        int r0 = sum2(a);
        System.out.println("step2 r0=" + r0);
        for (int k = 0; k < 3000; k++) { sum2(a); }
        System.out.println("step3");
        System.out.println("DONE");
    }
}
