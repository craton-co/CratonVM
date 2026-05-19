public class LoopTest9 {
    static int dummy = 0;
    public static int sum(int n) {
        dummy = 1;  // force non-IR
        int s = 0;
        for (int i = 0; i < n; i++) s += i;
        return s;
    }
    public static void main(String[] args) throws Exception {
        System.out.println("step1");
        int r0 = sum(100);
        System.out.println("step2 r0=" + r0);
        for (int k = 0; k < 3000; k++) { sum(100); }
        System.out.println("step3");
        System.out.println("DONE");
    }
}
