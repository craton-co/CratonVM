package cratonvm;

/**
 * JVMS Chapter 5: Loading, Linking, and Initialization tests.
 *
 * Tests class loading, static initialization order, interface
 * initialization, array creation, and inheritance chains.
 */
public class TckLoading {

    static int initOrder = 0;

    // Helper class with static initializer
    static class A {
        static int aValue;
        static {
            aValue = 10;
        }
    }

    // Subclass that depends on parent being initialized first
    static class B extends A {
        static int bValue;
        static {
            bValue = aValue + 5;  // Should be 15 if A was initialized first
        }
    }

    // Interface with a constant
    interface Valued {
        int DEFAULT_VALUE = 42;  // interface constant
    }

    static class ValuedImpl implements Valued {
        public int getValue() {
            return DEFAULT_VALUE;
        }
    }

    // Deep inheritance chain
    static class Base {
        int level() { return 0; }
    }
    static class Mid extends Base {
        int level() { return 1; }
    }
    static class Leaf extends Mid {
        int level() { return 2; }
    }

    // 5.3: Class loading — user classes can be loaded and instantiated
    public static int testClassLoading() {
        A a = new A();
        if (A.aValue != 10) return 0;
        return 1;
    }

    // 5.5: Static initializers run before first active use
    public static int testStaticInit() {
        // Accessing B.bValue should trigger both A and B initialization
        int result = B.bValue;
        if (result != 15) return 0;
        if (A.aValue != 10) return 0;
        return 1;
    }

    // 5.5: Interface constants are accessible
    public static int testInterfaceInit() {
        ValuedImpl impl = new ValuedImpl();
        if (impl.getValue() != 42) return 0;
        if (Valued.DEFAULT_VALUE != 42) return 0;
        return 1;
    }

    // 5.3.3: Array class creation
    public static int testArrayCreation() {
        int[] intArr = new int[5];
        if (intArr.length != 5) return 0;
        intArr[0] = 99;
        if (intArr[0] != 99) return 0;

        String[] strArr = new String[3];
        if (strArr.length != 3) return 0;
        strArr[0] = "hello";
        if (strArr[0] == null) return 0;

        int[][] multiArr = new int[2][3];
        if (multiArr.length != 2) return 0;
        if (multiArr[0].length != 3) return 0;
        multiArr[1][2] = 77;
        if (multiArr[1][2] != 77) return 0;

        return 1;
    }

    // 5.4.3: Method resolution with inheritance
    public static int testInheritance() {
        Base base = new Base();
        Mid mid = new Mid();
        Leaf leaf = new Leaf();
        if (base.level() != 0) return 0;
        if (mid.level() != 1) return 0;
        if (leaf.level() != 2) return 0;

        // Polymorphism: parent reference to child object
        Base polyRef = new Leaf();
        if (polyRef.level() != 2) return 0;

        return 1;
    }
}
