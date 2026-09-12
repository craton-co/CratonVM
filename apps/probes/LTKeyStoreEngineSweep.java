import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.security.Key;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.cert.Certificate;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Date;
import java.util.List;

/** Lane T — the `engine*` half of the keystore surface `KeyStoreTypeProbe` does
 *  not reach.
 *
 *  `native-builtins/src/keystore.rs` registers SEVENTEEN `engine*` methods over
 *  EIGHT `KeyStoreSpi` implementations (136 rows, lane T's second-largest
 *  cross-cutting registrar). `KeyStoreTypeProbe` exercises roughly seven of the
 *  seventeen — `engineLoad`, `engineStore`, `engineSetCertificateEntry`,
 *  `engineGetCertificate`, `engineAliases`, `engineSize`,
 *  `engineContainsAlias` — and every one of its rows is byte-identical with
 *  `CRATONVM_ENFORCE_NATIVE_SHADOW` armed over the eight classes.
 *
 *  The ten it does NOT reach are the private-key half: `engineGetKey`,
 *  `engineSetKeyEntry`, `engineGetCertificateChain`, `engineIsKeyEntry`,
 *  `engineIsCertificateEntry`, `engineGetCertificateAlias`,
 *  `engineGetCreationDate`, `engineDeleteEntry`, `engineGetEntry` and
 *  `engineSetEntry`. That half is where the encryption is, and a keystore that
 *  hands back the WRONG key is the failure shape lane 4's page calls "a loud
 *  failure turned into a rare silent one" — so a retirement priced on the first
 *  seven is priced on the easy half.
 *
 *  WHAT IS COMPARABLE. Key material is deterministic only if it is not
 *  generated: a fresh `KeyPairGenerator` produces different bytes on every run
 *  and on every VM, so nothing here prints a key's ENCODING. What it prints is
 *  the round trip — store a key under an alias, reload the store from its own
 *  bytes, and ask whether the key that comes back `equals` the one that went
 *  in, whether its algorithm and format survived, and whether the chain came
 *  back the same length with the same certificate. Those are properties the
 *  specification fixes and a wrong-key defect breaks.
 *
 *  DETERMINISM: no key bytes, no dates (a creation date is asked as "non-null
 *  and not in the future"), no iteration order — aliases are sorted.
 */
public class LTKeyStoreEngineSweep {

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            v = String.valueOf(f.get());
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(tag + " |" + v + "|");
    }

    /** Each row runs on its own so a throw in one cannot hide the quiet wrong
     *  answers below it — the rule the l8-tail throwable batch earned. */
    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            System.out.println("SECTION-ABORTED " + name + " " + e.getClass().getName()
                               + ": " + e.getMessage());
        }
    }

    static final char[] PW = "changeit".toCharArray();
    static final char[] KEYPW = "keypass".toCharArray();
    static final String[] TYPES = {"JKS", "PKCS12", "JCEKS"};

    /** One certificate from the JDK's own cacerts, chosen by SORTED alias so
     *  the choice cannot depend on iteration order. */
    static Certificate sampleCert() throws Exception {
        String home = System.getProperty("java.home");
        java.io.File f = new java.io.File(home + "/lib/security/cacerts");
        if (!f.isFile()) {
            return null;
        }
        KeyStore ca = KeyStore.getInstance("JKS");
        try (java.io.FileInputStream in = new java.io.FileInputStream(f)) {
            ca.load(in, "changeit".toCharArray());
        }
        List<String> aliases = new ArrayList<>(Collections.list(ca.aliases()));
        Collections.sort(aliases);
        for (String a : aliases) {
            Certificate c = ca.getCertificate(a);
            if (c != null) {
                return c;
            }
        }
        return null;
    }

    static KeyPair keyPair() throws Exception {
        KeyPairGenerator g = KeyPairGenerator.getInstance("RSA");
        g.initialize(2048);
        return g.generateKeyPair();
    }

    static byte[] bytes(KeyStore ks, char[] pw) throws Exception {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        ks.store(out, pw);
        return out.toByteArray();
    }

    static KeyStore reload(String type, byte[] b, char[] pw) throws Exception {
        KeyStore ks = KeyStore.getInstance(type);
        ks.load(new ByteArrayInputStream(b), pw);
        return ks;
    }

    public static void main(String[] args) throws Exception {
        final Certificate cert = sampleCert();
        p("fixture: a certificate is available", () -> cert != null);
        if (cert == null) {
            System.out.println("rows " + rows);
            System.out.println("DONE LTKeyStoreEngineSweep");
            return;
        }

        KeyPair kp = null;
        Throwable kpFail = null;
        try {
            kp = keyPair();
        } catch (Throwable t) {
            kpFail = t;
        }
        final Throwable kpErr = kpFail;
        p("fixture: an RSA key pair", () -> kpErr == null ? "ok"
                                                         : "THREW " + kpErr.getClass().getName());
        if (kp == null) {
            System.out.println("rows " + rows);
            System.out.println("DONE LTKeyStoreEngineSweep");
            return;
        }
        final PrivateKey priv = kp.getPrivate();
        p("fixture: private key algorithm", () -> priv.getAlgorithm());
        p("fixture: private key format", () -> priv.getFormat());

        for (String type : TYPES) {
            final String t = type;
            sect(t, () -> {
                // --- the key half: set, store, reload, get -----------------
                p("[" + t + "] setKeyEntry then size", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
                    return ks.size();
                });
                p("[" + t + "] isKeyEntry / isCertificateEntry before store", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
                    return ks.isKeyEntry("k") + "/" + ks.isCertificateEntry("k");
                });
                p("[" + t + "] key survives a store/reload round trip", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    Key got = back.getKey("k", KEYPW);
                    return (got != null) + "/" + (got != null && got.equals(priv))
                           + "/" + (got == null ? "-" : got.getAlgorithm())
                           + "/" + (got == null ? "-" : got.getFormat());
                });
                p("[" + t + "] the chain survives with it", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    Certificate[] chain = back.getCertificateChain("k");
                    return chain == null ? "null"
                                         : chain.length + "/" + chain[0].equals(cert);
                });
                p("[" + t + "] getKey with the WRONG password", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    return back.getKey("k", "wrong".toCharArray());
                });
                p("[" + t + "] getKey for an alias that is a CERT entry", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setCertificateEntry("c", cert);
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    return back.getKey("c", KEYPW);
                });
                p("[" + t + "] getKey for an alias that does not exist", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    return ks.getKey("nope", KEYPW);
                });
                // --- the entry API ----------------------------------------
                p("[" + t + "] getEntry returns a PrivateKeyEntry", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    KeyStore.Entry e =
                        back.getEntry("k", new KeyStore.PasswordProtection(KEYPW));
                    return e == null ? "null" : e.getClass().getSimpleName();
                });
                p("[" + t + "] setEntry then the key comes back equal", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setEntry("k",
                        new KeyStore.PrivateKeyEntry(priv, new Certificate[] {cert}),
                        new KeyStore.PasswordProtection(KEYPW));
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    Key got = back.getKey("k", KEYPW);
                    return got != null && got.equals(priv);
                });
                // --- alias bookkeeping ------------------------------------
                p("[" + t + "] getCertificateAlias finds the entry it stored", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setCertificateEntry("c", cert);
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    return back.getCertificateAlias(cert);
                });
                p("[" + t + "] getCreationDate is set and not in the future", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setCertificateEntry("c", cert);
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    Date d = back.getCreationDate("c");
                    return d != null && d.getTime() <= System.currentTimeMillis() + 60000L;
                });
                p("[" + t + "] deleteEntry removes exactly one alias", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setCertificateEntry("c", cert);
                    ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
                    ks.deleteEntry("c");
                    List<String> a = new ArrayList<>(Collections.list(ks.aliases()));
                    Collections.sort(a);
                    return ks.size() + "/" + a;
                });
                p("[" + t + "] deleteEntry on an absent alias", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.deleteEntry("nope");
                    return "ok/" + ks.size();
                });
                p("[" + t + "] aliases after a reload, sorted", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setCertificateEntry("c", cert);
                    ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    List<String> a = new ArrayList<>(Collections.list(back.aliases()));
                    Collections.sort(a);
                    return a.toString();
                });
                p("[" + t + "] entry kinds after a reload", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setCertificateEntry("c", cert);
                    ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
                    KeyStore back = reload(t, bytes(ks, PW), PW);
                    return back.isCertificateEntry("c") + "/" + back.isKeyEntry("c")
                           + "/" + back.isCertificateEntry("k") + "/" + back.isKeyEntry("k");
                });
                p("[" + t + "] a store reloaded with the WRONG password", () -> {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(null, null);
                    ks.setCertificateEntry("c", cert);
                    byte[] b = bytes(ks, PW);
                    return reload(t, b, "nope".toCharArray()).size();
                });
            });
        }

        System.out.println("rows " + rows);
        System.out.println("DONE LTKeyStoreEngineSweep");
    }
}
