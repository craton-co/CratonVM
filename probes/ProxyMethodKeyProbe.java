import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.HashMap;
import java.util.Map;

/**
 * Byte Buddy's {@code JavaDispatcher} builds a {@code Map<Method, Dispatcher>}
 * from {@code proxyInterface.getMethods()} and then, inside a
 * {@code java.lang.reflect.Proxy} invocation handler, looks the incoming
 * {@code Method} up in that map. When the lookup misses it throws
 * {@code IllegalStateException: No proxy target found for <method>} — which is
 * how Mockito's {@code InlineByteBuddyMockMaker} failed to initialise on
 * CratonVM, taking every {@code Mockito.mock(...)} call down with it.
 *
 * This probe reduces that to its primitive: put every {@code getMethods()}
 * entry of an interface into a {@code HashMap}, then look up the {@code Method}
 * the proxy hands the handler and report exactly which half of the
 * {@code HashMap} contract fails — the hash, or {@code Method.equals}, whose
 * name comparison in the JDK is an IDENTITY test that assumes reflection hands
 * out interned name strings.
 *
 * Prints one PROBE line per method; exits non-zero if any lookup misses.
 */
public final class ProxyMethodKeyProbe {

    public interface Target {
        boolean isInstance(Object value);

        String name(Object value);

        int count(Object value, int extra);
    }

    public static void main(String[] args) throws Exception {
        Map<Method, String> targets = new HashMap<>();
        for (Method m : Target.class.getMethods()) {
            targets.put(m, m.getName());
        }
        System.out.println("PROBE map_size=" + targets.size());

        final boolean[] failed = { false };
        Target proxy = (Target) Proxy.newProxyInstance(
                ProxyMethodKeyProbe.class.getClassLoader(),
                new Class<?>[] { Target.class },
                new InvocationHandler() {
                    @Override
                    public Object invoke(Object p, Method method, Object[] argument) {
                        String hit = targets.get(method);
                        Method key = null;
                        for (Method candidate : targets.keySet()) {
                            if (candidate.getName().equals(method.getName())) {
                                key = candidate;
                                break;
                            }
                        }
                        StringBuilder sb = new StringBuilder();
                        sb.append("PROBE method=").append(method.getName())
                          .append(" hit=").append(hit != null)
                          .append(" containsKey=").append(targets.containsKey(method));
                        if (key != null) {
                            sb.append(" sameInstance=").append(key == method)
                              .append(" equals=").append(key.equals(method))
                              .append(" reverseEquals=").append(method.equals(key))
                              .append(" hashEq=").append(key.hashCode() == method.hashCode())
                              .append(" declClassIdentity=")
                              .append(key.getDeclaringClass() == method.getDeclaringClass())
                              .append(" nameIdentity=")
                              .append((Object) key.getName() == (Object) method.getName())
                              .append(" nameInterned=")
                              .append((Object) method.getName() == (Object) method.getName().intern())
                              .append(" returnEq=")
                              .append(key.getReturnType().equals(method.getReturnType()))
                              .append(" paramsEq=")
                              .append(java.util.Arrays.equals(key.getParameterTypes(),
                                      method.getParameterTypes()));
                        } else {
                            sb.append(" NO_CANDIDATE_KEY");
                        }
                        System.out.println(sb);
                        if (hit == null) {
                            failed[0] = true;
                        }
                        if (method.getReturnType() == boolean.class) {
                            return Boolean.FALSE;
                        }
                        if (method.getReturnType() == int.class) {
                            return Integer.valueOf(0);
                        }
                        return null;
                    }
                });

        proxy.isInstance("x");
        proxy.name("x");
        proxy.count("x", 1);

        System.out.println("PROBE result=" + (failed[0] ? "MISS" : "OK"));
        if (failed[0]) {
            System.exit(1);
        }
    }
}
