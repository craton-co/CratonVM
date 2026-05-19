public class LoopTest10 {
    static int dummy = 0;
    public static int sum(int[] a) {
        dummy = 1;  // force non-IR
        int n = 100;  // hardcoded n, don't read a
        int s = 0;
        for (int i = 0; i < n; i++) s += i;
        return s;
    }
    public static void main(String[] args) throws Exception {
        int[] a = new int[100];
        System.out.println("step1");
        int r0 = sum(a);
        System.out.println("step2 r0=" + r0);
        for (int k = 0; k < 3000; k++) { sum(a); }
        System.out.println("step3");
        System.out.println("DONE");
    }
}
