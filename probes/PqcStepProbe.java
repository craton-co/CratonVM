import java.security.*;
import java.security.spec.NamedParameterSpec;

/**
 * The PQC key path, one step per line, so a failure names the step that failed.
 * `PqcProbe` wrapped getInstance + initialize + generateKeyPair in one try, which
 * cannot distinguish "the algorithm is not offered" from "the generator exists
 * but cannot generate" — and the known-issue page and I disagreed about exactly
 * that.
 */
public final class PqcStepProbe {
    static void step(String key, String outcome) { System.out.println(key + " -> " + outcome); }

    static String desc(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }

    static void run(String alg, String pname) {
        String tag = alg + (pname == null ? "" : "/" + pname);
        KeyPairGenerator g;
        try {
            g = KeyPairGenerator.getInstance(alg);
            step("1.getInstance(" + tag + ")", "OK cls=" + g.getClass().getName()
                    + " provider=" + (g.getProvider() == null ? "null" : g.getProvider().getName()));
        } catch (Throwable t) { step("1.getInstance(" + tag + ")", "THREW " + desc(t)); return; }

        if (pname != null) {
            try {
                g.initialize(new NamedParameterSpec(pname), new SecureRandom());
                step("2.initialize(" + tag + ")", "OK");
            } catch (Throwable t) { step("2.initialize(" + tag + ")", "THREW " + desc(t)); return; }
        }

        KeyPair kp;
        try {
            kp = g.generateKeyPair();
            step("3.generateKeyPair(" + tag + ")", "OK");
        } catch (Throwable t) { step("3.generateKeyPair(" + tag + ")", "THREW " + desc(t)); return; }

        try {
            PublicKey pub = kp.getPublic();
            step("4.publicKey(" + tag + ")", "alg=" + pub.getAlgorithm()
                    + " cls=" + pub.getClass().getName()
                    + " len=" + (pub.getEncoded() == null ? -1 : pub.getEncoded().length));
        } catch (Throwable t) { step("4.publicKey(" + tag + ")", "THREW " + desc(t)); }
    }

    public static void main(String[] a) {
        run("ML-DSA", null);
        run("ML-DSA", "ML-DSA-44");
        run("ML-DSA", "ML-DSA-65");
        run("ML-DSA", "ML-DSA-87");
        run("ML-DSA-44", null);
        run("ML-KEM", null);
        run("ML-KEM", "ML-KEM-512");
        run("ML-KEM-512", null);
        run("SLH-DSA", null);
    }
}
