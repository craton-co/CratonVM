public class LoopTest12 {
    public static int touch(int x) { return x; }
    public static int sum2(int n) {
        touch(n);  // invoke forces non-IR
        int x = 0;
        int y = 0;
        int s = 0;
        for (int i = 0; i < n; i++) s += i;
        return s + x + y;
    }
    public static void main(String[] args) throws Exception {
        System.out.println("step1");
        int r0 = sum2(100);
        System.out.println("step2 r0=" + r0);
        for (int k = 0; k < 3000; k++) { sum2(100); }
        System.out.println("step3");
        System.out.println("DONE");
    }
}
