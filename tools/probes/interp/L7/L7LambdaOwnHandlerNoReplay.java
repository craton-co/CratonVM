// Interpreter round i1 wave 4, lane L7 -- a lambda body WITH its own
// try/catch, called from compiled code through the lambda call site's direct
// arm (`try_lambda_site_direct_call`, vm/src/jit/helpers.rs).
//
// Before wave 4 an implicit exception (AIOOBE, `/ by zero`) raised in such a
// compiled body was read as a DEOPT: the site was latched off the direct arm
// and the generic path re-ran the body FROM ENTRY, so the `sideEffects++`
// before the `try` ran twice for that call. Now it is routed through the
// body's own handler (`lambda_direct_route_implicit`).
//
// Expected (HotSpot 25, `java L7LambdaOwnHandlerNoReplay`; values computed from
// a model of the loop), exactly:
//
//   sideEffects=8550000
//   checksum=4231874702894575976
//
// Compare with `cratonvm L7LambdaOwnHandlerNoReplay` (default flags) and with
// --nojit. Timing goes to stderr only.

import java.util.function.IntUnaryOperator;

public class L7LambdaOwnHandlerNoReplay {
    static int sideEffects;
    static long checksum;

    public static void main(String[] args) {
        long t0 = System.nanoTime();
        int[] data = {1, 2, 3};
        IntUnaryOperator lam = x -> {
            sideEffects++;
            try {
                // AIOOBE when x % 4 == 3, else `/ by zero` when x % 3 == 0.
                return data[x % 4] / (x % 3);
            } catch (ArithmeticException e) {
                sideEffects += 10;
                return -1;
            } catch (ArrayIndexOutOfBoundsException e) {
                sideEffects += 100;
                return -2;
            }
        };
        for (int i = 0; i < 300_000; i++) {
            checksum = checksum * 31 + lam.applyAsInt(i);
        }
        System.out.println("sideEffects=" + sideEffects);
        System.out.println("checksum=" + checksum);
        System.err.println("elapsed_ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
