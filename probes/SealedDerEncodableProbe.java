import java.lang.reflect.Method;
import java.util.Arrays;

/**
 * Probe for the sealed `java.security.DEREncodable` hierarchy (JDK 25 PEM JEP).
 *
 * Prints, for DEREncodable and every class in X509Certificate's hierarchy:
 *   - isSealed()
 *   - getPermittedSubclasses() (the public, filtered wrapper)
 *   - getInterfaces() on each permitted subclass (the exact call that NPEs
 *     with `Cannot read field "interfaces" because "rd" is null` inside
 *     Class.isDirectSubType while the wrapper filters).
 *
 * Uses Class.forName so no --enable-preview is needed to compile.
 */
public class SealedDerEncodableProbe {

    static void report(String name) {
        Class<?> c;
        try {
            c = Class.forName(name);
        } catch (Throwable t) {
            System.out.println("SEALED " + name + " FORNAME-FAIL " + t);
            return;
        }
        boolean sealed;
        try {
            sealed = c.isSealed();
        } catch (Throwable t) {
            System.out.println("SEALED " + name + " isSealed-THREW " + t);
            return;
        }
        Class<?>[] perm;
        try {
            perm = c.getPermittedSubclasses();
        } catch (Throwable t) {
            System.out.println("SEALED " + name + " sealed=" + sealed
                    + " getPermittedSubclasses-THREW " + t);
            return;
        }
        String rendered;
        if (perm == null) {
            rendered = "null";
        } else {
            StringBuilder sb = new StringBuilder("[");
            for (int i = 0; i < perm.length; i++) {
                if (i > 0) sb.append(", ");
                sb.append(perm[i] == null ? "<NULL-SLOT>" : perm[i].getName());
            }
            rendered = sb.append("]").toString();
        }
        System.out.println("SEALED " + name + " sealed=" + sealed
                + " permitted=" + (perm == null ? -1 : perm.length) + " " + rendered);
    }

    /**
     * The RAW native, before `getPermittedSubclasses()`'s isDirectSubType filter.
     * This is what distinguishes "native returned null" from "native returned an
     * 8-slot array of nulls" from "native returned a real 8-element list".
     */
    static void raw(String name) {
        try {
            Class<?> c = Class.forName(name);
            Method m = Class.class.getDeclaredMethod("getPermittedSubclasses0");
            m.setAccessible(true);
            Object v = m.invoke(c);
            if (v == null) {
                System.out.println("RAW0 " + name + " -> null");
                return;
            }
            Class<?>[] a = (Class<?>[]) v;
            StringBuilder sb = new StringBuilder("[");
            for (int i = 0; i < a.length; i++) {
                if (i > 0) sb.append(", ");
                sb.append(a[i] == null ? "<NULL-SLOT>" : a[i].getName());
            }
            System.out.println("RAW0 " + name + " n=" + a.length + " " + sb.append("]"));
        } catch (Throwable t) {
            System.out.println("RAW0 " + name + " THREW " + t
                    + (t.getCause() != null ? " cause=" + t.getCause() : ""));
        }
    }

    /**
     * Does an instance call on a null receiver throw NPE at the CALL SITE, or does
     * the VM push a frame with this==null? A registered native for a 0-arg
     * instance method that answers `args[0] == null` with a null RETURN turns the
     * second shape into `rd is null` one line later — the exact reported symptom.
     */
    static void nullReceiverShape() {
        Class<?> nul = null;
        try {
            System.out.println("NULLRECV unexpected n=" + nul.getInterfaces().length);
        } catch (Throwable t) {
            StackTraceElement top = t.getStackTrace().length > 0 ? t.getStackTrace()[0] : null;
            System.out.println("NULLRECV " + t + " top=" + top);
        }
    }

    /** Exercise getInterfaces() on every named class — the NPE site. */
    static void interfacesOf(String name) {
        try {
            Class<?> c = Class.forName(name);
            Class<?>[] ifs = c.getInterfaces();
            System.out.println("IFACES " + name + " n=" + ifs.length + " "
                    + Arrays.toString(Arrays.stream(ifs).map(Class::getName).toArray()));
        } catch (Throwable t) {
            System.out.println("IFACES " + name + " THREW " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        String[] hierarchy = {
                "java.lang.Object",
                "java.security.cert.Certificate",
                "java.security.cert.X509Extension",
                "java.io.Serializable",
                "java.security.DEREncodable",
                "java.security.cert.X509Certificate",
        };
        // The 8 permitted subclasses of DEREncodable on real HotSpot 25.
        String[] permitted = {
                "java.security.AsymmetricKey",
                "java.security.KeyPair",
                "java.security.spec.PKCS8EncodedKeySpec",
                "java.security.spec.X509EncodedKeySpec",
                "javax.crypto.EncryptedPrivateKeyInfo",
                "java.security.cert.X509Certificate",
                "java.security.cert.X509CRL",
                "java.security.PEMRecord",
        };

        System.out.println("=== phase 0: null-receiver instance-call shape ===");
        nullReceiverShape();

        System.out.println("=== phase 1: sealed metadata, nothing preloaded ===");
        for (String n : hierarchy) {
            report(n);
        }
        raw("java.security.DEREncodable");

        System.out.println("=== phase 2: getInterfaces() on each permitted subclass ===");
        for (String n : permitted) {
            interfacesOf(n);
        }

        System.out.println("=== phase 3: sealed metadata again, subclasses now loaded ===");
        for (String n : hierarchy) {
            report(n);
        }
        raw("java.security.DEREncodable");

        System.out.println("=== phase 4: repeated reflectionData churn on one mirror ===");
        Class<?> x509 = Class.forName("java.security.cert.X509Certificate");
        for (int i = 0; i < 5; i++) {
            System.out.println("  round " + i + " interfaces=" + x509.getInterfaces().length
                    + " methods=" + x509.getDeclaredMethods().length
                    + " fields=" + x509.getDeclaredFields().length);
        }
        System.out.println("PROBE-DONE");
    }
}
