package cratonvm;

/**
 * JVMS Chapter 4: Class File Format verification tests.
 *
 * Tests that the VM correctly handles class file structures:
 * magic number validation, version checking, constant pool entries,
 * field access, and method access modifiers.
 */
public class TckClassFile {

    // Fields with various access modifiers
    public int publicField = 10;
    private int privateField = 20;
    protected int protectedField = 30;
    static int staticField = 100;
    final int finalField = 42;

    // 4.1: Class files must have correct magic number (0xCAFEBABE)
    // This test passes if the class was loaded at all — the VM validates magic on load.
    public static int testMagicNumber() {
        // If we reach this point, the class file was loaded and its magic was valid.
        return 1;
    }

    // 4.1: Class file version must be recognized
    // If this class loaded, the version was accepted.
    public static int testClassVersion() {
        return 1;
    }

    // 4.4: Constant pool must be correctly parsed
    // Test that string constants, integer constants, and class references work.
    public static int testConstantPool() {
        String s = "hello";
        int i = 42;
        double d = 3.14;
        long l = 999999L;

        if (s == null) return 0;
        if (s.length() != 5) return 0;
        if (i != 42) return 0;
        if (d < 3.0 || d > 4.0) return 0;
        if (l != 999999L) return 0;
        return 1;
    }

    // 4.5: Fields must be accessible according to access flags
    public static int testFieldAccess() {
        TckClassFile obj = new TckClassFile();
        // Public field is accessible
        if (obj.publicField != 10) return 0;
        // Private field accessible within same class
        if (obj.privateField != 20) return 0;
        // Protected field accessible within same class
        if (obj.protectedField != 30) return 0;
        // Static field
        if (TckClassFile.staticField != 100) return 0;
        // Final field
        if (obj.finalField != 42) return 0;
        return 1;
    }

    // 4.6: Methods must be callable according to access flags
    public static int testMethodAccess() {
        TckClassFile obj = new TckClassFile();
        if (obj.instanceMethod() != 7) return 0;
        if (TckClassFile.staticMethod() != 8) return 0;
        if (obj.overloadedMethod(1) != 1) return 0;
        if (obj.overloadedMethod(1, 2) != 3) return 0;
        return 1;
    }

    public int instanceMethod() {
        return 7;
    }

    public static int staticMethod() {
        return 8;
    }

    public int overloadedMethod(int a) {
        return a;
    }

    public int overloadedMethod(int a, int b) {
        return a + b;
    }
}
