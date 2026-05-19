public class LoopTest14 {
    public static int touch() { return 0; }  // no args
    public static int sum2(int[] a) {
        touch();
        return a.length;
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
