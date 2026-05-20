package cratonvm;

/**
 * Advanced exception handling tests for the JVM interpreter.
 * Each test method exercises a different aspect of exception semantics.
 */
public class ExceptionAdvanced {

    /**
     * Nested try/catch: inner catch handles exception, outer code continues.
     * Expected: tempPrint 1, 2, 3
     */
    public static void testNestedTryCatch() {
        Util.tempPrint(1);
        try {
            try {
                int x = 1 / 0;
            } catch (ArithmeticException e) {
                Util.tempPrint(2);
            }
            Util.tempPrint(3);
        } catch (Exception e) {
            Util.tempPrint(-1); // should not reach
        }
    }

    /**
     * Exception thrown inside a catch handler.
     * Expected: tempPrint 1, 2
     */
    public static void testExceptionInCatch() {
        try {
            try {
                throw new RuntimeException("first");
            } catch (RuntimeException e) {
                Util.tempPrint(1);
                throw new RuntimeException("second");
            }
        } catch (RuntimeException e) {
            Util.tempPrint(2);
        }
    }

    /**
     * Finally block executes even with a return statement.
     * Expected: tempPrint 1, 2 and returns 42
     */
    public static int testFinallyWithReturn() {
        try {
            Util.tempPrint(1);
            return 42;
        } finally {
            Util.tempPrint(2);
        }
    }

    /**
     * Catch different exception types in sequence.
     * Expected: tempPrint 1, 2, 3
     */
    public static void testMultiCatch() {
        // First: ArithmeticException
        try {
            int x = 1 / 0;
        } catch (ArithmeticException e) {
            Util.tempPrint(1);
        }

        // Second: ArrayIndexOutOfBoundsException
        try {
            int[] arr = new int[1];
            int x = arr[5];
        } catch (ArrayIndexOutOfBoundsException e) {
            Util.tempPrint(2);
        }

        // Third: NullPointerException
        try {
            String s = null;
            s.length();
        } catch (NullPointerException e) {
            Util.tempPrint(3);
        }
    }

    /**
     * Catch and re-throw an exception.
     * Expected: tempPrint 1, 2
     */
    public static void testRethrow() {
        try {
            try {
                throw new RuntimeException("original");
            } catch (RuntimeException e) {
                Util.tempPrint(1);
                throw e; // re-throw
            }
        } catch (RuntimeException e) {
            Util.tempPrint(2);
        }
    }

    /**
     * Exception propagates up through multiple stack frames.
     * Expected: tempPrint 1, 2
     */
    public static void testStackUnwinding() {
        try {
            level1();
        } catch (RuntimeException e) {
            Util.tempPrint(2);
        }
    }

    private static void level1() {
        level2();
    }

    private static void level2() {
        Util.tempPrint(1);
        throw new RuntimeException("deep");
    }
}
