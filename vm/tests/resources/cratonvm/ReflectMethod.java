// JAVA21+
package cratonvm;

import java.lang.reflect.Method;

/**
 * Phase 88.1: Method.invoke on user bytecode.
 */
public class ReflectMethod {

    // Target methods for reflection tests
    public static int staticAdd(int a, int b) { return a + b; }
    public int instanceDouble(int x) { return x * 2; }
    public static String greet(String name) { return "Hello " + name; }
    private static int secret() { return 777; }
    public static int throwingMethod() { throw new RuntimeException("boom"); }

    // 88.1: Invoke a static method via reflection
    public static int testStaticMethod() throws Exception {
        Method m = ReflectMethod.class.getDeclaredMethod("staticAdd", int.class, int.class);
        Object result = m.invoke(null, 10, 20);
        return (Integer) result;  // 30
    }

    // 88.1: Invoke an instance method via reflection
    public static int testInstanceMethod() throws Exception {
        ReflectMethod obj = new ReflectMethod();
        Method m = ReflectMethod.class.getDeclaredMethod("instanceDouble", int.class);
        Object result = m.invoke(obj, 21);
        return (Integer) result;  // 42
    }

    // 88.1: Invoke a method that returns String
    public static int testStringReturn() throws Exception {
        Method m = ReflectMethod.class.getDeclaredMethod("greet", String.class);
        Object result = m.invoke(null, "World");
        return "Hello World".equals(result) ? 1 : 0;  // 1
    }

    // 88.1: Private method with setAccessible
    public static int testPrivateSetAccessible() throws Exception {
        Method m = ReflectMethod.class.getDeclaredMethod("secret");
        m.setAccessible(true);
        Object result = m.invoke(null);
        return (Integer) result;  // 777
    }

    // 88.1: Exception wrapping — exceptions from invoked method
    public static int testExceptionWrapping() {
        try {
            Method m = ReflectMethod.class.getDeclaredMethod("throwingMethod");
            m.invoke(null);
            return -1;  // should not reach here
        } catch (Exception e) {
            // InvocationTargetException wrapping RuntimeException
            return 1;
        }
    }
}
