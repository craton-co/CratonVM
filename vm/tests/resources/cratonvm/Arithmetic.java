package cratonvm;

/**
 * Test basic arithmetic operations.
 * Expected: test() returns 30.
 */
public class Arithmetic {
    public static int test() {
        int a = 10;
        int b = 20;
        int c = a + b;   // 30
        return c;
    }

    public static int testMul() {
        int x = 6;
        int y = 7;
        return x * y;  // 42
    }

    public static int testDiv() {
        return 100 / 4;  // 25
    }

    public static int testMod() {
        return 17 % 5;  // 2
    }

    public static int testNeg() {
        int x = 42;
        return -x;  // -42
    }
}
