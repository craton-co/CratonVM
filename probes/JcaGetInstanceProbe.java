import java.security.*;
import javax.crypto.Cipher;
import javax.crypto.KeyAgreement;
import javax.crypto.KeyGenerator;
import javax.crypto.Mac;
import javax.crypto.SecretKeyFactory;

/**
 * Does `getInstance` REFUSE what it cannot serve, and does it name the provider
 * that served it? One line per (engine, algorithm), so a diff against HotSpot
 * names the engine and the algorithm.
 *
 * Deliberately includes algorithms that SHOULD work, not only bogus ones: a
 * "fix" that makes getInstance strict is only correct if it still hands out
 * every generator it used to.
 */
public final class JcaGetInstanceProbe {

    interface Get { Object get(String alg) throws Exception; }

    static String provOf(Object o) {
        try {
            Provider p = (Provider) o.getClass().getMethod("getProvider").invoke(o);
            return p == null ? "null" : p.getName();
        } catch (Throwable t) { return "?"; }
    }

    static void probe(String engine, Get g, String... algs) {
        for (String alg : algs) {
            String key = engine + "(" + alg + ")";
            try {
                Object o = g.get(alg);
                System.out.println(key + " -> OK provider=" + provOf(o));
            } catch (NoSuchAlgorithmException e) {
                System.out.println(key + " -> NoSuchAlgorithmException");
            } catch (Throwable t) {
                System.out.println(key + " -> " + t.getClass().getName());
            }
        }
    }

    public static void main(String[] a) {
        String BOGUS = "TOTALLY-BOGUS-ALG";

        probe("KeyPairGenerator", KeyPairGenerator::getInstance,
                "RSA", "EC", "DSA", "Ed25519", "Ed448", "EdDSA", "X25519", "XDH",
                "ML-DSA", "ML-KEM", "SLH-DSA", "RSASSA-PSS", BOGUS);

        probe("KeyFactory", KeyFactory::getInstance,
                "RSA", "EC", "Ed25519", "XDH", "ML-DSA", "ML-DSA-44", BOGUS);

        probe("Signature", Signature::getInstance,
                "SHA256withRSA", "SHA256withECDSA", "SHA384withECDSA", "Ed25519",
                "ML-DSA", "ML-DSA-44", "NONEwithRSA", BOGUS);

        probe("MessageDigest", MessageDigest::getInstance,
                "SHA-256", "SHA-512", "SHA3-256", "MD5", BOGUS);

        probe("Cipher", Cipher::getInstance,
                "AES/GCM/NoPadding", "AES/CBC/PKCS5Padding", "RSA/ECB/PKCS1Padding", BOGUS);

        probe("KeyGenerator", KeyGenerator::getInstance, "AES", "HmacSHA256", BOGUS);

        probe("Mac", Mac::getInstance, "HmacSHA256", "HmacSHA512", BOGUS);

        probe("SecretKeyFactory", SecretKeyFactory::getInstance,
                "PBKDF2WithHmacSHA256", "PBEWithMD5AndDES", BOGUS);

        probe("KeyAgreement", KeyAgreement::getInstance, "ECDH", "X25519", BOGUS);

        probe("KeyStore", KeyStore::getInstance, "PKCS12", "JKS", BOGUS);

        probe("SecureRandom", SecureRandom::getInstance, "SHA1PRNG", BOGUS);

        probe("CertificateFactory", java.security.cert.CertificateFactory::getInstance,
                "X.509", BOGUS);

        // The provider FILTER view of the same question.
        for (String f : new String[] {
                "KeyPairGenerator.RSA", "KeyPairGenerator.EC", "KeyPairGenerator.Ed25519",
                "KeyPairGenerator.ML-DSA", "MessageDigest.SHA-256", "Cipher.AES",
                "KeyPairGenerator." + BOGUS }) {
            Provider[] ps = Security.getProviders(f);
            StringBuilder sb = new StringBuilder();
            if (ps != null) { for (Provider p : ps) { sb.append(p.getName()).append(' '); } }
            System.out.println("filter(" + f + ") -> " + (sb.length() == 0 ? "<none>" : sb.toString().trim()));
        }
    }
}
