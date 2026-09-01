import java.security.KeyPair;
import java.security.KeyPairGenerator;
import javax.crypto.KeyAgreement;
import java.util.HexFormat;

/**
 * A full Diffie-Hellman key agreement, end to end.
 *
 * `jca-provider-population-gap-20260830.md` measured that SunJCE's
 * `KeyAgreement` type is absent here, and `JcaGapSizer` then showed its one
 * implementation class -- `com.sun.crypto.provider.DHKeyAgreement` -- LOADS on
 * this VM exactly as it does on HotSpot. That makes the gap look like a missing
 * table row rather than missing code, which is how JCEKS turned out.
 *
 * "Looks like" is not a result. This probe is the difference: if registering
 * the service is genuinely all that is missing, then after the registration
 * both parties here derive the SAME shared secret, and the bytes match
 * HotSpot's for the same key pair. If registration is necessary but not
 * sufficient -- because the SPI needs a native this VM lacks, or its BigInteger
 * path is wrong -- this fails AFTER the service resolves, which is a completely
 * different and much larger finding than "one line is missing".
 *
 * The keys are generated, not fixed, so the secret differs run to run. What must
 * agree is the two parties WITH EACH OTHER, plus the length -- so the assertions
 * are agreement and size, not a golden hex string.
 */
public final class DhAgree {
    static int n = 0;
    static void p(String l, Object v) { System.out.println(++n + " " + l + " |" + v + "|"); }

    public static void main(String[] args) {
        try {
            KeyPairGenerator kpg = KeyPairGenerator.getInstance("DiffieHellman");
            kpg.initialize(2048);
            p("KeyPairGenerator provider", kpg.getProvider().getName());
            KeyPair a = kpg.generateKeyPair();
            KeyPair b = kpg.generateKeyPair();
            p("keypairs generated", a.getPublic() != null && b.getPublic() != null);

            KeyAgreement ka = KeyAgreement.getInstance("DiffieHellman");
            p("KeyAgreement provider", ka.getProvider().getName());
            p("KeyAgreement class", ka.getClass().getName());

            ka.init(a.getPrivate());
            ka.doPhase(b.getPublic(), true);
            byte[] s1 = ka.generateSecret();

            KeyAgreement kb = KeyAgreement.getInstance("DiffieHellman");
            kb.init(b.getPrivate());
            kb.doPhase(a.getPublic(), true);
            byte[] s2 = kb.generateSecret();

            p("secret length", s1.length);
            p("both parties agree", java.util.Arrays.equals(s1, s2));
            p("secret is not all zero", !HexFormat.of().formatHex(s1).chars().allMatch(c -> c == '0'));
        } catch (Throwable t) {
            p("FAILED", t.getClass().getName() + ": " + t.getMessage());
        }
    }
}
