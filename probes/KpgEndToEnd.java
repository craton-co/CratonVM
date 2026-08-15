import java.security.*;

/**
 * getInstance AND generateKeyPair, per algorithm.
 *
 * The serviceability test that decides whether getInstance may refuse a name
 * has to equal what generateKeyPair can actually do — refuse less and callers
 * keep the dead-fallback bug, refuse more and working algorithms disappear.
 * Only this table says which is which.
 */
public final class KpgEndToEnd {
    public static void main(String[] a) {
        String[] algs = {"RSA", "EC", "ECDSA", "DSA", "Ed25519", "Ed448", "EdDSA",
                         "X25519", "X448", "XDH", "ML-DSA", "ML-KEM", "SLH-DSA",
                         "ML-DSA-44", "ML-KEM-512", "RSASSA-PSS", "DH", "TOTALLY-BOGUS-ALG"};
        for (String alg : algs) {
            String get, gen;
            KeyPairGenerator g = null;
            try { g = KeyPairGenerator.getInstance(alg); get = "OK"; }
            catch (Throwable t) { get = t.getClass().getSimpleName(); }
            if (g == null) { gen = "-"; }
            else {
                try { gen = "OK len=" + g.generateKeyPair().getPublic().getEncoded().length; }
                catch (Throwable t) { gen = t.getClass().getSimpleName(); }
            }
            System.out.printf("%-20s getInstance=%-24s generateKeyPair=%s%n", alg, get, gen);
        }
    }
}
