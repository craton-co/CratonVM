/*
 * Interpreter round i1, wave 10, lane L4: a JDK dynamic proxy defined and
 * invoked under --jdk-only.
 *
 * SETUP: run CratonVM with --jdk-only (the defect is strict-mode only; the
 * output must be identical under --compatible too). Before the fix every
 * generated proxy method body that actually executed (JIT-compiled callers,
 * reflection, method handles) raised
 *   java.lang.NoClassDefFoundError: java/lang/reflect/Proxy$Dispatch
 * because --jdk-only resolves the owner of the body's INVOKESTATIC
 * Proxy$Dispatch.invokeProxy and that VM-invented class was treated as a
 * refused compatibility stand-in (Spring AOP's defaultProxyConfig bean).
 *
 * HotSpot 25 prints exactly (and CratonVM --jdk-only must, with and without
 * --nojit):
 *
 *   direct: hello:world
 *   add: 5
 *   default: default:x
 *   loop: 199990000
 *   reflect: hello:refl
 *   handle: hello:mh
 *   declared: java.io.IOException io
 *   undeclared: java.lang.reflect.UndeclaredThrowableException java.lang.Exception
 *   runtime: java.lang.IllegalStateException ise
 *   null-int: java.lang.NullPointerException
 *   object-methods: true true proxy-toString
 *   isProxyClass: true
 *   handler calls: 20012
 */
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;

public class L5ProxyUnderJdkOnly {
    public interface Greeter {
        String hello(String who);

        int add(int a, int b);

        void io() throws IOException;

        void checked();

        void boom();

        int nullInt();

        default String dflt(String s) {
            return "unused";
        }
    }

    static int calls;

    public static void main(String[] args) throws Throwable {
        InvocationHandler h = (proxy, m, a) -> {
            calls++;
            switch (m.getName()) {
                case "hello":
                    return "hello:" + a[0];
                case "add":
                    return (Integer) a[0] + (Integer) a[1];
                case "dflt":
                    return "default:" + a[0];
                case "io":
                    throw new IOException("io");
                case "checked":
                    throw new Exception("checked");
                case "boom":
                    throw new IllegalStateException("ise");
                case "nullInt":
                    return null;
                case "hashCode":
                    return 42;
                case "equals":
                    return proxy == a[0];
                case "toString":
                    return "proxy-toString";
                default:
                    throw new AssertionError(m.getName());
            }
        };
        Greeter g = (Greeter) Proxy.newProxyInstance(
                L5ProxyUnderJdkOnly.class.getClassLoader(), new Class<?>[] {Greeter.class}, h);

        System.out.println("direct: " + g.hello("world"));
        System.out.println("add: " + g.add(2, 3));
        System.out.println("default: " + g.dflt("x"));

        long sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += g.add(i, -1) + 1;
        }
        System.out.println("loop: " + sum);

        Method hello = Greeter.class.getMethod("hello", String.class);
        System.out.println("reflect: " + hello.invoke(g, "refl"));

        MethodHandle mh = MethodHandles.publicLookup().findVirtual(
                Greeter.class, "hello", MethodType.methodType(String.class, String.class));
        System.out.println("handle: " + (String) mh.invokeExact(g, "mh"));

        try {
            g.io();
        } catch (IOException e) {
            System.out.println("declared: " + e.getClass().getName() + " " + e.getMessage());
        }
        try {
            g.checked();
        } catch (Throwable t) {
            System.out.println("undeclared: " + t.getClass().getName() + " "
                    + (t.getCause() == null ? "null" : t.getCause().getClass().getName()));
        }
        try {
            g.boom();
        } catch (Throwable t) {
            System.out.println("runtime: " + t.getClass().getName() + " " + t.getMessage());
        }
        try {
            System.out.println("null-int returned " + g.nullInt());
        } catch (Throwable t) {
            System.out.println("null-int: " + t.getClass().getName());
        }

        System.out.println("object-methods: " + (g.hashCode() == 42) + " " + g.equals(g) + " "
                + g.toString());
        System.out.println("isProxyClass: " + Proxy.isProxyClass(g.getClass()));
        System.out.println("handler calls: " + calls);
    }
}
