public class LoopTest15 {
    public static int touch() { return 0; }
    public static int sum2(Object a) {
        touch();
        return a.hashCode();
    }
    public static void main(String[] args) throws Exception {
        Object a = new Object();
        System.out.println("step1");
        int r0 = sum2(a);
        System.out.println("step2 r0=" + r0);
        for (int k = 0; k < 3000; k++) { sum2(a); }
        System.out.println("step3");
        System.out.println("DONE");
    }
}
