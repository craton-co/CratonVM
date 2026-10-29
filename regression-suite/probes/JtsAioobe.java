// R20PNatFastThrow's aioobeKernel alone: does the fast throw hand out at an AIOOBE site?
public class JtsAioobe {
    static final int[] ARR = new int[8];
    static long aioobeKernel(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                acc += ARR[i % 10];
            } catch (ArrayIndexOutOfBoundsException e) {
                acc += 7;
            }
        }
        return acc;
    }
    public static void main(String[] a) {
        long acc = 0;
        for (int r = 0; r < 20; r++) acc += aioobeKernel(40_000);
        long t0 = System.nanoTime();
        acc += aioobeKernel(400_000);
        System.out.println("fact acc " + acc);
        System.out.println("time aioobe-caught " + (System.nanoTime() - t0) / 400_000 + " ns");
    }
}
