import java.lang.reflect.*;
import java.util.*;

/** Dynamic-proxy probe: exercises the Proxy$Instance supertype and its natives. */
public class ProxyProbe {
    public interface Greeter { String greet(String who); int count(); }
    public interface Marker {}

    public static void main(String[] args) throws Exception {
        InvocationHandler h = new InvocationHandler() {
            int n = 0;
            public Object invoke(Object proxy, Method m, Object[] a) {
                n++;
                if (m.getName().equals("greet")) return "hello " + a[0];
                if (m.getName().equals("count")) return n;
                if (m.getName().equals("toString")) return "PROXY";
                if (m.getName().equals("hashCode")) return 42;
                if (m.getName().equals("equals")) return proxy == a[0];
                return null;
            }
        };
        Object p = Proxy.newProxyInstance(ProxyProbe.class.getClassLoader(),
                new Class<?>[]{Greeter.class, Marker.class}, h);
        Greeter g = (Greeter) p;
        System.out.println("greet=" + g.greet("world"));
        System.out.println("count=" + g.count());
        System.out.println("isProxyClass=" + Proxy.isProxyClass(p.getClass()));
        System.out.println("handler=" + (Proxy.getInvocationHandler(p) == h));
        System.out.println("ifaces=" + Arrays.toString(p.getClass().getInterfaces()));
        System.out.println("super=" + p.getClass().getSuperclass().getName());
        System.out.println("instanceofMarker=" + (p instanceof Marker));
        // A second proxy over a different interface set, to exercise the
        // shared-supertype path more than once.
        Object p2 = Proxy.newProxyInstance(ProxyProbe.class.getClassLoader(),
                new Class<?>[]{Marker.class}, h);
        System.out.println("p2class=" + (p2.getClass() != p.getClass()));
        System.out.println("PROXY-DONE");
    }
}
