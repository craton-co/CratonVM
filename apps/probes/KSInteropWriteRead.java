import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.cert.Certificate;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/** Does a keystore one VM WRITES round-trip through the other VM's reader,
 *  using the ENTRY password the writer was given?
 *
 *  `LTKeyStoreEngineSweep` measures one VM writing and the same VM reading, so
 *  a writer and a reader that agree with each other and disagree with the
 *  specification look correct. This asks the cross product instead. The row
 *  that matters is `getKey(alias, KEYPW)` on a store written with
 *  `setKeyEntry(alias, key, KEYPW, chain)` and stored under a DIFFERENT
 *  password `PW`: PKCS#12 shrouds each key bag with the ENTRY password, so
 *  KEYPW must recover the key and PW must not.
 *
 *  Deterministic across VMs: no key bytes are printed (a generated key differs
 *  every run), only which password recovers the key and what the recovered
 *  key's algorithm and format are.
 *
 *  usage: KSInteropWriteRead write <type> <path>
 *         KSInteropWriteRead read  <type> <path>
 */
public class KSInteropWriteRead {

    static final char[] PW = "changeit".toCharArray();
    static final char[] KEYPW = "keypass".toCharArray();
    static final char[] WRONG = "wrong".toCharArray();

    static Certificate anyCert() throws Exception {
        String home = System.getProperty("java.home");
        KeyStore ca = KeyStore.getInstance("JKS");
        try (FileInputStream in = new FileInputStream(home + "/lib/security/cacerts")) {
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
        throw new IllegalStateException("no certificate in cacerts");
    }

    static void write(String type, String path) throws Exception {
        KeyPairGenerator g = KeyPairGenerator.getInstance("RSA");
        g.initialize(2048);
        KeyPair kp = g.generateKeyPair();
        PrivateKey priv = kp.getPrivate();
        Certificate cert = anyCert();
        KeyStore ks = KeyStore.getInstance(type);
        ks.load(null, null);
        ks.setKeyEntry("k", priv, KEYPW, new Certificate[] {cert});
        try (FileOutputStream out = new FileOutputStream(path)) {
            ks.store(out, PW);
        }
        System.out.println("[" + type + "] wrote |ok|");
    }

    /** `getAlgorithm()`/`getFormat()` are what the KEY OBJECT claims. They
     *  cannot tell a recovered key from the ciphertext dressed as one, because
     *  a mirror object answers them from a field. `usable` asks the question
     *  the claim cannot fake: does `getEncoded()` parse back as a PKCS#8
     *  private key? Only real recovered key material does. */
    static String usable(java.security.Key k) {
        try {
            byte[] enc = k.getEncoded();
            if (enc == null) {
                return "no-encoding";
            }
            java.security.KeyFactory.getInstance("RSA")
                .generatePrivate(new java.security.spec.PKCS8EncodedKeySpec(enc));
            return "usable";
        } catch (Throwable e) {
            return "NOT-A-KEY";
        }
    }

    /** Each ask gets its OWN freshly loaded store. CratonVM's `getKey` unlocks
     *  the entry in a process-wide side table, so a first ask with the right
     *  password leaves plaintext behind and every ask after it reads as
     *  "usable" whatever password it passed. Asking three questions of one
     *  KeyStore object measured the ORDER of the questions. */
    static void ask(KeyStore ks, String type, String label, char[] pw) {
        String v;
        try {
            java.security.Key k = ks.getKey("k", pw);
            v = k == null ? "null" : k.getAlgorithm() + "/" + k.getFormat() + "/" + usable(k);
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName();
        }
        System.out.println("[" + type + "] getKey with " + label + " |" + v + "|");
    }

    static void read(String type, String path) throws Exception {
        KeyStore ks = KeyStore.getInstance(type);
        try (FileInputStream in = new FileInputStream(path)) {
            ks.load(in, PW);
        }
        System.out.println("[" + type + "] loaded, aliases |"
                           + Collections.list(ks.aliases()) + "|");
        System.out.println("[" + type + "] isKeyEntry |" + ks.isKeyEntry("k") + "|");
        ask(fresh(type, path), type, "a WRONG password  ", WRONG);
        ask(fresh(type, path), type, "the STORE password", PW);
        ask(fresh(type, path), type, "the ENTRY password", KEYPW);
    }

    static KeyStore fresh(String type, String path) throws Exception {
        KeyStore ks = KeyStore.getInstance(type);
        try (FileInputStream in = new FileInputStream(path)) {
            ks.load(in, PW);
        }
        return ks;
    }

    public static void main(String[] args) throws Exception {
        if (args[0].equals("write")) {
            write(args[1], args[2]);
        } else {
            read(args[1], args[2]);
        }
    }
}
