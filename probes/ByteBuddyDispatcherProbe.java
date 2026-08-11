import java.lang.annotation.Annotation;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.HashMap;
import java.util.Map;

/**
 * Reproduce Byte Buddy's {@code JavaDispatcher} lookup outside Byte Buddy.
 *
 * {@code JavaDispatcher.run()} keys a {@code HashMap<Method, Dispatcher>} off
 * {@code proxyInterface.getMethods()} and then looks the incoming method up
 * inside a {@code java.lang.reflect.Proxy} handler. Mockito's inline mock maker
 * dies on CratonVM with
 * {@code IllegalStateException: No proxy target found for public abstract
 * boolean ...ParameterList$ForLoadedExecutable$Executable.isInstance(Object)},
 * i.e. that lookup missed.
 *
 * There are only two ways to miss: the method was never a key (getMethods()
 * did not yield it), or the key does not match (hash/equals). This prints both
 * sides for the REAL Byte Buddy interface, so the answer needs one run.
 *
 * Usage: {@code ByteBuddyDispatcherProbe [<interface binary name>]}
 */
public final class ByteBuddyDispatcherProbe {

    private static final String DEFAULT_IFACE =
            "net.bytebuddy.description.method.ParameterList$ForLoadedExecutable$Executable";

    public static void main(String[] args) throws Exception {
        String name = args.length > 0 ? args[0] : DEFAULT_IFACE;
        Class<?> iface = Class.forName(name);
        System.out.println("PROBE iface=" + iface.getName() + " isInterface=" + iface.isInterface());

        for (Annotation a : iface.getAnnotations()) {
            System.out.println("PROBE iface_annotation=" + a);
        }

        Method[] declared = iface.getDeclaredMethods();
        System.out.println("PROBE declaredMethods=" + declared.length);
        for (Method m : declared) {
            System.out.println("PROBE declared name=" + m.getName() + " decl="
                    + m.getDeclaringClass().getName() + " annotations=" + m.getAnnotations().length);
        }

        Method[] pub = iface.getMethods();
        System.out.println("PROBE getMethods=" + pub.length);
        final Map<Method, String> targets = new HashMap<>();
        for (Method m : pub) {
            StringBuilder sb = new StringBuilder("PROBE public name=").append(m.getName())
                    .append(" decl=").append(m.getDeclaringClass().getName())
                    .append(" ret=").append(m.getReturnType().getName())
                    .append(" params=").append(m.getParameterCount())
                    .append(" annotations=[");
            for (Annotation a : m.getAnnotations()) {
                sb.append(a.annotationType().getSimpleName()).append(' ');
            }
            sb.append(']');
            System.out.println(sb);
            targets.put(m, m.getName());
        }
        System.out.println("PROBE map_size=" + targets.size());

        final boolean[] miss = { false };
        Object proxy = Proxy.newProxyInstance(iface.getClassLoader(), new Class<?>[] { iface },
                new InvocationHandler() {
                    @Override
                    public Object invoke(Object p, Method method, Object[] argument) {
                        boolean hit = targets.containsKey(method);
                        StringBuilder sb = new StringBuilder("PROBE invoke name=")
                                .append(method.getName())
                                .append(" decl=").append(method.getDeclaringClass().getName())
                                .append(" hit=").append(hit);
                        if (!hit) {
                            miss[0] = true;
                            for (Method key : targets.keySet()) {
                                if (!key.getName().equals(method.getName())) {
                                    continue;
                                }
                                sb.append(" | candidate decl=").append(key.getDeclaringClass().getName())
                                  .append(" declIdentity=")
                                  .append(key.getDeclaringClass() == method.getDeclaringClass())
                                  .append(" hashEq=").append(key.hashCode() == method.hashCode())
                                  .append(" equals=").append(key.equals(method))
                                  .append(" reverseEquals=").append(method.equals(key))
                                  .append(" nameIdentity=")
                                  .append((Object) key.getName() == (Object) method.getName())
                                  .append(" returnEq=")
                                  .append(key.getReturnType() == method.getReturnType())
                                  .append(" paramsEq=")
                                  .append(java.util.Arrays.equals(key.getParameterTypes(),
                                          method.getParameterTypes()));
                            }
                        }
                        System.out.println(sb);
                        Class<?> ret = method.getReturnType();
                        if (ret == boolean.class) {
                            return Boolean.FALSE;
                        }
                        if (ret == int.class) {
                            return Integer.valueOf(0);
                        }
                        if (ret.isPrimitive()) {
                            return Integer.valueOf(0);
                        }
                        return null;
                    }
                });

        for (Method m : pub) {
            Object[] a = new Object[m.getParameterCount()];
            for (int i = 0; i < a.length; i++) {
                a[i] = m.getParameterTypes()[i].isPrimitive() ? Integer.valueOf(0) : null;
            }
            try {
                m.invoke(proxy, a);
            } catch (Exception e) {
                System.out.println("PROBE invoke_error " + m.getName() + " " + e);
            }
        }

        System.out.println("PROBE result=" + (miss[0] ? "MISS" : "OK"));
    }
}
