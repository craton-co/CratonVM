import java.lang.reflect.Method;

/**
 * Probe for `Class.reflectionData()` returning null — the
 * `NullPointerException: Cannot read field "interfaces" because "rd" is null`
 * at `Class.getInterfaces(Class.java:1217)` seen while ByteBuddy walks the
 * sealed `java.security.DEREncodable` hierarchy.
 *
 * Drives `getInterfaces()` (which reads `reflectionData().interfaces`) hard
 * enough to tier up, from three call shapes:
 *   A. straight interpreted/JIT-compiled Java call
 *   B. reflective `Method.invoke` of `getInterfaces` (ByteBuddy's JavaDispatcher
 *      reaches `isSealed`/`getPermittedSubclasses` exactly this way)
 *   C. reflective `Method.invoke` of the private `reflectionData()` itself,
 *      which reports the null directly instead of via the NPE.
 */
public class ReflectionDataNullProbe {

    static final String[] NAMES = {
            "java.security.cert.X509Certificate",
            "java.security.cert.X509CRL",
            "java.security.KeyPair",
            "java.security.AsymmetricKey",
            "java.security.spec.PKCS8EncodedKeySpec",
            "java.security.spec.X509EncodedKeySpec",
            "javax.crypto.EncryptedPrivateKeyInfo",
            "java.security.PEMRecord",
            "java.lang.String",
            "java.util.ArrayList",
    };

    static int sink;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 60000;

        Class<?>[] cs = new Class<?>[NAMES.length];
        for (int i = 0; i < NAMES.length; i++) {
            try {
                cs[i] = Class.forName(NAMES[i]);
            } catch (Throwable t) {
                System.out.println("SKIP " + NAMES[i] + " " + t);
            }
        }

        // --- A: direct calls, hot ---
        int firstFail = -1;
        for (int r = 0; r < rounds; r++) {
            for (Class<?> c : cs) {
                if (c == null) continue;
                try {
                    sink += c.getInterfaces().length;
                } catch (Throwable t) {
                    if (firstFail < 0) {
                        firstFail = r;
                        System.out.println("A-FAIL round=" + r + " class=" + c.getName() + " " + t);
                    }
                }
            }
        }
        System.out.println("A-DONE firstFail=" + firstFail + " sink=" + sink);

        // --- C: reflective private reflectionData() ---
        Method rd = Class.class.getDeclaredMethod("reflectionData");
        rd.setAccessible(true);
        int nulls = 0;
        for (int r = 0; r < 20000; r++) {
            for (Class<?> c : cs) {
                if (c == null) continue;
                Object v = rd.invoke(c);
                if (v == null) {
                    if (nulls == 0) {
                        System.out.println("C-NULL round=" + r + " class=" + c.getName());
                    }
                    nulls++;
                }
            }
        }
        System.out.println("C-DONE nullReturns=" + nulls);

        // --- B: reflective getInterfaces / isSealed / getPermittedSubclasses ---
        Method gi = Class.class.getMethod("getInterfaces");
        Method sealed = Class.class.getMethod("isSealed");
        Method perm = Class.class.getMethod("getPermittedSubclasses");
        int bFail = 0;
        for (int r = 0; r < 20000; r++) {
            for (Class<?> c : cs) {
                if (c == null) continue;
                try {
                    sink += ((Class<?>[]) gi.invoke(c)).length;
                    sink += ((Boolean) sealed.invoke(c)) ? 1 : 0;
                    Object p = perm.invoke(c);
                    if (p != null) sink += ((Class<?>[]) p).length;
                } catch (Throwable t) {
                    if (bFail == 0) {
                        System.out.println("B-FAIL round=" + r + " class=" + c.getName() + " " + t
                                + (t.getCause() != null ? " cause=" + t.getCause() : ""));
                    }
                    bFail++;
                }
            }
        }
        System.out.println("B-DONE failures=" + bFail + " sink=" + sink);
        System.out.println("PROBE-DONE");
    }
}
