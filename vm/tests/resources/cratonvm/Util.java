package cratonvm;

/**
 * Test utility class with native methods for capturing output.
 * The VM has native implementations registered for these methods.
 */
public class Util {
    public static native void tempPrint(int value);
    public static native void tempPrint(String value);
}
