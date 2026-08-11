import java.io.File;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.HashMap;
import java.util.Map;

/**
 * Byte Buddy's {@code JavaDispatcher}, reduced to its primitive, run TWICE —
 * once against an interface from the application loader and once against the
 * SAME interface loaded again in an isolated child loader.
 *
 * {@code JavaDispatcher.run()} keys a {@code HashMap<Method, Dispatcher>} off
 * {@code proxyInterface.getMethods()} and looks the incoming method up inside a
 * {@code java.lang.reflect.Proxy} handler. {@code Method.equals} compares
 * declaring classes by IDENTITY, so the lookup can only hit if the method the
 * proxy hands the handler comes from the same {@code Class} object the map was
 * built from. A generated proxy resolves its method table from its OWN defining
 * loader; a VM that resolves it from the system loader instead hands world 2 a
 * world-1 method and the lookup misses with
 * {@code IllegalStateException: No proxy target found for ...}.
 *
 * That is the shape behind Mockito's inline mock maker failing to initialise on
 * the second class-loader world of
 * {@code HikariDataSourceConfigurationTests} while succeeding on the first.
 *
 * Usage: {@code ProxyTwoWorldProbe <jar> <interface binary name>}
 */
public final class ProxyTwoWorldProbe {

    public static void main(String[] args) throws Exception {
        String jar = args[0];
        String name = args[1];

        // Order matters. CratonVM resolves a not-yet-loaded name globally, so
        // whichever world touches the class FIRST can end up owning the only
        // copy; the real failure (Spring Boot's ModifiedClassPathClassLoader
        // method running before a plain one) has the ISOLATED world first.
        boolean isolatedFirst = args.length > 2 && "isolatedFirst".equals(args[2]);

        // Parent = the PLATFORM loader, matching Spring Boot's
        // ModifiedClassPathClassLoader. A bootstrap (null) parent is a
        // different topology and CratonVM handles the two differently.
        URLClassLoader child = new URLClassLoader("isolated",
                new URL[] { new File(jar).toURI().toURL() },
                ClassLoader.getPlatformClassLoader());

        boolean ok1;
        boolean ok2;
        Class<?> world1;
        Class<?> world2;
        if (isolatedFirst) {
            world2 = Class.forName(name, false, child);
            ok2 = check("world2-child-loader", world2);
            world1 = Class.forName(name);
            ok1 = check("world1-app-loader", world1);
        } else {
            world1 = Class.forName(name);
            ok1 = check("world1-app-loader", world1);
            world2 = Class.forName(name, false, child);
            ok2 = check("world2-child-loader", world2);
        }
        System.out.println("PROBE distinctClasses=" + (world1 != world2)
                + " world1Loader=" + world1.getClassLoader()
                + " world2Loader=" + world2.getClassLoader());

        System.out.println("PROBE result=" + ((ok1 && ok2) ? "OK" : "MISS"));
        if (!(ok1 && ok2)) {
            System.exit(1);
        }
    }

    private static boolean check(String label, Class<?> iface) throws Exception {
        final Map<Method, String> targets = new HashMap<>();
        for (Method m : iface.getMethods()) {
            targets.put(m, m.getName());
        }
        final boolean[] miss = { false };
        Object proxy = Proxy.newProxyInstance(iface.getClassLoader(), new Class<?>[] { iface },
                new InvocationHandler() {
                    @Override
                    public Object invoke(Object p, Method method, Object[] argument) {
                        boolean hit = targets.containsKey(method);
                        if (!hit) {
                            miss[0] = true;
                            StringBuilder sb = new StringBuilder("PROBE " + label + " MISS name=")
                                    .append(method.getName())
                                    .append(" incomingDecl=")
                                    .append(System.identityHashCode(method.getDeclaringClass()))
                                    .append('/')
                                    .append(method.getDeclaringClass().getClassLoader());
                            for (Method key : targets.keySet()) {
                                if (!key.getName().equals(method.getName())) {
                                    continue;
                                }
                                sb.append(" keyDecl=")
                                  .append(System.identityHashCode(key.getDeclaringClass()))
                                  .append('/')
                                  .append(key.getDeclaringClass().getClassLoader())
                                  .append(" declIdentity=")
                                  .append(key.getDeclaringClass() == method.getDeclaringClass())
                                  .append(" hashEq=").append(key.hashCode() == method.hashCode())
                                  .append(" equals=").append(key.equals(method));
                            }
                            System.out.println(sb);
                        }
                        Class<?> ret = method.getReturnType();
                        if (ret == boolean.class) {
                            return Boolean.FALSE;
                        }
                        if (ret.isPrimitive()) {
                            return Integer.valueOf(0);
                        }
                        return null;
                    }
                });

        // Which Class did the generated proxy actually bind as its interface?
        // `Method.equals` compares declaring classes by identity, so if this
        // is not the same object as `iface` the proxy's own `implements` link
        // — not just the `<clinit>` method lookup — bound the wrong copy.
        Class<?>[] bound = proxy.getClass().getInterfaces();
        StringBuilder ib = new StringBuilder("PROBE " + label + " proxyClass="
                + proxy.getClass().getName() + " interfaces=" + bound.length);
        for (Class<?> b : bound) {
            ib.append(" [").append(b.getName())
              .append(" id=").append(System.identityHashCode(b))
              .append(" loader=").append(b.getClassLoader())
              .append(" sameAsIface=").append(b == iface).append(']');
        }
        System.out.println(ib);

        for (Method m : iface.getMethods()) {
            Object[] a = new Object[m.getParameterCount()];
            for (int i = 0; i < a.length; i++) {
                a[i] = m.getParameterTypes()[i].isPrimitive() ? Integer.valueOf(0) : null;
            }
            try {
                m.invoke(proxy, a);
            } catch (Exception e) {
                System.out.println("PROBE " + label + " invoke_error " + m.getName() + " " + e);
            }
        }
        System.out.println("PROBE " + label + " methods=" + iface.getMethods().length
                + " miss=" + miss[0]);
        return !miss[0];
    }
}
