// JAVA21+
package cratonvm;

import java.lang.reflect.Method;
import java.lang.reflect.Field;
import java.lang.reflect.Constructor;
import java.lang.reflect.Proxy;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Modifier;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Session 17: Reflection Completeness.
 * Full java.lang.reflect support for frameworks.
 */
public class ReflectionComplete {

    // ---- Target classes for reflection tests ----

    private int privateField = 42;
    protected String protectedField = "secret";
    public static int counter = 0;
    public final double PI = 3.14;
    private final AtomicReference<Integer> privateFinalReference = new AtomicReference<Integer>(42);

    private static int privateStaticMethod(int x) { return x * x; }
    public int instanceMethod(int a, int b) { return a + b; }
    static String varargsMethod(String... parts) {
        StringBuilder sb = new StringBuilder();
        for (String p : parts) sb.append(p);
        return sb.toString();
    }

    // ---- Test 1: Method.invoke() with access checks and type coercion ----

    // `Method.invoke` checks access against the CALLING class, and the caller
    // here is ReflectionComplete itself — the very class that declares
    // `privateStaticMethod`. A class may always reflect on its own private
    // members, so the first `invoke` SUCCEEDS with no `setAccessible(true)`;
    // there is no IllegalAccessException to catch.
    //
    // This method used to `return -1` on that success path and expect 49. Real
    // JDK 25 returns -1 (measured: `probes/CorpusOracle`, and a standalone
    // probe of the same shape on Temurin 25.0.4). The rule, and this exact
    // correction, are already documented on `testFieldGetPrivate` two methods
    // below for the field form of the same question — the method form was
    // simply left behind, and passed only for as long as CratonVM's own
    // `Method.invoke` denied it too. It stopped passing when dev made that
    // check caller-sensitive (`caller_may_access_member`).
    //
    // Assert what the JDK actually guarantees: own-private invoke works, and
    // `setAccessible(true)` is a no-op on top of it.
    public static int testMethodInvokePrivateViaReflection() throws Exception {
        Method m = ReflectionComplete.class.getDeclaredMethod("privateStaticMethod", int.class);
        int withoutSetAccessible = (Integer) m.invoke(null, 7);
        m.setAccessible(true);
        int withSetAccessible = (Integer) m.invoke(null, 7);
        if (withoutSetAccessible != withSetAccessible) {
            return -1;
        }
        return withSetAccessible; // 49
    }

    public static int testMethodInvokeInstance() throws Exception {
        ReflectionComplete obj = new ReflectionComplete();
        Method m = ReflectionComplete.class.getDeclaredMethod("instanceMethod", int.class, int.class);
        Object result = m.invoke(obj, 15, 27);
        return (Integer) result; // 42
    }

    public static int testMethodInvokeTypeCoercion() throws Exception {
        // Invoke with boxed Integer args — should auto-unbox
        Method m = ReflectionComplete.class.getDeclaredMethod("privateStaticMethod", int.class);
        m.setAccessible(true);
        Integer boxed = 6;
        Object result = m.invoke(null, boxed);
        return (Integer) result; // 36
    }

    // ---- Test 2: Field.get()/set() with accessibility override ----

    // `Field.get` checks access against the CALLING class, and the caller here
    // is ReflectionComplete itself — the very class that declares
    // `privateField`. A class may always reflect on its own private members,
    // so `f.get(obj)` SUCCEEDS with no `setAccessible(true)`; there is no
    // IllegalAccessException to catch.
    //
    // This method used to `return -1` on that success path and expect 42. Real
    // JDK 25 returns -1, so the expectation was wrong — and the very next test
    // in this file already documents the correct rule for the same shape.
    // Assert what the JDK actually guarantees: own-private access works, and
    // `setAccessible(true)` is a no-op on top of it.
    public static int testFieldGetPrivate() throws Exception {
        ReflectionComplete obj = new ReflectionComplete();
        Field f = ReflectionComplete.class.getDeclaredField("privateField");
        int withoutSetAccessible = (Integer) f.get(obj);
        f.setAccessible(true);
        int withSetAccessible = (Integer) f.get(obj);
        if (withoutSetAccessible != withSetAccessible) {
            return -1;
        }
        return withSetAccessible; // 42
    }

    // A declaring class may reflectively read its own private-final field
    // without setAccessible(true). HikariConfig.copyStateTo uses this shape
    // for its private-final AtomicReference credentials field.
    //
    // STATIC, and it builds its own receiver, because every corpus fixture is
    // called as `vm.invoke(class, method, "()I", &[])` — no receiver argument
    // exists to pass. Declared as an INSTANCE method this ran with `this ==
    // null`, so `f.get(this)` threw `NullPointerException: Field.get(…): null
    // receiver for instance field` — which is the correct answer to the
    // question that shape actually asks, and not the question the fixture
    // means to ask. `probes/CorpusOracle` hid the mismatch by constructing a
    // receiver of its own for non-static methods (`Modifier.isStatic` →
    // `getDeclaredConstructor().newInstance()`), so the oracle exercised the
    // reflective read while CratonVM was handed a null receiver, and the
    // difference was filed as a class-library gap. Access is unchanged by the
    // move: the calling class is still `ReflectionComplete`, which is what
    // `Field.get`'s access check reads.
    public static int testFieldGetOwnPrivateFinalReferenceWithoutSetAccessible() throws Exception {
        ReflectionComplete self = new ReflectionComplete();
        Field f = ReflectionComplete.class.getDeclaredField("privateFinalReference");
        AtomicReference<?> value = (AtomicReference<?>) f.get(self);
        return (Integer) value.get();
    }

    public static int testFieldSetPrivate() throws Exception {
        ReflectionComplete obj = new ReflectionComplete();
        Field f = ReflectionComplete.class.getDeclaredField("privateField");
        f.setAccessible(true);
        f.set(obj, 999);
        return obj.privateField; // 999
    }

    public static int testFieldStaticGetSet() throws Exception {
        counter = 0;
        Field f = ReflectionComplete.class.getDeclaredField("counter");
        f.set(null, 42);
        int val = (Integer) f.get(null);
        f.set(null, 0); // Reset
        return val; // 42
    }

    // ---- Test 3: Constructor.newInstance() with varargs ----

    static class Pair {
        int a, b;
        Pair() { a = 0; b = 0; }
        Pair(int a, int b) { this.a = a; this.b = b; }
    }

    public static int testConstructorNoArg() throws Exception {
        Constructor<?> c = Pair.class.getDeclaredConstructor();
        Object obj = c.newInstance();
        Pair p = (Pair) obj;
        return p.a + p.b; // 0
    }

    public static int testConstructorWithArgs() throws Exception {
        Constructor<?> c = Pair.class.getDeclaredConstructor(int.class, int.class);
        Object obj = c.newInstance(10, 32);
        Pair p = (Pair) obj;
        return p.a + p.b; // 42
    }

    public static int testConstructorSetAccessible() throws Exception {
        // Pair constructors are package-private, but setAccessible should work
        Constructor<?> c = Pair.class.getDeclaredConstructor(int.class, int.class);
        c.setAccessible(true);
        Pair p = (Pair) c.newInstance(100, 200);
        return p.a + p.b; // 300
    }

    // ---- Test 4: Proxy.newProxyInstance() for dynamic proxy creation ----

    interface Calculator {
        int compute(int a, int b);
    }

    interface Greeter {
        String greet(String name);
    }

    public static int testProxyBasic() {
        Calculator calc = (Calculator) Proxy.newProxyInstance(
            Calculator.class.getClassLoader(),
            new Class<?>[] { Calculator.class },
            new InvocationHandler() {
                public Object invoke(Object proxy, Method method, Object[] args) {
                    if ("compute".equals(method.getName())) {
                        // Add the two arguments
                        int a = (Integer) args[0];
                        int b = (Integer) args[1];
                        return a + b;
                    }
                    return null;
                }
            }
        );
        return calc.compute(20, 22); // 42
    }

    public static int testProxyIsProxyClass() {
        Object proxy = Proxy.newProxyInstance(
            Greeter.class.getClassLoader(),
            new Class<?>[] { Greeter.class },
            (p, m, a) -> "Hi " + a[0]
        );
        return Proxy.isProxyClass(proxy.getClass()) ? 1 : 0; // 1
    }

    public static int testProxyGetHandler() {
        InvocationHandler handler = (p, m, a) -> 42;
        Object proxy = Proxy.newProxyInstance(
            Calculator.class.getClassLoader(),
            new Class<?>[] { Calculator.class },
            handler
        );
        InvocationHandler retrieved = Proxy.getInvocationHandler(proxy);
        return (retrieved == handler) ? 1 : 0; // 1
    }

    // ---- Test 5: Class.getDeclaredMethods/Fields/Constructors with proper filtering ----

    public static int testGetDeclaredMethods() throws Exception {
        Method[] methods = Pair.class.getDeclaredMethods();
        // Pair has no declared methods (only <init>s which should be filtered)
        return methods.length; // 0
    }

    public static int testGetDeclaredFields() throws Exception {
        Field[] fields = Pair.class.getDeclaredFields();
        // Pair has 2 declared fields: a, b
        return fields.length; // 2
    }

    public static int testGetDeclaredConstructors() throws Exception {
        Constructor<?>[] ctors = Pair.class.getDeclaredConstructors();
        // Pair has 2 constructors: () and (int, int)
        return ctors.length; // 2
    }

    public static int testGetDeclaredMethodByName() throws Exception {
        Method m = ReflectionComplete.class.getDeclaredMethod("instanceMethod", int.class, int.class);
        return m.getName().equals("instanceMethod") ? 1 : 0; // 1
    }

    public static int testMethodModifiers() throws Exception {
        Method priv = ReflectionComplete.class.getDeclaredMethod("privateStaticMethod", int.class);
        Method pub = ReflectionComplete.class.getDeclaredMethod("instanceMethod", int.class, int.class);
        boolean privIsPrivate = Modifier.isPrivate(priv.getModifiers());
        boolean privIsStatic = Modifier.isStatic(priv.getModifiers());
        boolean pubIsPublic = Modifier.isPublic(pub.getModifiers());
        boolean pubIsNotStatic = !Modifier.isStatic(pub.getModifiers());
        return (privIsPrivate && privIsStatic && pubIsPublic && pubIsNotStatic) ? 1 : 0; // 1
    }

    public static int testFieldModifiers() throws Exception {
        Field priv = ReflectionComplete.class.getDeclaredField("privateField");
        Field stat = ReflectionComplete.class.getDeclaredField("counter");
        Field fin = ReflectionComplete.class.getDeclaredField("PI");
        boolean privIsPrivate = Modifier.isPrivate(priv.getModifiers());
        boolean statIsStatic = Modifier.isStatic(stat.getModifiers());
        boolean finIsFinal = Modifier.isFinal(fin.getModifiers());
        return (privIsPrivate && statIsStatic && finIsFinal) ? 1 : 0; // 1
    }

    // ---- Test 6: Method/Field metadata ----

    public static int testMethodReturnType() throws Exception {
        Method m = ReflectionComplete.class.getDeclaredMethod("instanceMethod", int.class, int.class);
        Class<?> retType = m.getReturnType();
        return (retType == int.class) ? 1 : 0; // 1
    }

    public static int testMethodParameterTypes() throws Exception {
        Method m = ReflectionComplete.class.getDeclaredMethod("instanceMethod", int.class, int.class);
        Class<?>[] paramTypes = m.getParameterTypes();
        return (paramTypes.length == 2 && paramTypes[0] == int.class && paramTypes[1] == int.class) ? 1 : 0; // 1
    }

    public static int testMethodParameterCount() throws Exception {
        Method m = ReflectionComplete.class.getDeclaredMethod("instanceMethod", int.class, int.class);
        return m.getParameterCount(); // 2
    }

    public static int testFieldType() throws Exception {
        Field f = ReflectionComplete.class.getDeclaredField("privateField");
        return (f.getType() == int.class) ? 1 : 0; // 1
    }

    public static int testFieldDeclaringClass() throws Exception {
        Field f = ReflectionComplete.class.getDeclaredField("privateField");
        return (f.getDeclaringClass() == ReflectionComplete.class) ? 1 : 0; // 1
    }

    public static int testMethodDeclaringClass() throws Exception {
        Method m = ReflectionComplete.class.getDeclaredMethod("instanceMethod", int.class, int.class);
        return (m.getDeclaringClass() == ReflectionComplete.class) ? 1 : 0; // 1
    }
}
