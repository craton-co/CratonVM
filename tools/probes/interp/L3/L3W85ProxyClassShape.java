/*
 * Interpreter round i4, wave 85, lane L3: what reflection sees on a dynamic
 * proxy CLASS.
 *
 * HotSpot's proxies come from java.lang.reflect.ProxyGenerator: the static
 * `Method` fields are `private static final m0..mN`, numbered hashCode (m0),
 * equals (m1), toString (m2), then the interfaces' methods, in that
 * class-file order; the class also declares
 * `private static MethodHandles.Lookup proxyClassLookup(MethodHandles.Lookup)
 * throws IllegalAccessException`, which refuses any lookup but a
 * full-privilege one on java.lang.reflect.Proxy with
 * `new IllegalAccessException(lookup.toString())`. CratonVM's generator
 * (classloading/src/proxy_gen.rs) named the fields m_0.. in name-sorted order
 * and had no proxyClassLookup.
 *
 * A package-private interface keeps the proxy class in this (unnamed)
 * package and module, so its private members can be opened here. The
 * proxy's own name ($ProxyN) is not printed: N counts the JVM's proxies.
 * getDeclaredMethods() order is HotSpot-internal, so it is sorted.
 *
 * Run (both modes must print the HotSpot lines):
 *   javac -d out L3W85ProxyClassShape.java
 *   cratonvm --jdk-only   -cp out L3W85ProxyClassShape
 *   cratonvm --compatible -cp out L3W85ProxyClassShape
 *   java                  -cp out L3W85ProxyClassShape   (HotSpot 25)
 *
 * Expected HotSpot 25 output (derived from ProxyGenerator, JDK 25.0.3):
 *   class: final super=java.lang.reflect.Proxy
 *   field: private static final java.lang.reflect.Method m0 = Object.hashCode
 *   field: private static final java.lang.reflect.Method m1 = Object.equals
 *   field: private static final java.lang.reflect.Method m2 = Object.toString
 *   field: private static final java.lang.reflect.Method m3 = Runner.run
 *   method: equals public final
 *   method: hashCode public final
 *   method: proxyClassLookup private static throws [class java.lang.IllegalAccessException]
 *   method: run public final
 *   method: toString public final
 *   ctor: public [interface java.lang.reflect.InvocationHandler]
 *   lookup: java.lang.IllegalAccessException L3W85ProxyClassShape
 *   call: 7
 */
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.lang.reflect.Proxy;
import java.util.Arrays;
import java.util.Comparator;

public class L3W85ProxyClassShape {
    interface Runner {
        int run();
    }

    public static void main(String[] args) throws Exception {
        Runner r = (Runner) Proxy.newProxyInstance(
                L3W85ProxyClassShape.class.getClassLoader(),
                new Class<?>[] {Runner.class},
                (proxy, m, a) -> 7);
        Class<?> pc = r.getClass();
        System.out.println("class: " + Modifier.toString(pc.getModifiers())
                + " super=" + pc.getSuperclass().getName());

        for (Field f : pc.getDeclaredFields()) {
            f.setAccessible(true);
            Method m = (Method) f.get(null);
            System.out.println("field: " + Modifier.toString(f.getModifiers()) + " "
                    + f.getType().getName() + " " + f.getName() + " = "
                    + m.getDeclaringClass().getSimpleName() + "." + m.getName());
        }

        Method[] methods = pc.getDeclaredMethods();
        Arrays.sort(methods, Comparator.comparing(Method::getName));
        for (Method m : methods) {
            String line = "method: " + m.getName() + " " + Modifier.toString(m.getModifiers());
            if (m.getExceptionTypes().length > 0) {
                line += " throws " + Arrays.toString(m.getExceptionTypes());
            }
            System.out.println(line);
        }

        for (Constructor<?> c : pc.getDeclaredConstructors()) {
            System.out.println("ctor: " + Modifier.toString(c.getModifiers()) + " "
                    + Arrays.toString(c.getParameterTypes()));
        }

        Method lookup = pc.getDeclaredMethod("proxyClassLookup", MethodHandles.Lookup.class);
        lookup.setAccessible(true);
        try {
            System.out.println("lookup: answered " + lookup.invoke(null, MethodHandles.lookup()));
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            System.out.println("lookup: " + t.getClass().getName() + " " + t.getMessage());
        }

        System.out.println("call: " + r.run());
    }
}
