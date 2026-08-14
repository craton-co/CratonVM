import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.spec.PKCS8EncodedKeySpec;
import java.security.spec.X509EncodedKeySpec;

/**
 * What `Key.getEncoded()` actually hands back, per algorithm.
 *
 * `probes/KpgEndToEnd` reports only a LENGTH, and three algorithms come out
 * short against HotSpot JDK 25 (RSA 294 vs 422, EC 91 vs 120, RSASSA-PSS 294
 * vs 420). A length says something is wrong and nothing about what, so this
 * prints the parts a length cannot:
 *
 *   * `getFormat()` — "X.509" for a public key, "PKCS#8" for a private one.
 *     A key that answers the right format and the wrong bytes is a different
 *     defect from one that admits it is not encoded.
 *   * the OUTER DER shape, decoded far enough to say whether the
 *     `AlgorithmIdentifier` envelope is there at all: a `SubjectPublicKeyInfo`
 *     is `SEQUENCE { SEQUENCE { OID, params }, BIT STRING }`, while the bare
 *     PKCS#1 `RSAPublicKey` it is often confused with is
 *     `SEQUENCE { INTEGER, INTEGER }`. Those are distinguishable from the
 *     first two bytes of the first inner element, with no ASN.1 library.
 *   * whether the bytes ROUND-TRIP through `KeyFactory`. That is the property
 *     every real consumer depends on — `X509EncodedKeySpec`, a CSR builder, a
 *     JWK serialiser — and the one a length cannot show.
 *
 * Every line must print identically under `java` and `cratonvm`.
 */
public final class KeyEncodingProbe {

    static String hex(byte[] b, int max) {
        StringBuilder sb = new StringBuilder();
        int n = Math.min(b.length, max);
        for (int i = 0; i < n; i++) {
            sb.append(Character.forDigit((b[i] >> 4) & 0xf, 16));
            sb.append(Character.forDigit(b[i] & 0xf, 16));
        }
        if (b.length > max) {
            sb.append("..");
        }
        return sb.toString();
    }

    /** Tag name for a DER tag byte, enough to tell the shapes apart. */
    static String tagName(int tag) {
        switch (tag & 0xff) {
            case 0x02: return "INTEGER";
            case 0x03: return "BIT_STRING";
            case 0x04: return "OCTET_STRING";
            case 0x05: return "NULL";
            case 0x06: return "OID";
            case 0x30: return "SEQUENCE";
            case 0x31: return "SET";
            default: return String.format("tag_%02x", tag & 0xff);
        }
    }

    /** Length of the DER header (tag + length bytes) at `off`, and the content length. */
    static int[] header(byte[] d, int off) {
        int len = d[off + 1] & 0xff;
        if (len < 0x80) {
            return new int[] {2, len};
        }
        int n = len & 0x7f;
        int v = 0;
        for (int i = 0; i < n; i++) {
            v = (v << 8) | (d[off + 2 + i] & 0xff);
        }
        return new int[] {2 + n, v};
    }

    /**
     * The `AlgorithmIdentifier` SEQUENCE, in hex — the one part of an encoded
     * key that is FIXED for a given algorithm and therefore comparable between
     * two VMs that each generated their own key pair.
     *
     * This is where the identity lives, and where this probe found its defect:
     * `rsaEncryption` is `300d 06092a864886f70d010101 0500` while
     * `id-RSASSA-PSS` is `300b 06092a864886f70d01010a` (different OID, and no
     * NULL parameters). CratonVM stamped the first on both, so a PSS key could
     * not be re-imported by the `$PSS` KeyFactory that requires the second.
     *
     * In a `SubjectPublicKeyInfo` it is child 0; in a PKCS#8 `PrivateKeyInfo`
     * it is child 1, after the version INTEGER. Everything after it is key
     * material and differs per key, which is exactly why this probe prints
     * NEITHER raw bytes nor exact lengths: a DER INTEGER's leading-zero byte
     * comes and goes with the value, so two correct encodings of two different
     * keys differ by a byte or two and a length diff reads as a defect.
     */
    static String algId(byte[] d, boolean isPrivate) {
        try {
            int[] h = header(d, 0);
            int off = h[0];
            if (isPrivate) {
                int[] v = header(d, off);
                off += v[0] + v[1];
            }
            int[] a = header(d, off);
            return hex(java.util.Arrays.copyOfRange(d, off, off + a[0] + a[1]), 64);
        } catch (Throwable t) {
            return "UNPARSEABLE:" + t.getClass().getSimpleName();
        }
    }

    /**
     * The outer SEQUENCE's direct children, as tag names — which is all it
     * takes to separate a SubjectPublicKeyInfo from a bare key.
     */
    static String shape(byte[] d) {
        try {
            if ((d[0] & 0xff) != 0x30) {
                return "NOT_A_SEQUENCE:" + tagName(d[0]);
            }
            int[] h = header(d, 0);
            int off = h[0];
            int end = off + h[1];
            StringBuilder sb = new StringBuilder("SEQUENCE{");
            boolean first = true;
            while (off < end && off < d.length) {
                int[] ch = header(d, off);
                if (!first) {
                    sb.append(',');
                }
                first = false;
                sb.append(tagName(d[off]));
                off += ch[0] + ch[1];
            }
            return sb.append('}').toString();
        } catch (Throwable t) {
            return "UNPARSEABLE:" + t.getClass().getSimpleName();
        }
    }

    static void report(String alg, PublicKey pub, PrivateKey priv) {
        line(alg + ".pub.class", pub.getClass().getName());
        line(alg + ".pub.algorithm", pub.getAlgorithm());
        line(alg + ".pub.format", String.valueOf(pub.getFormat()));
        byte[] pe = pub.getEncoded();
        if (pe == null) {
            line(alg + ".pub.encoded", "null");
        } else {
            line(alg + ".pub.encoded.shape", shape(pe));
            line(alg + ".pub.encoded.algId", algId(pe, false));
        }

        line(alg + ".priv.class", priv.getClass().getName());
        line(alg + ".priv.algorithm", priv.getAlgorithm());
        line(alg + ".priv.format", String.valueOf(priv.getFormat()));
        byte[] se = priv.getEncoded();
        if (se == null) {
            line(alg + ".priv.encoded", "null");
        } else {
            line(alg + ".priv.encoded.shape", shape(se));
            line(alg + ".priv.encoded.algId", algId(se, true));
        }

        // The property every consumer actually needs. `getEncoded()` is only
        // useful if it comes back.
        String kfAlg = alg.equals("RSASSA-PSS") ? "RSASSA-PSS" : alg;
        try {
            KeyFactory kf = KeyFactory.getInstance(kfAlg);
            PublicKey back = kf.generatePublic(new X509EncodedKeySpec(pe));
            line(alg + ".pub.roundTrip", "OK sameBytes="
                    + java.util.Arrays.equals(pe, back.getEncoded()));
        } catch (Throwable t) {
            line(alg + ".pub.roundTrip", t.getClass().getName());
        }
        try {
            KeyFactory kf = KeyFactory.getInstance(kfAlg);
            PrivateKey back = kf.generatePrivate(new PKCS8EncodedKeySpec(se));
            line(alg + ".priv.roundTrip", "OK sameBytes="
                    + java.util.Arrays.equals(se, back.getEncoded()));
        } catch (Throwable t) {
            line(alg + ".priv.roundTrip", t.getClass().getName());
        }
    }

    static void line(String key, Object value) {
        System.out.println(key + "=" + value);
    }

    /**
     * What an UNINITIALISED generator produces — which is what
     * `probes/KpgEndToEnd` measures, and the only thing that differs between
     * the two VMs on the RSA/EC/RSASSA-PSS rows.
     *
     * A default is a policy choice, not an encoding: JDK 22 raised the default
     * RSA modulus and JDK 24 the default EC curve, so a VM that kept the older
     * defaults produces a SHORTER `getEncoded()` for the same algorithm with
     * nothing wrong with the bytes. Printing the STRENGTH beside the length is
     * what separates the two readings.
     */
    static void defaults() {
        for (String alg : new String[] {"RSA", "RSASSA-PSS", "EC", "DSA"}) {
            try {
                KeyPair kp = KeyPairGenerator.getInstance(alg).generateKeyPair();
                PublicKey pub = kp.getPublic();
                String strength;
                if (pub instanceof java.security.interfaces.RSAPublicKey) {
                    strength = "modulusBits="
                            + ((java.security.interfaces.RSAPublicKey) pub).getModulus().bitLength();
                } else if (pub instanceof java.security.interfaces.ECPublicKey) {
                    strength = "fieldBits="
                            + ((java.security.interfaces.ECPublicKey) pub).getParams()
                                    .getCurve().getField().getFieldSize();
                } else if (pub instanceof java.security.interfaces.DSAPublicKey) {
                    strength = "pBits="
                            + ((java.security.interfaces.DSAPublicKey) pub).getParams()
                                    .getP().bitLength();
                } else {
                    strength = "strength=?";
                }
                // Strength ONLY. The encoded length was the symptom that led
                // here (`probes/KpgEndToEnd`: RSA 294 against 422), but it is a
                // proxy — and a noisy one for DSA, whose public `y` gains or
                // loses a leading zero byte per key. The bit count is the
                // policy this row exists to pin.
                line("default." + alg, strength);
            } catch (Throwable t) {
                line("default." + alg, t.getClass().getName());
            }
        }
    }

    public static void main(String[] args) {
        defaults();
        for (String alg : new String[] {"RSA", "RSASSA-PSS", "EC", "DSA", "Ed25519", "X25519"}) {
            try {
                KeyPairGenerator g = KeyPairGenerator.getInstance(alg);
                if (alg.equals("RSA") || alg.equals("RSASSA-PSS") || alg.equals("DSA")) {
                    g.initialize(2048);
                } else if (alg.equals("EC")) {
                    g.initialize(256);
                }
                KeyPair kp = g.generateKeyPair();
                report(alg, kp.getPublic(), kp.getPrivate());
            } catch (Throwable t) {
                line(alg + ".FAILED", t.getClass().getName() + ": " + t.getMessage());
            }
        }
        System.out.println("KeyEncodingProbe done");
    }
}
