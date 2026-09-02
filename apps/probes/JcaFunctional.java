import javax.crypto.Cipher;
import javax.crypto.KeyAgreement;
import javax.crypto.KeyGenerator;
import javax.crypto.Mac;
import javax.crypto.SecretKeyFactory;
import java.security.AlgorithmParameters;
import java.security.KeyFactory;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.security.Signature;

/**
 * Does a service ABSENT from `provider.getServices()` actually fail to resolve?
 *
 * This exists because it does not, and I asserted otherwise.
 * `jca-provider-population-gap-20260830.md` reasoned from an enumeration gap --
 * SunJCE lists 103 services here against HotSpot's 194 -- to a functional one:
 * "invisible until a program asks for an algorithm in one of the six missing
 * types and gets NoSuchAlgorithmException". Then a full Diffie-Hellman
 * agreement, whose `KeyAgreement` type is one of the six absent ones, ran
 * 0-diff against HotSpot: both parties agreed on a 256-byte secret.
 *
 * So `getInstance` reaches implementations the service map does not enumerate,
 * and the two questions have to be asked separately. This asks the FUNCTIONAL
 * one, across every type in the gap, by calling `getInstance` on algorithms the
 * enumeration says are missing and reporting which actually refuse.
 *
 * Each line prints the resolving provider on success, so a row that resolves
 * through a DIFFERENT provider than HotSpot uses is visible too rather than
 * being scored as a pass.
 */
public final class JcaFunctional {
    static int n = 0;

    static void row(String type, String alg, Try t) {
        String r;
        try {
            r = "OK provider=" + t.go();
        } catch (Throwable e) {
            r = e.getClass().getSimpleName() + ": " + e.getMessage();
        }
        System.out.println(++n + " " + type + " " + alg + " |" + r + "|");
    }

    interface Try { String go() throws Exception; }

    public static void main(String[] args) {
        // SunJCE types absent from getServices() entirely
        row("KeyAgreement", "DiffieHellman", () -> KeyAgreement.getInstance("DiffieHellman").getProvider().getName());
        row("SecretKeyFactory", "PBKDF2WithHmacSHA256", () -> SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256").getProvider().getName());
        row("SecretKeyFactory", "DES", () -> SecretKeyFactory.getInstance("DES").getProvider().getName());
        row("SecretKeyFactory", "PBEWithHmacSHA256AndAES_256", () -> SecretKeyFactory.getInstance("PBEWithHmacSHA256AndAES_256").getProvider().getName());
        row("Signature", "SHA256withDSA", () -> Signature.getInstance("SHA256withDSA").getProvider().getName());
        row("AlgorithmParameterGenerator", "DiffieHellman", () -> java.security.AlgorithmParameterGenerator.getInstance("DiffieHellman").getProvider().getName());

        // algorithms missing from types that ARE present
        row("Cipher", "AES/CBC/NoPadding", () -> Cipher.getInstance("AES/CBC/NoPadding").getProvider().getName());
        row("Cipher", "ChaCha20-Poly1305", () -> Cipher.getInstance("ChaCha20-Poly1305").getProvider().getName());
        row("Cipher", "DESede", () -> Cipher.getInstance("DESede").getProvider().getName());
        row("Mac", "HmacSHA256", () -> Mac.getInstance("HmacSHA256").getProvider().getName());
        row("Mac", "HmacPBESHA256", () -> Mac.getInstance("HmacPBESHA256").getProvider().getName());
        row("MessageDigest", "SHA3-256", () -> MessageDigest.getInstance("SHA3-256").getProvider().getName());
        row("MessageDigest", "SHA-512/256", () -> MessageDigest.getInstance("SHA-512/256").getProvider().getName());
        row("KeyGenerator", "HmacSHA512", () -> KeyGenerator.getInstance("HmacSHA512").getProvider().getName());
        row("KeyFactory", "DiffieHellman", () -> KeyFactory.getInstance("DiffieHellman").getProvider().getName());
        row("SecureRandom", "SHA1PRNG", () -> SecureRandom.getInstance("SHA1PRNG").getProvider().getName());
        row("AlgorithmParameters", "GCM", () -> AlgorithmParameters.getInstance("GCM").getProvider().getName());
    }
}
