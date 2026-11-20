package cratonvm;

/**
 * JVMS Chapter 6: JVM Instruction Set tests.
 *
 * Tests core bytecode instructions: arithmetic, comparisons,
 * switch statements, field operations, array operations,
 * method invocations, exception handling, and type checks.
 */
public class TckInstructions {

    int instanceField = 0;
    static int staticValue = 42;

    // 6.1: Integer arithmetic (iadd, isub, imul, idiv, irem, ineg)
    public static int testIntArithmetic() {
        int a = 10, b = 3;
        if (a + b != 13) return 0;
        if (a - b != 7) return 0;
        if (a * b != 30) return 0;
        if (a / b != 3) return 0;
        if (a % b != 1) return 0;
        if (-a != -10) return 0;
        // Overflow wrapping (two's complement)
        int max = Integer.MAX_VALUE;
        if (max + 1 != Integer.MIN_VALUE) return 0;
        return 1;
    }

    // 6.1: Long arithmetic (ladd, lsub, lmul, ldiv, lrem, lneg)
    public static int testLongArithmetic() {
        long a = 1000000000L, b = 2000000000L;
        long sum = a + b;
        if (sum != 3000000000L) return 0;
        if (a * 3L != 3000000000L) return 0;
        if (b / a != 2L) return 0;
        if (b % 3L != 2L) return 0;
        if (-a != -1000000000L) return 0;
        return 1;
    }

    // 6.1: Float arithmetic (fadd, fsub, fmul, fdiv, frem)
    public static int testFloatArithmetic() {
        float a = 3.0f, b = 2.0f;
        if (a + b != 5.0f) return 0;
        if (a - b != 1.0f) return 0;
        if (a * b != 6.0f) return 0;
        if (a / b != 1.5f) return 0;
        // NaN != NaN
        float nan = Float.NaN;
        if (nan == nan) return 0;
        return 1;
    }

    // 6.1: Comparison instructions (if_icmp*, lcmp, fcmp*)
    public static int testComparisons() {
        // Integer comparisons
        int a = 5, b = 10;
        if (a >= b) return 0;       // if_icmpge (should be false)
        if (!(a < b)) return 0;     // if_icmplt (should be true)
        if (a == b) return 0;       // if_icmpeq (should be false)
        if (!(a != b)) return 0;    // if_icmpne (should be true)
        if (!(a <= b)) return 0;    // if_icmple (should be true)

        // Long comparison
        long la = 100L, lb = 200L;
        if (la >= lb) return 0;

        // Null comparison
        Object obj = null;
        if (obj != null) return 0;
        obj = new Object();
        if (obj == null) return 0;

        return 1;
    }

    // 6.1: tableswitch instruction
    public static int testTableswitch() {
        int total = 0;
        for (int i = 0; i <= 3; i++) {
            switch (i) {
                case 0: total += 1; break;
                case 1: total += 10; break;
                case 2: total += 100; break;
                case 3: total += 1000; break;
                default: return 0;
            }
        }
        // 1 + 10 + 100 + 1000 = 1111
        if (total != 1111) return 0;
        return 1;
    }

    // 6.1: lookupswitch instruction (sparse switch)
    public static int testLookupswitch() {
        int val = classify(100);
        if (val != 1) return 0;
        val = classify(200);
        if (val != 2) return 0;
        val = classify(999);
        if (val != 3) return 0;
        val = classify(50);
        if (val != -1) return 0;
        return 1;
    }

    private static int classify(int x) {
        switch (x) {
            case 100: return 1;
            case 200: return 2;
            case 999: return 3;
            default: return -1;
        }
    }

    // 6.1: getfield, putfield instructions
    public static int testFieldOps() {
        TckInstructions obj = new TckInstructions();
        if (obj.instanceField != 0) return 0;
        obj.instanceField = 42;
        if (obj.instanceField != 42) return 0;

        // getstatic, putstatic
        if (TckInstructions.staticValue != 42) return 0;
        TckInstructions.staticValue = 99;
        if (TckInstructions.staticValue != 99) return 0;
        TckInstructions.staticValue = 42; // restore

        return 1;
    }

    // 6.1: iaload, iastore, arraylength, newarray, anewarray, multianewarray
    public static int testArrayOps() {
        // Primitive array
        int[] arr = new int[4];
        arr[0] = 10;
        arr[1] = 20;
        arr[2] = 30;
        arr[3] = 40;
        if (arr.length != 4) return 0;
        if (arr[0] + arr[1] + arr[2] + arr[3] != 100) return 0;

        // Object array
        String[] names = new String[2];
        names[0] = "foo";
        names[1] = "bar";
        if (names.length != 2) return 0;

        // Boolean/byte arrays
        boolean[] bools = new boolean[3];
        bools[0] = true;
        bools[1] = false;
        bools[2] = true;
        if (!bools[0]) return 0;
        if (bools[1]) return 0;

        return 1;
    }

    // 6.1: invokevirtual instruction
    public static int testInvokeVirtual() {
        TckInstructions obj = new TckInstructions();
        if (obj.add(3, 4) != 7) return 0;

        // Polymorphic dispatch
        Animal a = new Dog();
        if (a.sound() != 1) return 0;
        a = new Cat();
        if (a.sound() != 2) return 0;

        return 1;
    }

    public int add(int x, int y) {
        return x + y;
    }

    static class Animal {
        int sound() { return 0; }
    }
    static class Dog extends Animal {
        int sound() { return 1; }
    }
    static class Cat extends Animal {
        int sound() { return 2; }
    }

    // 6.1: invokestatic instruction
    public static int testInvokeStatic() {
        if (staticAdd(10, 20) != 30) return 0;
        if (factorial(5) != 120) return 0;  // recursive static
        return 1;
    }

    static int staticAdd(int a, int b) { return a + b; }
    static int factorial(int n) {
        if (n <= 1) return 1;
        return n * factorial(n - 1);
    }

    // 6.1: athrow, exception table handling
    public static int testExceptionHandling() {
        // Basic try-catch
        int result = 0;
        try {
            result = 1;
            throw new RuntimeException("test");
        } catch (RuntimeException e) {
            result += 10;
        }
        if (result != 11) return 0;

        // Finally block always executes
        int finResult = 0;
        try {
            finResult = 1;
        } finally {
            finResult += 100;
        }
        if (finResult != 101) return 0;

        // Nested try-catch
        int nested = 0;
        try {
            try {
                throw new IllegalArgumentException();
            } catch (IllegalArgumentException e) {
                nested = 1;
                throw new RuntimeException();
            }
        } catch (RuntimeException e) {
            nested += 10;
        }
        if (nested != 11) return 0;

        return 1;
    }

    // 6.1: checkcast instruction
    public static int testCheckcast() {
        Object obj = "hello";
        String s = (String) obj;
        if (s == null) return 0;

        // Null cast should succeed
        Object nullObj = null;
        String nullStr = (String) nullObj;
        if (nullStr != null) return 0;

        // Bad cast should throw ClassCastException
        try {
            Object intObj = Integer.valueOf(42);
            String bad = (String) intObj;
            return 0;  // should not reach here
        } catch (ClassCastException e) {
            // expected
        }

        return 1;
    }

    // 6.1: instanceof instruction
    public static int testInstanceof() {
        Object obj = "hello";
        if (!(obj instanceof String)) return 0;
        if (obj instanceof Integer) return 0;

        // null instanceof X is always false
        Object nullObj = null;
        if (nullObj instanceof String) return 0;

        // Subclass instanceof parent
        Dog dog = new Dog();
        if (!(dog instanceof Animal)) return 0;
        if (!(dog instanceof Object)) return 0;

        return 1;
    }
}
