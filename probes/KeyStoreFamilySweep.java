import java.io.*;
import java.security.*;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.util.*;
import javax.crypto.spec.SecretKeySpec;

/** `KeyStore` across PKCS12, JKS and JCEKS, aimed by the survey's own prior.
 *
 *  `native-builtins/src/keystore.rs` says, above the four FQNs it registers on:
 *
 *      "PKCS12 + JKS share the same `engine*` surface; we register on each FQN
 *       explicitly because dispatch is keyed by class name"
 *
 *  Every defect this survey has found sat in a family whose registrar carried a
 *  stated justification that had drifted from its code. PKCS12 and JKS do NOT
 *  share the same surface, and the places they part are exactly what this asks:
 *
 *    * JKS LOWERCASES every alias; PKCS12 preserves case.
 *    * JKS refuses a `SecretKey` entry; PKCS12 and JCEKS accept one.
 *    * JKS refuses a null store password on `store`; PKCS12 permits it.
 *    * an UNINITIALIZED store must throw `KeyStoreException` from every
 *      operation, in all three types alike.
 *
 *  DETERMINISM: the certificate is a FIXED self-signed RSA cert embedded below,
 *  so no key is generated at run time and no date, serial or signature varies
 *  between the two VMs. Certificates are compared by SHA-256 fingerprint, never
 *  by `toString` -- a DN renders differently per VM and a cert's dates render
 *  per locale and time zone. Nothing prints an identity hash.
 */
public class KeyStoreFamilySweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    static final char[] PWD = "changeit".toCharArray();
    static final String CERT_B64 = ""
        + "MIIC5zCCAc+gAwIBAgIIHQB7vHky3McwDQYJKoZIhvcNAQEMBQAwITEPMA0GA1UEChMGY3JhdG9u"
        + "MQ4wDAYDVQQDEwVwcm9iZTAgFw0yNjA4MjcxNDAyMDVaGA8yMTI2MDgwMzE0MDIwNVowITEPMA0G"
        + "A1UEChMGY3JhdG9uMQ4wDAYDVQQDEwVwcm9iZTCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoC"
        + "ggEBAMckQVB8O+c+tY5abFzdoVy+PNCTJLMA8posDhXFqKgoRve23b68m7uaJ7Eppq6f8nKQerPw"
        + "KRJ3q7KPJSxY5q69bD5uFkbVI6dMTjzOMC/DJ0QuhCiBoks0gnEDpl5V9plvFL6F1mUnq7Jl24vy"
        + "w6sELy/VHmgpwPrKiVIgrUHc+YTLRHnY/Q9KE5Ce/UK/PG0lESSWol1YnDYARqhFCyfmBh3EzJAV"
        + "QwRmLjqiiK1o4z18L8Hk4l2NhgSvjQNMXeD7S1SgLUDKOG+3M6ef7eV3fW0M/OOW2SVD50/qFKZW"
        + "Ecn1MFTPNX4nA3VT+v7bZBeXBh0/ZpwHyMymj5UvR0sCAwEAAaMhMB8wHQYDVR0OBBYEFMDdpWAi"
        + "gm3TEtyquSOgj1VwCr9XMA0GCSqGSIb3DQEBDAUAA4IBAQCQTBxNXXMSkLDBdf72ZyOwOZTUMQyX"
        + "MalF4EPx8v/XkIkhAMRvu+VFNQvrrfABX1NS6sVcpJ7kRBOS7/XTGIXnStmH0qfvG98NUXF5Yifd"
        + "NMDjfUM+bICHoborRjS8i1OO0nBxrHd65rVlcWGCo+qBHp+nTGxIk6ENixxspWdn0Q4g3i6uPShb"
        + "OqwgrN7LpAn8n/4BzawHU2U+eymzX/VZxS9P8iPIYnSIaApwYdNTmAt4M65NEc/nGwiZ9j7BoAzl"
        + "ydQy58buwcGDONA1rMq1MaYUzawyNYDXP9DxvxhFpHeDl7iuC2odu/uz20QJxlU5j3RZWN4rfW18"
        + "AXHgORyp";

    static Certificate cert() throws Exception {
        byte[] der = Base64.getDecoder().decode(CERT_B64);
        return CertificateFactory.getInstance("X.509")
            .generateCertificate(new ByteArrayInputStream(der));
    }
    /** A cert is identified by its fingerprint, never by its rendered DN. */
    static String fp(Certificate c) throws Exception {
        if (c == null) return "null";
        byte[] d = MessageDigest.getInstance("SHA-256").digest(c.getEncoded());
        StringBuilder s = new StringBuilder();
        for (byte b : d) s.append(String.format("%02x", b));
        return s.toString();
    }
    static String sortedAliases(KeyStore ks) throws Exception {
        List<String> l = Collections.list(ks.aliases());
        Collections.sort(l);
        return l.toString();
    }

    /** Everything a store answers with a single TRUSTED CERT entry in it. */
    static void certEntry(String type) throws Exception {
        String k = "[" + type + " cert]";
        KeyStore ks;
        try { ks = KeyStore.getInstance(type); }
        catch (Throwable e) { p(k + " getInstance", "THREW " + e.getClass().getName()); return; }
        p(k + " getType", ks.getType());
        p(k + " provider is named", ks.getProvider() != null);
        ks.load(null, null);
        p(k + " empty size", ks.size());
        p(k + " empty aliases", sortedAliases(ks));

        Certificate c = cert();
        ks.setCertificateEntry("MixedCaseAlias", c);
        p(k + " size after set", ks.size());
        // THE JKS/PKCS12 SPLIT: JKS lowercases every alias it stores.
        p(k + " aliases", sortedAliases(ks));
        p(k + " containsAlias exact", ks.containsAlias("MixedCaseAlias"));
        p(k + " containsAlias lower", ks.containsAlias("mixedcasealias"));
        p(k + " containsAlias upper", ks.containsAlias("MIXEDCASEALIAS"));
        p(k + " isCertificateEntry exact", ks.isCertificateEntry("MixedCaseAlias"));
        p(k + " isKeyEntry exact", ks.isKeyEntry("MixedCaseAlias"));
        p(k + " getCertificate fp", fp(ks.getCertificate("MixedCaseAlias")));
        p(k + " getCertificateAlias", ks.getCertificateAlias(c));
        p(k + " getCertificateChain is null", ks.getCertificateChain("MixedCaseAlias") == null);
        p(k + " creationDate non-null", ks.getCreationDate("MixedCaseAlias") != null);
        p(k + " getKey on cert entry", ks.getKey("MixedCaseAlias", PWD));
        p(k + " getEntry class",
          ks.getEntry("MixedCaseAlias", null) == null
            ? "null" : ks.getEntry("MixedCaseAlias", null).getClass().getSimpleName());

        // absent-alias answers must be uniform across the three types
        p(k + " containsAlias absent", ks.containsAlias("nope"));
        p(k + " getCertificate absent", ks.getCertificate("nope"));
        p(k + " getCertificateChain absent", ks.getCertificateChain("nope"));
        p(k + " getCreationDate absent", ks.getCreationDate("nope"));
        p(k + " isKeyEntry absent", ks.isKeyEntry("nope"));
        p(k + " isCertificateEntry absent", ks.isCertificateEntry("nope"));
        p(k + " getKey absent", ks.getKey("nope", PWD));
        t(k + " deleteEntry absent", () -> ks.deleteEntry("nope"));

        // round-trip through bytes, which is where a format really differs
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        ks.store(out, PWD);
        p(k + " stored bytes > 0", out.size() > 0);
        KeyStore back = KeyStore.getInstance(type);
        back.load(new ByteArrayInputStream(out.toByteArray()), PWD);
        p(k + " reloaded size", back.size());
        p(k + " reloaded aliases", sortedAliases(back));
        p(k + " reloaded cert fp", fp(back.getCertificate(sortedAliases(back)
            .replaceAll("^\\[|\\]$", ""))));
        t(k + " reload with wrong password", () ->
            KeyStore.getInstance(type).load(new ByteArrayInputStream(out.toByteArray()),
                                            "wrong".toCharArray()));
        // JKS demands a store password; PKCS12 permits a null one.
        t(k + " store with null password", () ->
            ks.store(new ByteArrayOutputStream(), null));

        ks.deleteEntry(sortedAliases(ks).replaceAll("^\\[|\\]$", ""));
        p(k + " size after delete", ks.size());
    }

    /** A SecretKey entry: accepted by PKCS12 and JCEKS, refused by JKS. */
    static void secretKeyEntry(String type) throws Exception {
        String k = "[" + type + " secret]";
        KeyStore ks;
        try { ks = KeyStore.getInstance(type); }
        catch (Throwable e) { p(k + " getInstance", "THREW " + e.getClass().getName()); return; }
        ks.load(null, null);
        SecretKeySpec sk = new SecretKeySpec(new byte[16], "AES");
        t(k + " setKeyEntry(SecretKey)", () -> ks.setKeyEntry("sk", sk, PWD, null));
        p(k + " size", ks.size());
        p(k + " containsAlias sk", ks.containsAlias("sk"));
        p(k + " isKeyEntry sk", ks.isKeyEntry("sk"));
        p(k + " isCertificateEntry sk", ks.isCertificateEntry("sk"));
        if (ks.containsAlias("sk")) {
            Key got = null;
            String cls = "THREW";
            try { got = ks.getKey("sk", PWD); cls = got == null ? "null" : got.getAlgorithm(); }
            catch (Throwable e) { cls = "THREW " + e.getClass().getName(); }
            p(k + " getKey algorithm", cls);
            p(k + " getKey encoded len",
              got == null || got.getEncoded() == null ? "null" : got.getEncoded().length);
        }
    }

    /** An uninitialized store must refuse everything, identically everywhere. */
    static void uninitialized(String type) {
        String k = "[" + type + " uninit]";
        KeyStore ks;
        try { ks = KeyStore.getInstance(type); }
        catch (Throwable e) { p(k + " getInstance", "THREW " + e.getClass().getName()); return; }
        t(k + " size", () -> ks.size());
        t(k + " aliases", () -> ks.aliases());
        t(k + " containsAlias", () -> ks.containsAlias("a"));
        t(k + " getCertificate", () -> ks.getCertificate("a"));
        t(k + " getKey", () -> ks.getKey("a", PWD));
        t(k + " deleteEntry", () -> ks.deleteEntry("a"));
        t(k + " store", () -> ks.store(new ByteArrayOutputStream(), PWD));
        t(k + " setCertificateEntry", () -> ks.setCertificateEntry("a", cert()));
    }

    /** Type naming and the factory's own refusals. */
    static void factory() {
        p("getDefaultType", KeyStore.getDefaultType());
        t("getInstance lowercase pkcs12", () -> KeyStore.getInstance("pkcs12"));
        t("getInstance lowercase jks", () -> KeyStore.getInstance("jks"));
        t("getInstance unknown type", () -> KeyStore.getInstance("NoSuchStoreType"));
        t("getInstance null type", () -> KeyStore.getInstance((String) null));
        t("getInstance unknown provider", () -> KeyStore.getInstance("PKCS12", "NoSuchProv"));
        try {
            p("lowercase pkcs12 getType", KeyStore.getInstance("pkcs12").getType());
            p("lowercase jks getType", KeyStore.getInstance("jks").getType());
        } catch (Throwable e) {
            p("lowercase getType", "THREW " + e.getClass().getName());
        }
        // null-alias arguments: the JDK throws NPE from some and not others,
        // and a shim that normalises the alias up front loses that split.
        for (String type : new String[]{"PKCS12", "JKS"}) {
            try {
                KeyStore ks = KeyStore.getInstance(type);
                ks.load(null, null);
                t("[" + type + "] containsAlias(null)", () -> ks.containsAlias(null));
                t("[" + type + "] getCertificate(null)", () -> ks.getCertificate(null));
                t("[" + type + "] isKeyEntry(null)", () -> ks.isKeyEntry(null));
                t("[" + type + "] setCertificateEntry(null, c)",
                  () -> ks.setCertificateEntry(null, cert()));
                t("[" + type + "] setCertificateEntry(a, null)",
                  () -> ks.setCertificateEntry("a", null));
            } catch (Throwable e) {
                p("[" + type + "] null-alias setup", "THREW " + e.getClass().getName());
            }
        }
    }

    public static void main(String[] a) throws Exception {
        p("embedded cert fp", fp(cert()));
        p("embedded cert type", cert().getType());
        factory();
        for (String type : new String[]{"PKCS12", "JKS", "JCEKS"}) {
            certEntry(type);
            secretKeyEntry(type);
            uninitialized(type);
        }
        System.out.println("DONE KeyStoreFamilySweep");
    }
}
