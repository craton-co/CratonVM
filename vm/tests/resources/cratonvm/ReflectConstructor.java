// JAVA21+
package cratonvm;

import java.lang.reflect.Constructor;

/**
 * Phase 88.3: Constructor.newInstance.
 */
public class ReflectConstructor {

    private int value;

    public ReflectConstructor() {
        this.value = 10;
    }

    public ReflectConstructor(int v) {
        this.value = v;
    }

    public int getValue() { return value; }

    // 88.3: No-arg constructor via reflection
    public static int testNoArgConstructor() throws Exception {
        Constructor<?> c = ReflectConstructor.class.getDeclaredConstructor();
        Object obj = c.newInstance();
        return ((ReflectConstructor) obj).getValue();  // 10
    }

    // 88.3: Parameterized constructor via reflection
    public static int testParamConstructor() throws Exception {
        Constructor<?> c = ReflectConstructor.class.getDeclaredConstructor(int.class);
        Object obj = c.newInstance(42);
        return ((ReflectConstructor) obj).getValue();  // 42
    }

    // 88.3: Exception in constructor
    public static int testExceptionInConstructor() {
        try {
            Constructor<?> c = ThrowingCtor.class.getDeclaredConstructor();
            c.newInstance();
            return -1;
        } catch (Exception e) {
            return 1;  // should catch InvocationTargetException
        }
    }

    static class ThrowingCtor {
        public ThrowingCtor() {
            throw new RuntimeException("ctor boom");
        }
    }
}
