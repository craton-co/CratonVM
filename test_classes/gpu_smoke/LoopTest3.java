public class LoopTest3 {
    public static int sum(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) s += i;
        return s;
    }
    public static void main(String[] args) throws Exception {
        System.out.println("step1");
        int r0 = sum(4096);
        System.out.println("step2 r0=" + r0);
        for (int k = 0; k < 3; k++) { sum(4096); }
        System.out.println("step3");
        System.out.println("DONE");
    }
}
