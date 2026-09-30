package cratonvm;

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

/**
 * Session 4: MethodHandle and VarHandle completeness tests.
 */
public class MethodHandleTest {

    // --- Helper class for testing ---
    public static int staticField = 0;
    public int instanceField = 0;

    public static int staticAdd(int a, int b) {
        return a + b;
    }

    public int instanceMultiply(int x) {
        return x * instanceField;
    }

    public MethodHandleTest() {
        this.instanceField = 42;
    }

    public MethodHandleTest(int val) {
        this.instanceField = val;
    }

    // --- Static method invocation via MethodHandle ---
    public static int testStaticMethodHandle() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodHandle mh = lookup.findStatic(
                MethodHandleTest.class,
                "staticAdd",
                MethodType.methodType(int.class, int.class, int.class)
            );
            // invoke with 3 + 7 = 10
            Object result = mh.invoke(3, 7);
            if (result instanceof Integer) {
                return ((Integer) result).intValue() == 10 ? 1 : 0;
            }
            return 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    // --- Virtual method invocation via MethodHandle ---
    public static int testVirtualMethodHandle() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodHandle mh = lookup.findVirtual(
                MethodHandleTest.class,
                "instanceMultiply",
                MethodType.methodType(int.class, int.class)
            );
            MethodHandleTest obj = new MethodHandleTest(5);
            // 5 * 3 = 15
            Object result = mh.invoke(obj, 3);
            if (result instanceof Integer) {
                return ((Integer) result).intValue() == 15 ? 1 : 0;
            }
            return 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    // --- Constructor invocation via MethodHandle ---
    public static int testConstructorMethodHandle() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodHandle mh = lookup.findConstructor(
                MethodHandleTest.class,
                MethodType.methodType(void.class, int.class)
            );
            Object result = mh.invoke(99);
            if (result instanceof MethodHandleTest) {
                return ((MethodHandleTest) result).instanceField == 99 ? 1 : 0;
            }
            return 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    // --- MethodHandle.bindTo() for bound receiver ---
    public static int testBindTo() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodHandle mh = lookup.findVirtual(
                MethodHandleTest.class,
                "instanceMultiply",
                MethodType.methodType(int.class, int.class)
            );
            MethodHandleTest obj = new MethodHandleTest(7);
            MethodHandle bound = mh.bindTo(obj);
            // 7 * 4 = 28
            Object result = bound.invoke(4);
            if (result instanceof Integer) {
                return ((Integer) result).intValue() == 28 ? 1 : 0;
            }
            return 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    // --- Lookup.in(targetClass) creates a new Lookup ---
    public static int testLookupIn() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodHandles.Lookup restricted = lookup.in(String.class);
            // Should return a non-null Lookup
            return restricted != null ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    // --- VarHandle for instance field: get/set ---
    public static int testVarHandleGetSet() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            VarHandle vh = lookup.findVarHandle(
                MethodHandleTest.class,
                "instanceField",
                int.class
            );
            if (vh == null) return 0;

            MethodHandleTest obj = new MethodHandleTest(10);
            // get should return 10
            Object val = vh.get(obj);
            if (!(val instanceof Integer) || ((Integer)val).intValue() != 10) return 0;

            // set to 20
            vh.set(obj, 20);
            Object val2 = vh.get(obj);
            if (!(val2 instanceof Integer) || ((Integer)val2).intValue() != 20) return 0;

            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    // --- VarHandle compareAndSet ---
    public static int testVarHandleCAS() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            VarHandle vh = lookup.findVarHandle(
                MethodHandleTest.class,
                "instanceField",
                int.class
            );
            if (vh == null) return 0;

            MethodHandleTest obj = new MethodHandleTest(42);

            // CAS with wrong expected value should fail
            boolean result1 = vh.compareAndSet(obj, 99, 100);
            if (result1) return 0;  // should have failed

            // CAS with correct expected value should succeed
            boolean result2 = vh.compareAndSet(obj, 42, 100);
            if (!result2) return 0;  // should have succeeded

            // Verify the new value
            Object val = vh.get(obj);
            if (!(val instanceof Integer) || ((Integer)val).intValue() != 100) return 0;

            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    // --- MethodHandle.type() returns a valid MethodType ---
    public static int testMethodHandleType() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodType expected = MethodType.methodType(int.class, int.class, int.class);
            MethodHandle mh = lookup.findStatic(
                MethodHandleTest.class,
                "staticAdd",
                expected
            );
            MethodType actual = mh.type();
            if (actual == null) return 0;
            // Should have 2 parameters
            if (actual.parameterCount() != 2) return 0;
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    // --- WP1.6 acceptance: MethodHandle.invokeExact strict-arity round-trip ---
    //
    // `invokeExact` is signature-polymorphic — javac emits an `invokevirtual`
    // call site whose descriptor matches the bound handle's `MethodType`
    // exactly (here `(I)I`). This is distinct from `invoke` which boxes/
    // unboxes on demand. The acceptance criterion in roadmap §4 WP1.6
    // includes "MethodHandles.Lookup.findVirtual + .bindTo + .invokeExact
    // round-trips" — the existing testBindTo only exercises the loose
    // `invoke` path, so this method closes the gap.
    public static int testInvokeExactRoundTrip() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodHandle mh = lookup.findVirtual(
                MethodHandleTest.class,
                "instanceMultiply",
                MethodType.methodType(int.class, int.class)
            );
            MethodHandleTest obj = new MethodHandleTest(6);
            MethodHandle bound = mh.bindTo(obj);
            // Strict-arity: bound expects (int)int after binding receiver.
            // 6 * 7 = 42.
            int result = (int) bound.invokeExact(7);
            return result == 42 ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    // --- WP1.6 acceptance: VarHandle.getAcquire / setRelease round-trip ---
    //
    // The acceptance criterion in roadmap §4 WP1.6 includes "VarHandle.
    // acquire/release on volatile int field of a regular class". Our
    // existing testInstanceVarHandle only exercises plain get/set; this
    // method drives the acquire/release access mode on the same field.
    // The access mode is selected by VarHandle method name, not by a
    // field flag — both modes should round-trip the same value.
    public static int testVarHandleAcquireRelease() {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            VarHandle vh = lookup.findVarHandle(
                MethodHandleTest.class, "instanceField", int.class);
            MethodHandleTest obj = new MethodHandleTest(7);
            vh.setRelease(obj, 11);
            Object got = vh.getAcquire(obj);
            return (got instanceof Integer
                    && ((Integer) got).intValue() == 11) ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }
}
