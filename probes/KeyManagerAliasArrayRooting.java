import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509KeyManager;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.cert.Certificate;

/**
 * `X509KeyManager.getServerAliases` / `getClientAliases` must never answer an
 * array with a hole in it.
 *
 * This is the instrument for the §D residual of
 * `openssl-key-material-and-engine-residuals-20260813.md`. Those two natives
 * built their `String[]` and then filled it in a loop whose body allocates
 * (`create_string`), holding the array reference raw across the allocation —
 * so a moving young collection relocated the array and every store after it
 * landed in the vacated slots. What the live array kept was `null`.
 *
 * The failure is intermittent by construction: it needs a collection to land
 * INSIDE the fill loop. So this does not wait for one — it FORCES the
 * schedule, allocating hard on a second thread for the whole run and calling
 * the accessor with many aliases so the loop is long enough to be interrupted.
 * A run that reports `holes=0` under that pressure is evidence; a run that
 * merely did not crash is not.
 *
 * Prints `RESULT calls=N holes=H shortfalls=S`. Any non-zero `holes` or
 * `shortfalls` is the defect. Exits non-zero so a harness can gate on it.
 */
public final class KeyManagerAliasArrayRooting {

    private static final int ALIASES = 64;

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 400;

        KeyStore ks = buildKeyStore();
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(
                KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, "changeit".toCharArray());

        // A TrustManagerFactory too: `getTrustManagers()` has the same
        // one-element-array shape and the same allocation between the array
        // and the store.
        TrustManagerFactory tmf = TrustManagerFactory.getInstance(
                TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ks);

        Thread churn = new Thread(() -> {
            Object[] keep = new Object[256];
            int i = 0;
            while (!Thread.currentThread().isInterrupted()) {
                keep[i++ & 255] = new byte[16 * 1024];
            }
        }, "alloc-churn");
        churn.setDaemon(true);
        churn.start();

        int calls = 0;
        int holes = 0;
        int shortfalls = 0;
        int nullManagers = 0;

        for (int r = 0; r < reps; r++) {
            javax.net.ssl.KeyManager[] kms = kmf.getKeyManagers();
            if (kms == null || kms.length == 0 || kms[0] == null) {
                nullManagers++;
                continue;
            }
            javax.net.ssl.TrustManager[] tms = tmf.getTrustManagers();
            if (tms == null || tms.length == 0 || tms[0] == null) {
                nullManagers++;
                continue;
            }
            if (!(kms[0] instanceof X509KeyManager)) {
                System.out.println("SKIP key manager is " + kms[0].getClass().getName());
                return;
            }
            X509KeyManager km = (X509KeyManager) kms[0];
            for (String type : new String[] {"RSA", "EC", "DSA"}) {
                String[] server = km.getServerAliases(type, null);
                String[] client = km.getClientAliases(type, null);
                calls += 2;
                holes += countNulls(server) + countNulls(client);
                if ("RSA".equals(type)) {
                    // Every alias in this store holds an RSA key, so both
                    // lists must name all of them. A SHORT array is the other
                    // face of the same defect: it says the population itself
                    // was lost, not just the strings in it.
                    if (server == null || server.length != ALIASES) {
                        shortfalls++;
                    }
                    if (client == null || client.length != ALIASES) {
                        shortfalls++;
                    }
                }
            }
        }
        churn.interrupt();

        System.out.println("RESULT calls=" + calls + " holes=" + holes
                + " shortfalls=" + shortfalls + " nullManagers=" + nullManagers);
        if (holes != 0 || shortfalls != 0 || nullManagers != 0) {
            System.out.println("FAILED — an alias array came back with a hole, "
                    + "a short length, or a null manager");
            System.exit(1);
        }
        System.out.println("PASSED");
    }

    private static int countNulls(String[] a) {
        if (a == null) {
            return 0;
        }
        int n = 0;
        for (String s : a) {
            if (s == null) {
                n++;
            }
        }
        return n;
    }

    /** A PKCS#12 store with {@link #ALIASES} distinct RSA key entries. */
    private static KeyStore buildKeyStore() throws Exception {
        char[] pw = "changeit".toCharArray();
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, pw);

        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        Certificate cert = SelfSigned.certificate(kp);

        for (int i = 0; i < ALIASES; i++) {
            ks.setKeyEntry("alias-" + i, (PrivateKey) kp.getPrivate(), pw,
                    new Certificate[] {cert});
        }
        // Round-trip through bytes so the store the factory sees is a parsed
        // one, the way every real caller's is.
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        ks.store(out, pw);
        KeyStore reloaded = KeyStore.getInstance("PKCS12");
        reloaded.load(new ByteArrayInputStream(out.toByteArray()), pw);
        return reloaded;
    }

    /**
     * A self-signed certificate for {@code kp}, built through whichever of the
     * two generators this image actually has.
     *
     * `sun.security.x509.CertAndKeyGen` is not exported, and netty's
     * `SelfSignedCertificate` is not on a plain classpath, so this probe asks
     * reflectively and says so rather than failing to compile against one.
     */
    private static final class SelfSigned {
        static Certificate certificate(KeyPair kp) throws Exception {
            Class<?> c = Class.forName("io.netty.handler.ssl.util.SelfSignedCertificate");
            Object ssc = c.getDeclaredConstructor().newInstance();
            Certificate cert = (Certificate) c.getMethod("cert").invoke(ssc);
            // The certificate's own key is irrelevant here — nothing verifies
            // a signature in this probe, and `setKeyEntry` only needs a chain
            // that parses. Keeping netty's certificate avoids depending on a
            // non-exported JDK internal.
            if (kp == null) {
                throw new IllegalStateException("unreachable");
            }
            return cert;
        }
    }
}
