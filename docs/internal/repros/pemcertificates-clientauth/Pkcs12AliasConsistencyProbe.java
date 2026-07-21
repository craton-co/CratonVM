import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.Signature;
import java.security.cert.Certificate;
import java.util.Enumeration;

// Directly tests whether each PrivateKeyEntry alias in test.p12 produces a
// SELF-CONSISTENT (certificate, private key) pair under CratonVM: sign a
// fixed message with the alias's private key, verify against the SAME
// alias's certificate's public key. A real cert/key mismatch (e.g. from a
// PKCS12 bag-pairing bug) would fail signature verification here,
// independent of any TLS/rustls/Netty involvement.
public class Pkcs12AliasConsistencyProbe {
    public static void main(String[] args) throws Exception {
        KeyStore ks = KeyStore.getInstance("PKCS12");
        try (java.io.FileInputStream fis = new java.io.FileInputStream("pemcerts/test.p12")) {
            ks.load(fis, "secret".toCharArray());
        }
        byte[] message = "consistency-check-message".getBytes();
        Enumeration<String> aliases = ks.aliases();
        while (aliases.hasMoreElements()) {
            String alias = aliases.nextElement();
            if (!ks.isKeyEntry(alias)) {
                continue;
            }
            PrivateKey key = (PrivateKey) ks.getKey(alias, "secret".toCharArray());
            Certificate cert = ks.getCertificate(alias);
            PublicKey pub = cert.getPublicKey();
            Signature signer = Signature.getInstance("SHA256withRSA");
            signer.initSign(key);
            signer.update(message);
            byte[] sig = signer.sign();

            Signature verifier = Signature.getInstance("SHA256withRSA");
            verifier.initVerify(pub);
            verifier.update(message);
            boolean ok = verifier.verify(sig);
            System.out.println("alias=" + alias + " key.getFormat()=" + key.getFormat()
                    + " keyEncodedLen=" + key.getEncoded().length
                    + " certEncodedLen=" + cert.getEncoded().length
                    + " SELF-CONSISTENT=" + ok);
        }

        // Cross-check: sign with "spring-boot"'s key, verify against
        // "test-alias"'s cert (and vice versa) -- should FAIL if the two
        // aliases carry genuinely different key material (as the known-issue
        // doc's RSA-modulus comparison implies), confirming they are
        // independent identities, not two names for the same key.
        PrivateKey kSb = (PrivateKey) ks.getKey("spring-boot", "secret".toCharArray());
        PublicKey pTa = ks.getCertificate("test-alias").getPublicKey();
        Signature crossSigner = Signature.getInstance("SHA256withRSA");
        crossSigner.initSign(kSb);
        crossSigner.update(message);
        byte[] crossSig = crossSigner.sign();
        Signature crossVerifier = Signature.getInstance("SHA256withRSA");
        crossVerifier.initVerify(pTa);
        crossVerifier.update(message);
        boolean crossOk = crossVerifier.verify(crossSig);
        System.out.println("CROSS spring-boot-key vs test-alias-cert verify=" + crossOk
                + " (expected false if aliases are genuinely different identities)");
    }
}
