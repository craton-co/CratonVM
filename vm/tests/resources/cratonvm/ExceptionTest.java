package cratonvm;

/**
 * Test exception throwing and catching.
 * This uses the tempPrint utility to report test results.
 */
public class ExceptionTest {
    /**
     * Basic try-catch: catch a manually thrown exception.
     * Expected printed values: [1, 2]
     */
    public static void testBasicCatch() {
        try {
            Util.tempPrint(1);
            throw new RuntimeException();
        } catch (RuntimeException e) {
            Util.tempPrint(2);
        }
    }

    /**
     * Exception propagation: exception thrown in callee is caught by caller.
     * Expected printed values: [1, 3]
     */
    public static void testPropagation() {
        try {
            Util.tempPrint(1);
            throwException();
            Util.tempPrint(2);  // should not execute
        } catch (RuntimeException e) {
            Util.tempPrint(3);
        }
    }

    private static void throwException() {
        throw new RuntimeException();
    }

    /**
     * Finally block: always executes.
     * Expected printed values: [1, 2, 3]
     */
    public static void testFinally() {
        try {
            Util.tempPrint(1);
            Util.tempPrint(2);
        } finally {
            Util.tempPrint(3);
        }
    }
}
