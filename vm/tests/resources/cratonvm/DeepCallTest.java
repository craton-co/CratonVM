package cratonvm;

/**
 * Test deep call chains and tail-call elimination for Session 16.
 */
public class DeepCallTest {

    // Iterative fibonacci (no deep recursion, but exercises basic execution)
    public static int fibIterative(int n) {
        if (n <= 1) return n;
        int a = 0, b = 1;
        for (int i = 2; i <= n; i++) {
            int c = a + b;
            a = b;
            b = c;
        }
        return b;
    }

    // Deep static recursion — counts down to 0
    public static int deepCountdown(int n) {
        if (n <= 0) return 0;
        return deepCountdown(n - 1) + 1;
    }

    // Tail-recursive static method — should be optimized by TCE
    public static int tailSum(int n, int acc) {
        if (n <= 0) return acc;
        return tailSum(n - 1, acc + n);
    }

    // Mutual recursion (A calls B, B calls A)
    public static int isEvenOdd(int n) {
        return isEven(n) ? 1 : 0;
    }

    private static boolean isEven(int n) {
        if (n == 0) return true;
        return isOdd(n - 1);
    }

    private static boolean isOdd(int n) {
        if (n == 0) return false;
        return isEven(n - 1);
    }

    // Deep instance method recursion
    private int depth;

    public DeepCallTest(int d) {
        this.depth = d;
    }

    public int countDown() {
        if (depth <= 0) return 0;
        depth--;
        return countDown() + 1;
    }

    // Stack overflow test — should throw StackOverflowError
    public static int infiniteRecursion(int n) {
        return infiniteRecursion(n + 1);
    }

    // Entry points for the test harness
    public static void testFib50() {
        int result = fibIterative(50);
        // fib(50) = 1_258_626_902 (truncated to int)
        Util.tempPrint(result);
    }

    public static void testDeep1000() {
        int result = deepCountdown(1000);
        Util.tempPrint(result);
    }

    public static void testTailSum10000() {
        int result = tailSum(10000, 0);
        Util.tempPrint(result);
    }

    public static void testMutualRecursion() {
        int result = isEvenOdd(100);
        Util.tempPrint(result);
    }

    public static void testStackOverflow() {
        try {
            infiniteRecursion(0);
            Util.tempPrint(-1); // should not reach here
        } catch (StackOverflowError e) {
            Util.tempPrint(1); // caught StackOverflowError
        }
    }
}
