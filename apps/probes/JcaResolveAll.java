import java.io.BufferedReader;
import java.io.FileReader;

/**
 * For every service the enumeration says is missing, does `getInstance` refuse?
 *
 * The enumeration gap and the functional gap are different sizes, and reasoning
 * from the first to the second is exactly the error
 * `jca-provider-population-gap-20260830.md` made: 186 services are absent from
 * `provider.getServices()` here, but a 17-row functional sample found only 3
 * that actually refuse -- Diffie-Hellman runs a complete 2048-bit agreement
 * 0-diff against HotSpot despite its whole `KeyAgreement` type being unlisted.
 *
 * So this asks all 186 rather than a sample. Input is `JcaGapSizer`'s own
 * missing-service lines (`SVC <provider> <type> <alg>=<class>`); output is one
 * row per service saying whether `getInstance(alg)` resolved, and through which
 * provider. Run it on both VMs: HotSpot is the control that says the ROW is
 * askable at all, so a row failing on both is a bad probe line rather than a
 * finding.
 */
public final class JcaResolveAll {
    public static void main(String[] args) throws Exception {
        int ok = 0, fail = 0, skip = 0;
        try (BufferedReader r = new BufferedReader(new FileReader(args[0]))) {
            String line;
            while ((line = r.readLine()) != null) {
                String[] parts = line.trim().split("\s+");
                if (parts.length < 4 || !parts[0].equals("SVC")) continue;
                String type = parts[2];
                String alg = parts[3].contains("=") ? parts[3].substring(0, parts[3].indexOf('=')) : parts[3];
                String res;
                try {
                    Object o = get(type, alg);
                    if (o == null) { res = "SKIP unsupported-type"; skip++; }
                    else { res = "OK " + o; ok++; }
                } catch (Throwable t) {
                    res = t.getClass().getSimpleName() + ": " + t.getMessage();
                    fail++;
                }
                System.out.println("R " + type + " " + alg + " |" + res + "|");
            }
        }
        System.out.println("TOTAL ok=" + ok + " fail=" + fail + " skip=" + skip);
    }

    static Object get(String type, String a) throws Exception {
        switch (type) {
            case "Cipher": return javax.crypto.Cipher.getInstance(a).getProvider().getName();
            case "Mac": return javax.crypto.Mac.getInstance(a).getProvider().getName();
            case "SecretKeyFactory": return javax.crypto.SecretKeyFactory.getInstance(a).getProvider().getName();
            case "KeyGenerator": return javax.crypto.KeyGenerator.getInstance(a).getProvider().getName();
            case "KeyAgreement": return javax.crypto.KeyAgreement.getInstance(a).getProvider().getName();
            case "KeyFactory": return java.security.KeyFactory.getInstance(a).getProvider().getName();
            case "KeyPairGenerator": return java.security.KeyPairGenerator.getInstance(a).getProvider().getName();
            case "MessageDigest": return java.security.MessageDigest.getInstance(a).getProvider().getName();
            case "Signature": return java.security.Signature.getInstance(a).getProvider().getName();
            case "SecureRandom": return java.security.SecureRandom.getInstance(a).getProvider().getName();
            case "AlgorithmParameters": return java.security.AlgorithmParameters.getInstance(a).getProvider().getName();
            case "AlgorithmParameterGenerator": return java.security.AlgorithmParameterGenerator.getInstance(a).getProvider().getName();
            case "CertificateFactory": return java.security.cert.CertificateFactory.getInstance(a).getProvider().getName();
            case "KeyStore": return java.security.KeyStore.getInstance(a).getProvider().getName();
            default: return null; // KDF/KEM/Configuration need APIs this probe does not model
        }
    }
}
