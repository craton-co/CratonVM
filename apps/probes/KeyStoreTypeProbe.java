import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.security.KeyStore;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/** `KeyStore.getInstance("JCEKS")` — how big is the gap, actually?
 *
 *  `HANDOFF-20260828-SCOPE.md` §4 carries it as "unclaimed; NOT a `--jdk-only`
 *  item, missing in both modes". That is a status, not a size. This probe asks
 *  the same questions of all four store types side by side, so the answer is a
 *  DIFFERENCE between JCEKS and the types that do work rather than a bare
 *  "unsupported" — and so that fixing it has a target.
 *
 *  Everything here is self-contained: the keystores are built in memory, and
 *  the one certificate comes from the JDK's own default trust store, so the
 *  probe needs no fixture file and no network.
 *
 *  DETERMINISM: the trust-store certificate is chosen by sorting the aliases
 *  and taking the first, never by iteration order.
 */
public class KeyStoreTypeProbe {

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

    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            System.out.println("SECTION-ABORTED " + name + " " + e.getClass().getName());
        }
    }

    static final char[] PW = "changeit".toCharArray();
    static final String[] TYPES = {"JKS", "PKCS12", "JCEKS", "jceks"};

    /** One certificate, taken from the JDK's own cacerts by sorted alias so the
     *  choice cannot depend on iteration order. */
    static Certificate sampleCert() throws Exception {
        String home = System.getProperty("java.home");
        for (String p : new String[] {"/lib/security/cacerts"}) {
            java.io.File f = new java.io.File(home + p);
            if (!f.isFile()) {
                continue;
            }
            for (String t : new String[] {"PKCS12", "JKS"}) {
                try (java.io.InputStream in = new java.io.FileInputStream(f)) {
                    KeyStore ks = KeyStore.getInstance(t);
                    ks.load(in, PW);
                    List<String> aliases = new ArrayList<>(Collections.list(ks.aliases()));
                    Collections.sort(aliases);
                    for (String a : aliases) {
                        Certificate c = ks.getCertificate(a);
                        if (c != null) {
                            return c;
                        }
                    }
                } catch (Throwable ignored) {
                    // try the next type
                }
            }
        }
        return null;
    }

    static void types() {
        for (String t : TYPES) {
            p("[" + t + "] getInstance", () -> KeyStore.getInstance(t).getType());
            p("[" + t + "] provider is present",
                () -> KeyStore.getInstance(t).getProvider() != null);
            p("[" + t + "] load(null) then size", () -> {
                KeyStore ks = KeyStore.getInstance(t);
                ks.load(null, PW);
                return ks.size();
            });
            p("[" + t + "] load(null) then aliases", () -> {
                KeyStore ks = KeyStore.getInstance(t);
                ks.load(null, PW);
                return Collections.list(ks.aliases()).size();
            });
            p("[" + t + "] store then reload is empty", () -> {
                KeyStore ks = KeyStore.getInstance(t);
                ks.load(null, PW);
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                ks.store(out, PW);
                KeyStore back = KeyStore.getInstance(t);
                back.load(new ByteArrayInputStream(out.toByteArray()), PW);
                return back.size();
            });
            p("[" + t + "] stored magic", () -> {
                KeyStore ks = KeyStore.getInstance(t);
                ks.load(null, PW);
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                ks.store(out, PW);
                byte[] b = out.toByteArray();
                if (b.length < 4) {
                    return "short:" + b.length;
                }
                return String.format("%02x%02x%02x%02x", b[0], b[1], b[2], b[3]);
            });
            p("[" + t + "] cert round trip", () -> {
                Certificate c = sampleCert();
                if (c == null) {
                    return "no sample cert";
                }
                KeyStore ks = KeyStore.getInstance(t);
                ks.load(null, PW);
                ks.setCertificateEntry("probe", c);
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                ks.store(out, PW);
                KeyStore back = KeyStore.getInstance(t);
                back.load(new ByteArrayInputStream(out.toByteArray()), PW);
                Certificate got = back.getCertificate("probe");
                return back.size() + "/" + back.containsAlias("probe") + "/"
                        + back.isCertificateEntry("probe") + "/"
                        + (got != null && got.equals(c));
            });
            p("[" + t + "] unknown alias", () -> {
                KeyStore ks = KeyStore.getInstance(t);
                ks.load(null, PW);
                return String.valueOf(ks.getCertificate("no-such-alias"));
            });
            p("[" + t + "] containsAlias on empty", () -> {
                KeyStore ks = KeyStore.getInstance(t);
                ks.load(null, PW);
                return ks.containsAlias("anything");
            });
            p("[" + t + "] getInstance with a bogus provider", () -> {
                try {
                    return KeyStore.getInstance(t, "NoSuchProvider").getType();
                } catch (Throwable e) {
                    return e.getClass().getName();
                }
            });
            p("[" + t + "] size before load", () -> {
                try {
                    return KeyStore.getInstance(t).size();
                } catch (Throwable e) {
                    return e.getClass().getName();
                }
            });
        }
        p("getDefaultType", () -> KeyStore.getDefaultType());
        p("getInstance of an unknown type", () -> {
            try {
                return KeyStore.getInstance("NO-SUCH-STORE").getType();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("getInstance null", () -> {
            try {
                return KeyStore.getInstance((String) null).getType();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("a sample cert was found", () -> sampleCert() != null);
        p("sample cert type", () -> {
            Certificate c = sampleCert();
            return c == null ? "none" : c.getType();
        });
        p("CertificateFactory X.509 round trip", () -> {
            Certificate c = sampleCert();
            if (c == null) {
                return "none";
            }
            CertificateFactory cf = CertificateFactory.getInstance("X.509");
            Certificate back = cf.generateCertificate(
                new ByteArrayInputStream(c.getEncoded()));
            return back.equals(c);
        });
    }

    public static void main(String[] args) {
        sect("types", KeyStoreTypeProbe::types);
        System.out.println("rows " + rows);
        System.out.println("DONE KeyStoreTypeProbe");
    }
}
