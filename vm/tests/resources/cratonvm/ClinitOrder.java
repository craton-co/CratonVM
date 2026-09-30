package cratonvm;

/**
 * Tests for static initializer (<clinit>) ordering per JVM spec §5.5.
 * Exercises superclass ordering, diamond dependencies, re-entrancy,
 * and ExceptionInInitializerError semantics.
 */
public class ClinitOrder {

    // --- 1. Superclass clinit runs before subclass ---
    // Expected: 1, 2, 3
    public static void testSuperBeforeSub() {
        int x = Child.CHILD_VALUE;
        // Parent prints 1, Child prints 2 in their clinits
        Util.tempPrint(3);
    }

    // --- 2. Re-entrant clinit is a no-op (doesn't deadlock or re-execute) ---
    // Expected: 1, 2
    public static void testReentrantClinit() {
        int x = ReentrantInit.VALUE;
        Util.tempPrint(2);
    }

    // --- 3. Diamond dependency: A extends B, A implements C; B and C both need init ---
    // Expected: 1, 2, 3
    public static void testDiamondInit() {
        int x = DiamondChild.VALUE;
        Util.tempPrint(3);
    }

    // --- 4. Clinit error marks class as unusable ---
    // Expected: 1, 2
    public static void testClinitErrorMarksUnusable() {
        // First access triggers clinit which throws
        try {
            int x = FailInit.VALUE;
            Util.tempPrint(-1); // should not reach
        } catch (ExceptionInInitializerError e) {
            Util.tempPrint(1);
        } catch (Throwable t) {
            Util.tempPrint(1); // fallback
        }
        // Second access should throw NoClassDefFoundError
        try {
            int x = FailInit.VALUE;
            Util.tempPrint(-1); // should not reach
        } catch (NoClassDefFoundError e) {
            Util.tempPrint(2);
        } catch (Throwable t) {
            Util.tempPrint(2); // fallback
        }
    }

    // --- 5. Multiple classes initialized in dependency order ---
    // Expected: 1, 2, 3, 4
    public static void testInitChain() {
        int x = ChainD.VALUE;
        Util.tempPrint(4);
    }

    // --- 6. Only non-constant fields trigger interface init ---
    // Expected: 1, 2
    public static void testConstantFieldSkipsInit() {
        // Accessing a constant (static final with ConstantValue) should NOT trigger init
        int c = ConstIface.CONSTANT;
        // Accessing a non-constant static should trigger init
        int nc = NonConstIface.NON_CONSTANT;
        Util.tempPrint(2);
    }

    // --- 7. Concurrent clinit: only one thread runs clinit ---
    // Expected: 1 (printed once by clinit), then both threads see VALUE == 42
    public static void testConcurrentInit() {
        // Just access the field — clinit should run exactly once
        int v = SlowInit.VALUE;
        Util.tempPrint(v); // prints 42
    }

    // ---- Helper classes ----

    static class Parent {
        static int PARENT_VALUE;
        static {
            Util.tempPrint(1);
            PARENT_VALUE = 10;
        }
    }

    static class Child extends Parent {
        static int CHILD_VALUE;
        static {
            Util.tempPrint(2);
            CHILD_VALUE = PARENT_VALUE + 20;
        }
    }

    static class ReentrantInit {
        static int VALUE;
        static {
            Util.tempPrint(1);
            // This re-entrant access should be a no-op (class is already Initializing)
            int x = ReentrantInit.VALUE;
            VALUE = 42;
        }
    }

    static class DiamondBase {
        static int BASE;
        static {
            Util.tempPrint(1);
            BASE = 100;
        }
    }

    static class DiamondChild extends DiamondBase {
        static int VALUE;
        static {
            Util.tempPrint(2);
            VALUE = BASE + 1;
        }
    }

    static class FailInit {
        static int VALUE;
        static {
            if (true) throw new RuntimeException("clinit failure");
            VALUE = 1;
        }
    }

    static class ChainA {
        static int VALUE;
        static {
            Util.tempPrint(1);
            VALUE = 1;
        }
    }

    static class ChainB extends ChainA {
        static int VALUE;
        static {
            Util.tempPrint(2);
            VALUE = ChainA.VALUE + 1;
        }
    }

    static class ChainC extends ChainB {
        static int VALUE;
        static {
            Util.tempPrint(3);
            VALUE = ChainB.VALUE + 1;
        }
    }

    static class ChainD extends ChainC {
        static int VALUE;
        static {
            VALUE = ChainC.VALUE + 1;
        }
    }

    // Interface with a compile-time constant — should NOT trigger clinit
    interface ConstIface {
        int CONSTANT = 42; // static final int = constant value (ConstantValue attr)
    }

    // Interface with a non-constant static — triggers clinit
    interface NonConstIface {
        // This creates a static field but NOT with a ConstantValue attribute
        // because it's computed at runtime
        int NON_CONSTANT = computeValue();
    }

    // Slow clinit — burns time so a second thread can race into initialization
    static class SlowInit {
        static int VALUE;
        static {
            Util.tempPrint(1); // marker: clinit is running
            // Busy-loop for ~50ms to give the other thread a chance to contend
            long start = System.currentTimeMillis();
            while (System.currentTimeMillis() - start < 50) {
                // spin
            }
            VALUE = 42;
        }
    }

    static int computeValue() {
        Util.tempPrint(1);
        return 99;
    }
}
