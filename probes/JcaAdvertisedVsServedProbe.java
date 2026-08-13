import java.security.AlgorithmParameters;
import java.security.KeyFactory;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.MessageDigest;
import java.security.Provider;
import java.security.SecureRandom;
import java.security.Security;
import java.security.Signature;
import java.security.cert.CertificateFactory;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Set;
import java.util.TreeSet;
import javax.crypto.Cipher;
import javax.crypto.KeyAgreement;
import javax.crypto.KeyGenerator;
import javax.crypto.Mac;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.SecretKeySpec;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManagerFactory;

/**
 * The advertised-versus-served census. W7-63-jca-advertise-vs-serve.md.
 *
 * For EVERY algorithm the provider chain advertises through
 * `Security.getAlgorithms(type)`, this actually requests it and prints what
 * happened. Where the engine can produce a deterministic result cheaply it
 * prints the BYTES. No line here is a verdict; every line is an observation,
 * so the CratonVM output and the HotSpot output can be diffed directly.
 *
 * The species being hunted has three shapes and this probe separates them:
 *
 *   ADVERTISE-BUT-REFUSE     `getAlgorithms` lists it, `getInstance` throws.
 *                            Section A prints `THREW` on an advertised name.
 *   ADVERTISE-BUT-MIS-SERVE  `getInstance` succeeds and the bytes are wrong.
 *                            Only section C can see this: section A would print
 *                            a perfectly healthy-looking `OK`.
 *   SERVE-BUT-NEVER-ADVERTISE `getAlgorithms` does not list it and
 *                            `getInstance` returns an object anyway. Section B
 *                            probes names no provider can possibly carry.
 *
 * Two traps, both already paid for in this campaign, both designed around here:
 *
 *  1. A ROUND TRIP CANNOT CATCH A WRONG ALGORITHM. Encrypt-then-decrypt with
 *     the same wrong cipher succeeds and proves nothing. Section C therefore
 *     prints raw digest/MAC hex against published known-answer vectors rather
 *     than round-tripping anything.
 *  2. COMPARING TWO REFUSALS REPORTS `true`. A probe that asks "do X and Y
 *     agree" and gets `NoSuchAlgorithmException` from both compares one
 *     exception name with itself and answers `true` -- which reads exactly like
 *     the defect it is hunting. `sameBytes` below answers `n/a` unless both
 *     sides produced real bytes, following `probes/CryptoTrioProbe.java`.
 */
public final class JcaAdvertisedVsServedProbe {

    /** Engine types walked in section A, in `Security.getAlgorithms` spelling. */
    private static final String[] ENGINE_TYPES = {
        "MessageDigest",
        "Mac",
        "Cipher",
        "Signature",
        "KeyFactory",
        "KeyPairGenerator",
        "KeyGenerator",
        "SecretKeyFactory",
        "SecureRandom",
        "KeyStore",
        "AlgorithmParameters",
        "CertificateFactory",
        "KeyAgreement",
        "KeyManagerFactory",
        "TrustManagerFactory",
        "SSLContext",
    };

    public static void main(String[] args) {
        line("java.version", System.getProperty("java.version"));
        line("java.vm.name", System.getProperty("java.vm.name"));
        providerChain();
        System.out.println();
        sectionA_advertisedIsServed();
        System.out.println();
        sectionB_servedWithoutBeingAdvertised();
        System.out.println();
        sectionC_knownAnswerVectors();
        System.out.println();
        sectionD_setShape();
        System.out.println("DONE JcaAdvertisedVsServedProbe");
    }

    // ---- provider chain ----------------------------------------------------

    private static void providerChain() {
        StringBuilder sb = new StringBuilder();
        Provider[] ps = Security.getProviders();
        for (int i = 0; i < ps.length; i++) {
            if (i > 0) {
                sb.append(',');
            }
            sb.append(ps[i].getName());
        }
        line("providers.n", Integer.toString(ps.length));
        line("providers", sb.toString());
    }

    // ---- section A: every advertised name, actually requested --------------

    private static void sectionA_advertisedIsServed() {
        System.out.println("== A: advertised -> served ==");
        for (String type : ENGINE_TYPES) {
            Set<String> advertised;
            try {
                advertised = new TreeSet<String>(Security.getAlgorithms(type));
            } catch (Throwable t) {
                line("A." + type + ".getAlgorithms", thrown(t));
                continue;
            }
            line("A." + type + ".n", Integer.toString(advertised.size()));
            for (String algo : advertised) {
                line("A." + type + "[" + algo + "]", request(type, algo));
            }
        }
    }

    /**
     * Request one (type, algorithm) and describe the outcome in observable
     * terms. `OK ...` means `getInstance` returned an object and the trailing
     * text is what that object then reported; `THREW ...` names the exception
     * class and message verbatim.
     *
     * Where the engine can be driven to a DETERMINISTIC result without key
     * material -- digest length and bytes, MAC length and bytes, generated key
     * length, block size -- it is driven, because `getInstance` succeeding is
     * exactly the observation that cannot distinguish a served algorithm from a
     * mis-served one.
     */
    private static String request(String type, String algo) {
        try {
            if (type.equals("MessageDigest")) {
                MessageDigest md = MessageDigest.getInstance(algo);
                byte[] empty = md.digest(new byte[0]);
                md.reset();
                byte[] abc = md.digest("abc".getBytes("UTF-8"));
                return "OK len=" + md.getDigestLength()
                        + " d(\"\")=" + hex(empty)
                        + " d(abc)=" + hex(abc)
                        + " prov=" + providerName(md.getProvider());
            }
            if (type.equals("Mac")) {
                Mac m = Mac.getInstance(algo);
                String tag;
                try {
                    // A fixed 32-byte key. Some PBE MACs reject a raw key --
                    // that is an InvalidKeyException, not an algorithm gap, and
                    // it is reported as itself rather than folded into a
                    // refusal.
                    byte[] key = new byte[32];
                    Arrays.fill(key, (byte) 0x0b);
                    m.init(new SecretKeySpec(key, algo));
                    byte[] out = m.doFinal("abc".getBytes("UTF-8"));
                    tag = "len=" + out.length + " t(abc)=" + hex(out);
                } catch (Throwable t) {
                    tag = "init/doFinal " + thrown(t);
                }
                return "OK maclen=" + m.getMacLength() + " " + tag
                        + " prov=" + providerName(m.getProvider());
            }
            if (type.equals("Cipher")) {
                Cipher c = Cipher.getInstance(algo);
                return "OK alg=" + c.getAlgorithm() + " block=" + c.getBlockSize()
                        + " prov=" + providerName(c.getProvider());
            }
            if (type.equals("Signature")) {
                Signature s = Signature.getInstance(algo);
                return "OK alg=" + s.getAlgorithm() + " prov=" + providerName(s.getProvider());
            }
            if (type.equals("KeyFactory")) {
                KeyFactory kf = KeyFactory.getInstance(algo);
                return "OK alg=" + kf.getAlgorithm() + " prov=" + providerName(kf.getProvider());
            }
            if (type.equals("KeyPairGenerator")) {
                KeyPairGenerator kpg = KeyPairGenerator.getInstance(algo);
                return "OK alg=" + kpg.getAlgorithm() + " prov=" + providerName(kpg.getProvider());
            }
            if (type.equals("KeyGenerator")) {
                KeyGenerator kg = KeyGenerator.getInstance(algo);
                String made;
                try {
                    byte[] enc = kg.generateKey().getEncoded();
                    // The LENGTH is the observation. `KeyGenerator` stores the
                    // algorithm string and, on a defective engine, never reads
                    // it again -- so every name yields the same default size.
                    made = "keylen=" + (enc == null ? -1 : enc.length);
                } catch (Throwable t) {
                    made = "generateKey " + thrown(t);
                }
                return "OK alg=" + kg.getAlgorithm() + " " + made
                        + " prov=" + providerName(kg.getProvider());
            }
            if (type.equals("SecretKeyFactory")) {
                SecretKeyFactory f = SecretKeyFactory.getInstance(algo);
                return "OK alg=" + f.getAlgorithm() + " prov=" + providerName(f.getProvider());
            }
            if (type.equals("SecureRandom")) {
                SecureRandom sr = SecureRandom.getInstance(algo);
                byte[] b = new byte[8];
                sr.nextBytes(b);
                // Deliberately NOT printing the bytes: they are supposed to
                // differ run to run. All-zeroes after nextBytes would be the
                // defect, and `allZero` reports exactly that much.
                return "OK alg=" + sr.getAlgorithm() + " drew8.allZero=" + allZero(b)
                        + " prov=" + providerName(sr.getProvider());
            }
            if (type.equals("KeyStore")) {
                KeyStore ks = KeyStore.getInstance(algo);
                return "OK type=" + ks.getType() + " prov=" + providerName(ks.getProvider());
            }
            if (type.equals("AlgorithmParameters")) {
                AlgorithmParameters ap = AlgorithmParameters.getInstance(algo);
                return "OK alg=" + ap.getAlgorithm() + " prov=" + providerName(ap.getProvider());
            }
            if (type.equals("CertificateFactory")) {
                CertificateFactory cf = CertificateFactory.getInstance(algo);
                return "OK type=" + cf.getType() + " prov=" + providerName(cf.getProvider());
            }
            if (type.equals("KeyAgreement")) {
                KeyAgreement ka = KeyAgreement.getInstance(algo);
                return "OK alg=" + ka.getAlgorithm() + " prov=" + providerName(ka.getProvider());
            }
            if (type.equals("KeyManagerFactory")) {
                KeyManagerFactory f = KeyManagerFactory.getInstance(algo);
                return "OK alg=" + f.getAlgorithm() + " prov=" + providerName(f.getProvider());
            }
            if (type.equals("TrustManagerFactory")) {
                TrustManagerFactory f = TrustManagerFactory.getInstance(algo);
                return "OK alg=" + f.getAlgorithm() + " prov=" + providerName(f.getProvider());
            }
            if (type.equals("SSLContext")) {
                SSLContext sc = SSLContext.getInstance(algo);
                return "OK proto=" + sc.getProtocol() + " prov=" + providerName(sc.getProvider());
            }
            return "SKIPPED no driver for engine type";
        } catch (Throwable t) {
            return "THREW " + thrown(t);
        }
    }

    // ---- section B: the reverse direction ----------------------------------

    /**
     * Names that no provider in any chain advertises. Every one of these must
     * be refused at `getInstance`. An `OK` line in this section is the
     * serve-but-never-advertise shape -- the direction a census comparing two
     * lists structurally cannot see, because the advertised list never moves.
     */
    private static final String[][] BOGUS = {
        {"MessageDigest", "NO-SUCH-DIGEST"},
        {"MessageDigest", "BLAKE2"},
        {"MessageDigest", ""},
        {"Mac", "NO-SUCH-MAC"},
        {"Mac", "AES"},
        {"Cipher", "NO-SUCH-CIPHER"},
        {"Cipher", "SHA-256"},
        {"Signature", "NO-SUCH-SIG"},
        {"Signature", "ML-KEM"},
        {"Signature", "AES"},
        {"Signature", "HmacSHA256"},
        {"Signature", ""},
        {"KeyFactory", "NO-SUCH-KF"},
        {"KeyGenerator", "NO-SUCH-KG"},
        {"SecureRandom", "NO-SUCH-PRNG"},
        {"CertificateFactory", "PKCS7"},
        {"CertificateFactory", "AES"},
        {"CertificateFactory", ""},
        {"AlgorithmParameters", "NO-SUCH-PARAMS"},
        {"KeyPairGenerator", "NO-SUCH-KPG"},
        {"SecretKeyFactory", "NO-SUCH-SKF"},
        {"KeyAgreement", "NO-SUCH-KA"},
    };

    private static void sectionB_servedWithoutBeingAdvertised() {
        System.out.println("== B: names no provider advertises ==");
        for (String[] row : BOGUS) {
            String type = row[0];
            String algo = row[1];
            boolean listed = Security.getAlgorithms(type).contains(algo.toUpperCase(java.util.Locale.ROOT));
            line("B." + type + "[" + algo + "]",
                    "advertised=" + listed + " " + request(type, algo));
        }
    }

    // ---- section C: known answers -----------------------------------------

    /**
     * Published known-answer vectors. This is the only section that can catch
     * ADVERTISE-BUT-MIS-SERVE, and it is here rather than as a round trip
     * because a round trip through a wrong algorithm succeeds.
     *
     * `expected` is the HotSpot 25 / published value; the probe prints BOTH the
     * expected and the observed hex so a reader sees the bytes rather than a
     * boolean. `match` is derived, and it is `n/a` when nothing was produced --
     * comparing a refusal against a refusal reports equality, which is the
     * defect wearing the instrument's clothes.
     */
    private static final String[][] DIGEST_KAT = {
        // algorithm, len, digest(""), digest("abc")
        {"MD2", "16",
            "8350e5a3e24c153df2275c9f80692773",
            "da853b0d3f88d99b30283a69e6ded6bb"},
        {"MD5", "16",
            "d41d8cd98f00b204e9800998ecf8427e",
            "900150983cd24fb0d6963f7d28e17f72"},
        {"SHA-256", "32",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"},
        {"SHA3-256", "32",
            "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a",
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"},
        {"SHAKE128-256", "32",
            "7f9c2ba4e88f827d616045507605853ed73b8093f6efbc88eb1a6eacfa66ef26",
            "5881092dd818bf5cf8a3ddb793fbcba74097d5c526a6d35f97b83351940f2cc8"},
        {"SHAKE256-512", "64",
            "46b9dd2b0ba88d13233b3feb743eeb243fcd52ea62b81b82b50c27646ed5762f"
                + "d75dc4ddd8c0f200cb05019d67b592f6fc821c49479ab48640292eacb3b7c4be",
            "483366601360a8771c6863080cc4114d8db44530f8f1e1ee4f94ea37e78b5739"
                + "d5a15bef186a5386c75744c0527e1faa9f8726e462a12a4feb06bd8801e751e4"},
        // The two ALIASES. On HotSpot these resolve and return bytes identical
        // to the hyphenated primary names, while `getAlgorithms` lists only the
        // primaries -- so "not in the advertised set" is the CORRECT state for
        // these two, and refusing them would be a different defect.
        {"SHAKE128", "32",
            "7f9c2ba4e88f827d616045507605853ed73b8093f6efbc88eb1a6eacfa66ef26",
            "5881092dd818bf5cf8a3ddb793fbcba74097d5c526a6d35f97b83351940f2cc8"},
        {"SHAKE256", "64",
            "46b9dd2b0ba88d13233b3feb743eeb243fcd52ea62b81b82b50c27646ed5762f"
                + "d75dc4ddd8c0f200cb05019d67b592f6fc821c49479ab48640292eacb3b7c4be",
            "483366601360a8771c6863080cc4114d8db44530f8f1e1ee4f94ea37e78b5739"
                + "d5a15bef186a5386c75744c0527e1faa9f8726e462a12a4feb06bd8801e751e4"},
    };

    private static void sectionC_knownAnswerVectors() {
        System.out.println("== C: known-answer vectors (a round trip cannot catch a wrong algorithm) ==");
        for (String[] row : DIGEST_KAT) {
            String algo = row[0];
            String gotEmpty;
            String gotAbc;
            String gotLen;
            try {
                MessageDigest md = MessageDigest.getInstance(algo);
                gotLen = Integer.toString(md.getDigestLength());
                gotEmpty = hex(md.digest(new byte[0]));
                md.reset();
                gotAbc = hex(md.digest("abc".getBytes("UTF-8")));
            } catch (Throwable t) {
                gotLen = thrown(t);
                gotEmpty = null;
                gotAbc = null;
            }
            line("C.md[" + algo + "].len", "want=" + row[1] + " got=" + gotLen);
            line("C.md[" + algo + "].empty",
                    "want=" + row[2] + " got=" + (gotEmpty == null ? "-" : gotEmpty)
                            + " match=" + sameBytes(gotEmpty, row[2]));
            line("C.md[" + algo + "].abc",
                    "want=" + row[3] + " got=" + (gotAbc == null ? "-" : gotAbc)
                            + " match=" + sameBytes(gotAbc, row[3]));
        }

        // The digest DEFAULT-ARM tell. An engine whose unknown-algorithm arm
        // falls back to SHA-256 answers a made-up name with SHA-256's bytes.
        // Printing them beside the real SHA-256 makes the fallback visible
        // without asserting anything: if these two lines carry the same hex,
        // the name was discarded.
        line("C.md[SHA-256].control", digestOrThrow("SHA-256", "abc"));
        line("C.md[NO-SUCH-DIGEST].fallbackTell", digestOrThrow("NO-SUCH-DIGEST", "abc"));
        line("C.md.fallbackEqualsSha256",
                sameBytes(digestHexOrNull("NO-SUCH-DIGEST", "abc"),
                        digestHexOrNull("SHA-256", "abc")));

        // Same shape one engine over: HmacSHA224 is 28 bytes on HotSpot. A Mac
        // engine with an HMAC-SHA-256 default arm answers 32 and corroborates
        // itself through getMacLength().
        String h256 = macHexOrNull("HmacSHA256");
        String h224 = macHexOrNull("HmacSHA224");
        line("C.mac[HmacSHA256]", h256 == null ? "-" : h256);
        line("C.mac[HmacSHA224]", h224 == null ? "-" : h224);
        line("C.mac.224equals256", sameBytes(h224, h256));
    }

    private static String digestOrThrow(String algo, String msg) {
        try {
            MessageDigest md = MessageDigest.getInstance(algo);
            return "len=" + md.getDigestLength() + " " + hex(md.digest(msg.getBytes("UTF-8")));
        } catch (Throwable t) {
            return thrown(t);
        }
    }

    private static String digestHexOrNull(String algo, String msg) {
        try {
            return hex(MessageDigest.getInstance(algo).digest(msg.getBytes("UTF-8")));
        } catch (Throwable t) {
            return null;
        }
    }

    private static String macHexOrNull(String algo) {
        try {
            Mac m = Mac.getInstance(algo);
            byte[] key = new byte[32];
            Arrays.fill(key, (byte) 0x0b);
            m.init(new SecretKeySpec(key, algo));
            return "len=" + m.getMacLength() + " " + hex(m.doFinal("abc".getBytes("UTF-8")));
        } catch (Throwable t) {
            return null;
        }
    }

    // ---- section D: the shape of the advertised set ------------------------

    private static void sectionD_setShape() {
        System.out.println("== D: the shape of the answer ==");
        shapeOf("MessageDigest");
        shapeOf("NoSuchEngineType");
        shapeOf("");
        try {
            Set<String> s = Security.getAlgorithms(null);
            line("D.getAlgorithms[null]", "n=" + s.size() + " class=" + s.getClass().getName());
        } catch (Throwable t) {
            line("D.getAlgorithms[null]", thrown(t));
        }
        line("D.getAlgorithms.twoCallsSameObject",
                Boolean.toString(Security.getAlgorithms("MessageDigest")
                        == Security.getAlgorithms("MessageDigest")));

        Provider sun = Security.getProvider("SUN");
        if (sun == null) {
            line("D.SUN", "absent");
            return;
        }
        Set<Provider.Service> svc = sun.getServices();
        line("D.SUN.getServices", "n=" + svc.size() + " class=" + svc.getClass().getName());
        try {
            svc.add(null);
            line("D.SUN.getServices.add", "SUCCEEDED (mutable)");
        } catch (UnsupportedOperationException e) {
            line("D.SUN.getServices.add", "UnsupportedOperationException");
        } catch (Throwable t) {
            line("D.SUN.getServices.add", thrown(t));
        }
    }

    private static void shapeOf(String type) {
        Set<String> s;
        try {
            s = Security.getAlgorithms(type);
        } catch (Throwable t) {
            line("D.getAlgorithms[" + type + "]", thrown(t));
            return;
        }
        String cls = s.getClass().getName();
        String add;
        try {
            s.add("ZZZ-PROBE");
            add = "SUCCEEDED (mutable)";
        } catch (UnsupportedOperationException e) {
            add = "UnsupportedOperationException";
        } catch (Throwable t) {
            add = thrown(t);
        }
        line("D.getAlgorithms[" + type + "]", "n=" + s.size() + " class=" + cls + " add=" + add);
    }

    // ---- helpers -----------------------------------------------------------

    /**
     * `true`/`false` only when BOTH operands are real hex; `n/a` when either is
     * absent. Two refusals are equal as strings and reporting that as `true`
     * would claim two algorithms agree when in fact neither ran -- this
     * campaign's dominant failure shape appearing inside the instrument built
     * to detect it. See `probes/CryptoTrioProbe.java`'s `sameCipher`.
     */
    private static String sameBytes(String a, String b) {
        if (a == null || b == null || a.isEmpty() || b.isEmpty()) {
            return "n/a";
        }
        return Boolean.toString(a.equals(b));
    }

    private static boolean allZero(byte[] b) {
        for (int i = 0; i < b.length; i++) {
            if (b[i] != 0) {
                return false;
            }
        }
        return true;
    }

    private static String providerName(Provider p) {
        return p == null ? "null" : p.getName();
    }

    private static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder(b.length * 2);
        for (int i = 0; i < b.length; i++) {
            sb.append(Character.forDigit((b[i] >> 4) & 0xf, 16));
            sb.append(Character.forDigit(b[i] & 0xf, 16));
        }
        return sb.toString();
    }

    private static String thrown(Throwable t) {
        String m = t.getMessage();
        return t.getClass().getName() + (m == null ? "" : ": " + m);
    }

    private static void line(String k, String v) {
        System.out.println(k + " = " + v);
    }

    private JcaAdvertisedVsServedProbe() {}
}
