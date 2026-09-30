package cratonvm;

/**
 * Session 39: JDWP Debugger Protocol — test class.
 *
 * Each static method returns an int so the test harness can verify execution.
 * The methods exercise code paths relevant to debugger inspection:
 * local variables, control flow, loops, arithmetic, etc.
 */
public class JdwpComplete {

    // --- Local variable inspection targets ---

    /** Simple method with a few int locals. */
    public static int locals_int() {
        int a = 10;
        int b = 20;
        int c = a + b;
        return c; // 30
    }

    /** Long locals. */
    public static int locals_long() {
        long x = 100000L;
        long y = 200000L;
        long sum = x + y;
        return (int)(sum / 100000L); // 3
    }

    /** Float locals. */
    public static int locals_float() {
        float a = 1.5f;
        float b = 2.5f;
        float c = a + b;
        return (int) c; // 4
    }

    /** Double locals. */
    public static int locals_double() {
        double a = 10.5;
        double b = 20.5;
        double c = a + b;
        return (int) c; // 31
    }

    /** Mixed-type locals. */
    public static int locals_mixed() {
        int i = 5;
        long l = 10L;
        float f = 2.0f;
        double d = 3.0;
        return (int)(i + l + (long)f + (long)d); // 20
    }

    // --- Control flow ---

    /** If-else with locals. */
    public static int control_if_else() {
        int x = 42;
        int result;
        if (x > 40) {
            result = 1;
        } else {
            result = 0;
        }
        return result; // 1
    }

    /** Switch statement. */
    public static int control_switch() {
        int val = 2;
        int result;
        switch (val) {
            case 1: result = 10; break;
            case 2: result = 20; break;
            case 3: result = 30; break;
            default: result = -1;
        }
        return result; // 20
    }

    /** For loop with accumulator. */
    public static int control_for_loop() {
        int sum = 0;
        for (int i = 1; i <= 10; i++) {
            sum += i;
        }
        return sum; // 55
    }

    /** While loop. */
    public static int control_while() {
        int n = 100;
        int count = 0;
        while (n > 1) {
            n /= 2;
            count++;
        }
        return count; // 6 (100->50->25->12->6->3->1)
    }

    /** Nested loops. */
    public static int control_nested() {
        int total = 0;
        for (int i = 0; i < 5; i++) {
            for (int j = 0; j < 5; j++) {
                total++;
            }
        }
        return total; // 25
    }

    // --- Array access ---

    /** Array creation and access. */
    public static int array_basic() {
        int[] arr = new int[5];
        for (int i = 0; i < 5; i++) {
            arr[i] = i * 10;
        }
        return arr[3]; // 30
    }

    /** Array sum. */
    public static int array_sum() {
        int[] arr = {1, 2, 3, 4, 5};
        int sum = 0;
        for (int i = 0; i < arr.length; i++) {
            sum += arr[i];
        }
        return sum; // 15
    }

    // --- Object interaction ---

    /** String length. */
    public static int object_string_len() {
        String s = "Hello, JDWP!";
        return s.length(); // 12
    }

    /** String concatenation (uses StringBuilder internally). */
    public static int object_string_concat() {
        String a = "Hello";
        String b = " World";
        String c = a + b;
        return c.length(); // 11
    }

    /** Null reference local. */
    public static int object_null_ref() {
        Object obj = null;
        if (obj == null) {
            return 1;
        }
        return 0; // 1
    }

    // --- Arithmetic ---

    /** Bitwise operations. */
    public static int arith_bitwise() {
        int a = 0xFF;
        int b = 0x0F;
        int and = a & b;    // 0x0F = 15
        int or  = a | b;    // 0xFF = 255
        int xor = a ^ b;    // 0xF0 = 240
        return and + or + xor; // 510
    }

    /** Shift operations. */
    public static int arith_shifts() {
        int val = 1;
        val = val << 10; // 1024
        val = val >> 2;  // 256
        return val; // 256
    }

    /** Integer division and modulo. */
    public static int arith_divmod() {
        int a = 100;
        int b = 7;
        int div = a / b;  // 14
        int mod_ = a % b; // 2
        return div * 10 + mod_; // 142
    }

    // --- Method calls ---

    private static int helper(int x) {
        return x * 2;
    }

    /** Call a helper method from a loop. */
    public static int method_call_loop() {
        int sum = 0;
        for (int i = 1; i <= 5; i++) {
            sum += helper(i);
        }
        return sum; // 2+4+6+8+10 = 30
    }

    /** Recursive method. */
    private static int fib(int n) {
        if (n <= 1) return n;
        return fib(n - 1) + fib(n - 2);
    }

    public static int method_recursive() {
        return fib(10); // 55
    }

    // --- Exception handling ---

    /** Try-catch with explicit throw. */
    public static int exception_trycatch() {
        int result = 0;
        try {
            throw new RuntimeException("test");
        } catch (RuntimeException e) {
            result = 42;
        }
        return result; // 42
    }

    /** Try-finally. */
    public static int exception_finally() {
        int val = 0;
        try {
            val = 10;
        } finally {
            val += 5;
        }
        return val; // 15
    }

    // --- Ternary / conditional expressions ---

    /** Ternary with multiple locals. */
    public static int ternary_expr() {
        int a = 10;
        int b = 20;
        int max = (a > b) ? a : b;
        int min = (a < b) ? a : b;
        return max - min; // 10
    }

    // --- Stack depth / many locals ---

    /** Method with many local variables (tests frame local snapshot). */
    public static int many_locals() {
        int a = 1, b = 2, c = 3, d = 4, e = 5;
        int f = 6, g = 7, h = 8, i = 9, j = 10;
        return a + b + c + d + e + f + g + h + i + j; // 55
    }

    /** Deep call stack. */
    private static int deepCall(int depth) {
        if (depth <= 0) return 1;
        return deepCall(depth - 1) + 1;
    }

    public static int deep_stack() {
        return deepCall(20); // 21
    }
}
