import java.lang.reflect.*;

/**
 * Probe: does the VM-synthesized java.lang.reflect.Method handed to a
 * java.lang.reflect.Proxy InvocationHandler keep a non-null `name` while the
 * young collector is relocating?
 *
 * Mirrors Spring's SynthesizedMergedAnnotationInvocationHandler.invoke, which
 * switches on method.getName() -- javac lowers a String switch to
 * `astore <localN>; aload <localN>; invokevirtual String.hashCode()`, so a null
 * name surfaces as
 *   NPE: Cannot invoke "String.hashCode()" because "<localN>" is null
 */
public class ProxyMethodNameProbe {

    public interface Api {
        String alpha();
        String bravo(String a);
        int charlie(int a, int b);
    }

    static int nullName = 0;
    static int nullSwitch = 0;
    static int calls = 0;

    static class Handler implements InvocationHandler {
        @Override
        public Object invoke(Object proxy, Method method, Object[] args) {
            calls++;
            String n = method.getName();
            if (n == null) {
                nullName++;
                return defaultFor(method);
            }
            // The exact shape Spring uses: a String switch -> String.hashCode()
            switch (n) {
                case "alpha":   return "A";
                case "bravo":   return "B";
                case "charlie": return Integer.valueOf(7);
                case "hashCode":return Integer.valueOf(1);
                case "equals":  return Boolean.valueOf(proxy == args[0]);
                case "toString":return "proxy";
                default:        return defaultFor(method);
            }
        }
        private Object defaultFor(Method m) {
            Class<?> r = (m == null) ? null : m.getReturnType();
            if (r == int.class) return Integer.valueOf(0);
            if (r == boolean.class) return Boolean.valueOf(false);
            return null;
        }
    }

    public static void main(String[] argv) throws Exception {
        int rounds = argv.length > 0 ? Integer.parseInt(argv[0]) : 20000;
        Api api = (Api) Proxy.newProxyInstance(
                ProxyMethodNameProbe.class.getClassLoader(),
                new Class<?>[] { Api.class },
                new Handler());

        Object[] keep = new Object[64];
        int k = 0;
        for (int i = 0; i < rounds; i++) {
            // Churn the young generation so a minor collection lands inside the
            // VM's proxy-Method synthesis window.
            keep[k++ & 63] = new byte[1024 + (i & 1023)];
            k &= 63;

            try {
                if (api.alpha() == null) nullSwitch++;
                if (api.bravo("x") == null) nullSwitch++;
                api.charlie(i, 3);
            } catch (NullPointerException e) {
                nullSwitch++;
                if (nullSwitch <= 3) {
                    System.out.println("NPE #" + nullSwitch + ": " + e.getMessage());
                }
            }
        }
        System.out.println("PROBE calls=" + calls
                + " null_getName=" + nullName
                + " bad_results_or_npe=" + nullSwitch);
        System.out.println(
                (nullName == 0 && nullSwitch == 0) ? "PROBE=PASS" : "PROBE=FAIL");
    }
}
