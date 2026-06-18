// Validates the IR-path div-by-zero deopt end-to-end:
//   - div() is hot-called so it JIT-compiles via the IR path (the div guard).
//   - div(100, 0) must throw ArithmeticException (deopt -> resume/re-run ->
//     re-execute idiv), NOT SIGFPE-crash (the latent bug the guard fixes).
public class DivDeopt {
    static int div(int a, int b) { return a / b; }
    static int rem(int a, int b) { return a % b; }

    public static void main(String[] args) {
        long sum = 0;
        // Warm up so div()/rem() tier up to the JIT (IR path).
        for (int i = 0; i < 200000; i++) {
            int b = (i % 7) + 1;
            sum += div(100, b) + rem(100, b);
        }
        System.out.println("warmup sum=" + sum);

        boolean divOk = false, remOk = false;
        try {
            int r = div(100, 0);
            System.out.println("FAIL div: no exception, r=" + r);
        } catch (ArithmeticException e) {
            divOk = true;
            System.out.println("OK div ArithmeticException: " + e.getMessage());
        }
        try {
            int r = rem(100, 0);
            System.out.println("FAIL rem: no exception, r=" + r);
        } catch (ArithmeticException e) {
            remOk = true;
            System.out.println("OK rem ArithmeticException: " + e.getMessage());
        }
        System.out.println("RESULT div=" + divOk + " rem=" + remOk);
    }
}
