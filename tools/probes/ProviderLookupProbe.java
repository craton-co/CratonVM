import java.security.KeyFactory;
import java.security.KeyStore;
import java.security.MessageDigest;
import java.security.Provider;
import java.security.SecureRandom;
import java.security.Security;
import java.security.Signature;
import javax.crypto.Cipher;

/** One deterministic line per JCA `getInstance(algorithm, providerName)` shape,
 *  so CratonVM output can be diffed against real HotSpot. Pins the ordering
 *  real `sun.security.jca.GetInstance.getInstance` uses: the provider-existence
 *  check runs BEFORE any algorithm lookup, so an unregistered provider name is
 *  always NoSuchProviderException, never NoSuchAlgorithmException. */
public class ProviderLookupProbe {

    interface Call { Object run() throws Exception; }

    static void t(String label, Call c) {
        StringBuilder sb = new StringBuilder(label).append(" => ");
        try {
            Object o = c.run();
            sb.append(o == null ? "null" : "OK");
        } catch (Throwable e) {
            sb.append(e.getClass().getName()).append(": ").append(e.getMessage());
            Throwable cause = e.getCause();
            if (cause != null) {
                sb.append(" | cause=").append(cause.getClass().getName())
                  .append(": ").append(cause.getMessage());
            }
        }
        System.out.println(sb);
    }

    public static void main(String[] args) {
        final String ghost = "com.example.KeyStoreProvider";

        // Security.getProvider must return null for an unregistered name.
        Provider p = Security.getProvider(ghost);
        System.out.println("Security.getProvider(ghost) => " + (p == null ? "null" : p.getName()));
        System.out.println("Security.getProvider(SUN) != null => " + (Security.getProvider("SUN") != null));

        // The two shapes JksSslStoreBundleTests exercises.
        t("KeyStore.getInstance(PKCS12, ghost)", () -> KeyStore.getInstance("PKCS12", ghost));
        t("KeyStore.getInstance(JKS, ghost)", () -> KeyStore.getInstance("JKS", ghost));

        // Registered provider that does not offer the requested type: the OTHER
        // branch, which must stay NoSuchAlgorithmException-wrapped.
        t("KeyStore.getInstance(PKCS12, SUN)", () -> KeyStore.getInstance("PKCS12", "SUN"));
        t("KeyStore.getInstance(JKS, SUN)", () -> KeyStore.getInstance("JKS", "SUN"));
        t("KeyStore.getInstance(NoSuchType, SUN)", () -> KeyStore.getInstance("NoSuchType", "SUN"));
        t("KeyStore.getInstance(PKCS12)", () -> KeyStore.getInstance("PKCS12"));
        t("KeyStore.getInstance(NoSuchType)", () -> KeyStore.getInstance("NoSuchType"));

        // The same provider-first ordering on every other JCA engine that goes
        // through GetInstance.getInstance(String, Class, String, String).
        t("MessageDigest.getInstance(SHA-256, ghost)", () -> MessageDigest.getInstance("SHA-256", ghost));
        t("MessageDigest.getInstance(SHA-256, SUN)", () -> MessageDigest.getInstance("SHA-256", "SUN"));
        t("MessageDigest.getInstance(NoSuchAlgo, SUN)", () -> MessageDigest.getInstance("NoSuchAlgo", "SUN"));
        t("KeyFactory.getInstance(RSA, ghost)", () -> KeyFactory.getInstance("RSA", ghost));
        t("Signature.getInstance(SHA256withRSA, ghost)", () -> Signature.getInstance("SHA256withRSA", ghost));
        t("SecureRandom.getInstance(SHA1PRNG, ghost)", () -> SecureRandom.getInstance("SHA1PRNG", ghost));
        t("Cipher.getInstance(AES/CBC/PKCS5Padding, ghost)",
                () -> Cipher.getInstance("AES/CBC/PKCS5Padding", ghost));

        // Empty / null provider names are argument errors, not lookup misses.
        // Each engine words this differently — Cipher even capitalises where
        // the shared GetInstance path does not.
        t("KeyStore.getInstance(PKCS12, emptyString)", () -> KeyStore.getInstance("PKCS12", ""));
        t("MessageDigest.getInstance(SHA-256, emptyString)", () -> MessageDigest.getInstance("SHA-256", ""));
        t("KeyFactory.getInstance(RSA, emptyString)", () -> KeyFactory.getInstance("RSA", ""));
        t("Signature.getInstance(SHA256withRSA, emptyString)", () -> Signature.getInstance("SHA256withRSA", ""));
        t("SecureRandom.getInstance(SHA1PRNG, emptyString)", () -> SecureRandom.getInstance("SHA1PRNG", ""));
        t("Cipher.getInstance(AES/CBC/PKCS5Padding, emptyString)",
                () -> Cipher.getInstance("AES/CBC/PKCS5Padding", ""));

        // Registered provider that DOES own the algorithm: must succeed, so the
        // provider-existence check can't be implemented as a blanket reject.
        t("KeyFactory.getInstance(RSA, SunRsaSign)", () -> KeyFactory.getInstance("RSA", "SunRsaSign"));
        t("Signature.getInstance(SHA256withRSA, SunRsaSign)",
                () -> Signature.getInstance("SHA256withRSA", "SunRsaSign"));
        t("SecureRandom.getInstance(SHA1PRNG, SUN)", () -> SecureRandom.getInstance("SHA1PRNG", "SUN"));
        t("Cipher.getInstance(AES/CBC/PKCS5Padding, SunJCE)",
                () -> Cipher.getInstance("AES/CBC/PKCS5Padding", "SunJCE"));
        t("MessageDigest.getInstance(MD5, SUN)", () -> MessageDigest.getInstance("MD5", "SUN"));
        t("MessageDigest.getInstance(SHA-1, SUN)", () -> MessageDigest.getInstance("SHA-1", "SUN"));

        // Registered provider that does NOT own the algorithm.
        t("KeyFactory.getInstance(RSA, SUN)", () -> KeyFactory.getInstance("RSA", "SUN"));
        t("Cipher.getInstance(AES/CBC/PKCS5Padding, SUN)",
                () -> Cipher.getInstance("AES/CBC/PKCS5Padding", "SUN"));

        // Single-argument form: unsupported algorithm must be
        // NoSuchAlgorithmException, not some other security exception.
        t("MessageDigest.getInstance(NoSuchAlgo)", () -> MessageDigest.getInstance("NoSuchAlgo"));
        t("MessageDigest.getInstance(SHA-256)", () -> MessageDigest.getInstance("SHA-256"));
        t("KeyFactory.getInstance(NoSuchAlgo)", () -> KeyFactory.getInstance("NoSuchAlgo"));
    }
}
