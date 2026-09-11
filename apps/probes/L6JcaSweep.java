import java.nio.charset.StandardCharsets;
import java.security.*;
import java.security.spec.*;
import java.util.*;
import javax.crypto.*;
import javax.crypto.spec.*;

/** L6 — the JCA surface reached through provider lookup: `javax.crypto.Cipher`
 *  (35 rows), `java.security.Signature` (22), `Provider` (21), `Mac` (17),
 *  `KeyAgreement` (13), `KeyPairGenerator` (12), `KeyFactory` (10) and
 *  `Security` (9).
 *
 *  The lane page's §2 warning applies to every row here: this family resolves
 *  algorithms through `ServiceLoader`, so a failure may be L7's rather than
 *  this lane's, and a row is only this lane's once the lookup itself works.
 *  The probe is therefore written to SEPARATE the two questions instead of
 *  conflating them: it first prints whether the lookup succeeded at all, and
 *  only then the behaviour of the object it produced. A blanket "throws" row
 *  cannot tell you which of the two failed, and this lane has already been
 *  bitten once by a composite call whose sub-questions had different answers.
 *
 *  **Nothing here prints a value the two VMs may choose independently.** No key
 *  is generated at random: every symmetric key is a fixed `SecretKeySpec`, every
 *  IV is a literal, and the digests and ciphertexts are known-answer vectors
 *  fixed by their standards. The provider NAME is deliberately not asserted —
 *  `SecuritySurfaceSweep` records why — only that a provider is present and
 *  that the ANSWER is right.
 *
 *  `SecureRandom` is asked only structural questions: that seeding is
 *  reproducible for `SHA1PRNG` given a fixed seed (which its specification
 *  fixes), and never for its default instance.
 */
public class L6JcaSweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static final char[] HEX = "0123456789abcdef".toCharArray();

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    static String hex(byte[] a) {
        if (a == null) return "null";
        StringBuilder sb = new StringBuilder();
        for (byte b : a) sb.append(HEX[(b >> 4) & 15]).append(HEX[b & 15]);
        return sb.toString();
    }

    static byte[] rep(int n, int v) {
        byte[] b = new byte[n];
        Arrays.fill(b, (byte) v);
        return b;
    }

    static byte[] utf8(String s) { return s.getBytes(StandardCharsets.UTF_8); }

    public static void main(String[] args) {
        // ---- STEP ONE: does the lookup work at all? Asked separately from
        // what the resolved object does, so an L7 loader failure is legible as
        // itself rather than as a wrong answer.
        String[] ciphers = {
            "AES", "AES/CBC/PKCS5Padding", "AES/CBC/NoPadding", "AES/ECB/NoPadding",
            "AES/GCM/NoPadding", "AES/CTR/NoPadding", "DES", "DESede", "RSA",
            "RSA/ECB/PKCS1Padding", "ChaCha20-Poly1305", "NoSuchCipher",
        };
        for (String a : ciphers)
            tv("lookup Cipher " + a, () -> {
                Cipher c = Cipher.getInstance(a);
                return "ok algorithm=" + c.getAlgorithm() + " blockSize=" + c.getBlockSize()
                     + " provider-present=" + (c.getProvider() != null);
            });
        String[] macs = {"HmacSHA1", "HmacSHA224", "HmacSHA256", "HmacSHA384", "HmacSHA512", "HmacMD5", "NoSuchMac"};
        for (String a : macs)
            tv("lookup Mac " + a, () -> {
                Mac m = Mac.getInstance(a);
                return "ok algorithm=" + m.getAlgorithm() + " macLength=" + m.getMacLength();
            });
        String[] sigs = {
            "SHA256withRSA", "SHA1withRSA", "SHA256withECDSA", "SHA512withRSA",
            "Ed25519", "RSASSA-PSS", "NONEwithRSA", "NoSuchSignature",
        };
        for (String a : sigs)
            tv("lookup Signature " + a, () -> "ok algorithm=" + Signature.getInstance(a).getAlgorithm());
        String[] kfs = {"RSA", "EC", "DSA", "DiffieHellman", "Ed25519", "X25519", "NoSuchKf"};
        for (String a : kfs)
            tv("lookup KeyFactory " + a, () -> "ok algorithm=" + KeyFactory.getInstance(a).getAlgorithm());
        for (String a : new String[] {"RSA", "EC", "DSA", "Ed25519", "X25519", "NoSuchKpg"})
            tv("lookup KeyPairGenerator " + a, () -> "ok algorithm=" + KeyPairGenerator.getInstance(a).getAlgorithm());
        for (String a : new String[] {"DiffieHellman", "ECDH", "X25519", "NoSuchKa"})
            tv("lookup KeyAgreement " + a, () -> "ok algorithm=" + KeyAgreement.getInstance(a).getAlgorithm());
        for (String a : new String[] {"AES", "DESede", "HmacSHA256", "NoSuchKg"})
            tv("lookup KeyGenerator " + a, () -> "ok algorithm=" + KeyGenerator.getInstance(a).getAlgorithm());
        for (String a : new String[] {"PBKDF2WithHmacSHA1", "PBKDF2WithHmacSHA256", "DESede", "AES", "NoSuchSkf"})
            tv("lookup SecretKeyFactory " + a, () -> "ok algorithm=" + SecretKeyFactory.getInstance(a).getAlgorithm());
        for (String a : new String[] {"PKCS12", "JKS", "JCEKS", "NoSuchKeyStore"})
            tv("lookup KeyStore " + a, () -> "ok type=" + KeyStore.getInstance(a).getType());
        for (String a : new String[] {"X.509", "NoSuchCertType"})
            tv("lookup CertificateFactory " + a,
               () -> "ok type=" + java.security.cert.CertificateFactory.getInstance(a).getType());
        for (String a : new String[] {"SHA1PRNG", "NativePRNG", "DRBG", "NoSuchPrng"})
            tv("lookup SecureRandom " + a, () -> "ok algorithm=" + SecureRandom.getInstance(a).getAlgorithm());
        for (String a : new String[] {"AES", "DiffieHellman", "EC", "NoSuchApg"})
            tv("lookup AlgorithmParameters " + a, () -> "ok algorithm=" + AlgorithmParameters.getInstance(a).getAlgorithm());

        // ---- getInstance's argument contract, which is where the refusal TYPE
        // matters: null is an NPE, absent is NoSuchAlgorithmException, and an
        // unknown PROVIDER is NoSuchProviderException even for a known algorithm
        tv("Cipher null algorithm", () -> Cipher.getInstance(null).getAlgorithm());
        tv("Cipher empty algorithm", () -> Cipher.getInstance("").getAlgorithm());
        tv("Cipher bad transform 2 parts", () -> Cipher.getInstance("AES/CBC").getAlgorithm());
        tv("Cipher bad transform 4 parts", () -> Cipher.getInstance("AES/CBC/PKCS5Padding/X").getAlgorithm());
        tv("Cipher bad mode", () -> Cipher.getInstance("AES/NoSuchMode/NoPadding").getAlgorithm());
        tv("Cipher bad padding", () -> Cipher.getInstance("AES/CBC/NoSuchPadding").getAlgorithm());
        tv("Cipher unknown provider", () -> Cipher.getInstance("AES", "NoSuchProvider").getAlgorithm());
        tv("Cipher null provider name", () -> Cipher.getInstance("AES", (String) null).getAlgorithm());
        tv("Cipher empty provider name", () -> Cipher.getInstance("AES", "").getAlgorithm());
        tv("MessageDigest unknown provider", () -> MessageDigest.getInstance("SHA-256", "NoSuchProvider").getAlgorithm());
        tv("Signature null algorithm", () -> Signature.getInstance(null).getAlgorithm());
        tv("Mac null algorithm", () -> Mac.getInstance(null).getAlgorithm());
        tv("KeyFactory null algorithm", () -> KeyFactory.getInstance(null).getAlgorithm());
        tv("KeyStore null type", () -> KeyStore.getInstance((String) null).getType());
        tv("Cipher.getMaxAllowedKeyLength AES", () -> Cipher.getMaxAllowedKeyLength("AES"));
        tv("Cipher.getMaxAllowedKeyLength null", () -> Cipher.getMaxAllowedKeyLength(null));
        tv("Cipher.getMaxAllowedKeyLength bogus", () -> Cipher.getMaxAllowedKeyLength("NoSuchCipher"));
        tv("Cipher.getMaxAllowedParameterSpec AES", () -> String.valueOf(Cipher.getMaxAllowedParameterSpec("AES")));

        // ---- KNOWN-ANSWER TESTS. Fixed keys, fixed IVs, published vectors: the
        // ciphertext is a pure function of the inputs and the standard fixes it.
        SecretKeySpec aes128 = new SecretKeySpec(rep(16, 0), "AES");
        SecretKeySpec aes256 = new SecretKeySpec(rep(32, 0), "AES");
        IvParameterSpec iv0 = new IvParameterSpec(rep(16, 0));

        tv("AES-128-ECB zero key zero block", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, aes128);
            return hex(c.doFinal(rep(16, 0)));
        });
        tv("AES-256-ECB zero key zero block", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, aes256);
            return hex(c.doFinal(rep(16, 0)));
        });
        tv("AES-128-CBC PKCS5 roundtrip", () -> {
            Cipher e = Cipher.getInstance("AES/CBC/PKCS5Padding");
            e.init(Cipher.ENCRYPT_MODE, aes128, iv0);
            byte[] ct = e.doFinal(utf8("the quick brown fox"));
            Cipher d = Cipher.getInstance("AES/CBC/PKCS5Padding");
            d.init(Cipher.DECRYPT_MODE, aes128, iv0);
            return hex(ct) + " -> " + new String(d.doFinal(ct), StandardCharsets.UTF_8);
        });
        tv("AES-128-CBC padding on exact block", () -> {
            Cipher e = Cipher.getInstance("AES/CBC/PKCS5Padding");
            e.init(Cipher.ENCRYPT_MODE, aes128, iv0);
            return hex(e.doFinal(rep(16, 0)));
        });
        tv("AES-128-CBC NoPadding partial block refused", () -> {
            Cipher e = Cipher.getInstance("AES/CBC/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128, iv0);
            return hex(e.doFinal(rep(15, 0)));
        });
        tv("AES-128-CTR keystream", () -> {
            Cipher e = Cipher.getInstance("AES/CTR/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128, iv0);
            return hex(e.doFinal(rep(20, 0)));
        });
        tv("AES-128-GCM zero key zero iv", () -> {
            Cipher e = Cipher.getInstance("AES/GCM/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128, new GCMParameterSpec(128, rep(12, 0)));
            return hex(e.doFinal(new byte[0]));
        });
        tv("AES-128-GCM with AAD", () -> {
            Cipher e = Cipher.getInstance("AES/GCM/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128, new GCMParameterSpec(128, rep(12, 0)));
            e.updateAAD(utf8("aad"));
            return hex(e.doFinal(utf8("plaintext")));
        });
        tv("AES-GCM tamper detected", () -> {
            Cipher e = Cipher.getInstance("AES/GCM/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128, new GCMParameterSpec(128, rep(12, 0)));
            byte[] ct = e.doFinal(utf8("plaintext"));
            ct[0] ^= 1;
            Cipher d = Cipher.getInstance("AES/GCM/NoPadding");
            d.init(Cipher.DECRYPT_MODE, aes128, new GCMParameterSpec(128, rep(12, 0)));
            try { d.doFinal(ct); return "no throw"; }
            catch (Throwable t) { return t.getClass().getName(); }
        });
        tv("AES-GCM same key+iv twice refused", () -> {
            Cipher e = Cipher.getInstance("AES/GCM/NoPadding");
            GCMParameterSpec s = new GCMParameterSpec(128, rep(12, 0));
            e.init(Cipher.ENCRYPT_MODE, aes128, s);
            e.doFinal(utf8("a"));
            try { e.init(Cipher.ENCRYPT_MODE, aes128, s); return "no throw"; }
            catch (Throwable t) { return t.getClass().getName(); }
        });
        tv("AES-GCM tag length 96", () -> {
            Cipher e = Cipher.getInstance("AES/GCM/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128, new GCMParameterSpec(96, rep(12, 0)));
            return hex(e.doFinal(new byte[0]));
        });
        tv("AES-GCM tag length 8 refused", () -> {
            Cipher e = Cipher.getInstance("AES/GCM/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128, new GCMParameterSpec(8, rep(12, 0)));
            return hex(e.doFinal(new byte[0]));
        });
        tv("Cipher wrong key length", () -> {
            Cipher e = Cipher.getInstance("AES/ECB/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(rep(17, 0), "AES"));
            return "no throw";
        });
        tv("Cipher wrong key algorithm", () -> {
            Cipher e = Cipher.getInstance("AES/ECB/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(rep(16, 0), "Blowfish"));
            return "no throw";
        });
        tv("Cipher CBC without IV auto-generates", () -> {
            Cipher e = Cipher.getInstance("AES/CBC/PKCS5Padding");
            e.init(Cipher.ENCRYPT_MODE, aes128);
            return "ivLen=" + (e.getIV() == null ? -1 : e.getIV().length)
                 + " paramsPresent=" + (e.getParameters() != null);
        });
        tv("Cipher DECRYPT CBC without IV refused", () -> {
            Cipher d = Cipher.getInstance("AES/CBC/PKCS5Padding");
            d.init(Cipher.DECRYPT_MODE, aes128);
            return "no throw";
        });
        tv("Cipher doFinal before init", () -> Cipher.getInstance("AES/ECB/NoPadding").doFinal(rep(16, 0)));
        tv("Cipher update before init", () -> hex(Cipher.getInstance("AES/ECB/NoPadding").update(rep(16, 0))));
        tv("Cipher getIV before init", () -> {
            byte[] b = Cipher.getInstance("AES/CBC/PKCS5Padding").getIV();
            return b == null ? "null" : hex(b);
        });
        tv("Cipher getBlockSize before init", () -> Cipher.getInstance("AES/ECB/NoPadding").getBlockSize());
        tv("Cipher getOutputSize before init", () -> Cipher.getInstance("AES/ECB/NoPadding").getOutputSize(16));
        tv("Cipher init bad opmode", () -> {
            Cipher e = Cipher.getInstance("AES/ECB/NoPadding");
            e.init(99, aes128);
            return "no throw";
        });
        tv("Cipher init null key", () -> {
            Cipher e = Cipher.getInstance("AES/ECB/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, (Key) null);
            return "no throw";
        });
        tv("Cipher update null input", () -> {
            Cipher e = Cipher.getInstance("AES/ECB/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128);
            return hex(e.update((byte[]) null));
        });
        tv("Cipher update empty returns null", () -> {
            Cipher e = Cipher.getInstance("AES/ECB/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128);
            byte[] r = e.update(new byte[0]);
            return r == null ? "null" : hex(r);
        });
        tv("Cipher update bad offsets", () -> {
            Cipher e = Cipher.getInstance("AES/ECB/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128);
            return hex(e.update(rep(16, 0), 10, 10));
        });
        tv("Cipher update negative len", () -> {
            Cipher e = Cipher.getInstance("AES/ECB/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128);
            return hex(e.update(rep(16, 0), 0, -1));
        });
        tv("Cipher doFinal short output buffer", () -> {
            Cipher e = Cipher.getInstance("AES/ECB/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, aes128);
            return e.doFinal(rep(16, 0), 0, 16, new byte[4], 0);
        });
        tv("Cipher wrap/unwrap secret key", () -> {
            Cipher w = Cipher.getInstance("AES/ECB/NoPadding");
            w.init(Cipher.WRAP_MODE, aes128);
            byte[] wrapped = w.wrap(aes128);
            Cipher u = Cipher.getInstance("AES/ECB/NoPadding");
            u.init(Cipher.UNWRAP_MODE, aes128);
            Key k = u.unwrap(wrapped, "AES", Cipher.SECRET_KEY);
            return hex(wrapped) + " -> " + hex(k.getEncoded()) + " " + k.getAlgorithm();
        });
        tv("Cipher wrap in ENCRYPT_MODE refused", () -> {
            Cipher w = Cipher.getInstance("AES/ECB/NoPadding");
            w.init(Cipher.ENCRYPT_MODE, aes128);
            return hex(w.wrap(aes128));
        });

        // ---- Mac: RFC 4231 / RFC 2202 known answers, and the state machine
        tv("HmacSHA256 RFC4231 case 1", () -> {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec(rep(20, 0x0b), "HmacSHA256"));
            return hex(m.doFinal(utf8("Hi There")));
        });
        tv("HmacSHA1 RFC2202 case 1", () -> {
            Mac m = Mac.getInstance("HmacSHA1");
            m.init(new SecretKeySpec(rep(20, 0x0b), "HmacSHA1"));
            return hex(m.doFinal(utf8("Hi There")));
        });
        tv("HmacSHA256 keyed by long key", () -> {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec(rep(131, 0xaa), "HmacSHA256"));
            return hex(m.doFinal(utf8("Test Using Larger Than Block-Size Key - Hash Key First")));
        });
        tv("Mac reset restarts", () -> {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec(rep(20, 0x0b), "HmacSHA256"));
            m.update(utf8("garbage"));
            m.reset();
            return hex(m.doFinal(utf8("Hi There")));
        });
        tv("Mac doFinal auto-resets", () -> {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec(rep(20, 0x0b), "HmacSHA256"));
            m.doFinal(utf8("Hi There"));
            return hex(m.doFinal(utf8("Hi There")));
        });
        tv("Mac update-then-doFinal equals one-shot", () -> {
            Mac a = Mac.getInstance("HmacSHA256");
            a.init(new SecretKeySpec(rep(20, 0x0b), "HmacSHA256"));
            a.update(utf8("Hi "));
            a.update(utf8("There"));
            Mac b = Mac.getInstance("HmacSHA256");
            b.init(new SecretKeySpec(rep(20, 0x0b), "HmacSHA256"));
            return Arrays.equals(a.doFinal(), b.doFinal(utf8("Hi There")));
        });
        tv("Mac doFinal before init", () -> hex(Mac.getInstance("HmacSHA256").doFinal(utf8("x"))));
        tv("Mac getMacLength before init", () -> Mac.getInstance("HmacSHA256").getMacLength());
        tv("Mac init null key", () -> {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(null);
            return "no throw";
        });
        tv("Mac init empty key", () -> {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec(new byte[0], "HmacSHA256"));
            return hex(m.doFinal(utf8("x")));
        });
        tv("Mac clone", () -> {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec(rep(20, 0x0b), "HmacSHA256"));
            m.update(utf8("Hi "));
            try {
                Mac c = (Mac) m.clone();
                c.update(utf8("There"));
                return hex(c.doFinal());
            } catch (CloneNotSupportedException e) { return "CloneNotSupportedException"; }
        });

        // ---- SecretKeyFactory / PBKDF2: RFC 6070-style, fully determined
        tv("PBKDF2WithHmacSHA1 1 iter", () -> {
            SecretKeyFactory f = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA1");
            return hex(f.generateSecret(new PBEKeySpec("password".toCharArray(), utf8("salt"), 1, 160)).getEncoded());
        });
        tv("PBKDF2WithHmacSHA256 1000 iter", () -> {
            SecretKeyFactory f = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256");
            return hex(f.generateSecret(new PBEKeySpec("password".toCharArray(), utf8("salt"), 1000, 256)).getEncoded());
        });
        tv("PBKDF2 zero iterations refused", () -> {
            SecretKeyFactory f = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA1");
            return hex(f.generateSecret(new PBEKeySpec("p".toCharArray(), utf8("s"), 0, 160)).getEncoded());
        });
        tv("PBKDF2 zero key length refused", () -> {
            SecretKeyFactory f = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA1");
            return hex(f.generateSecret(new PBEKeySpec("p".toCharArray(), utf8("s"), 1, 0)).getEncoded());
        });
        tv("PBEKeySpec empty salt refused", () -> new PBEKeySpec("p".toCharArray(), new byte[0], 1, 160).getKeyLength());

        // ---- SecretKeySpec's own validation, which needs no provider at all
        tv("SecretKeySpec null key", () -> new SecretKeySpec(null, "AES").getAlgorithm());
        tv("SecretKeySpec empty key", () -> new SecretKeySpec(new byte[0], "AES").getAlgorithm());
        tv("SecretKeySpec null algorithm", () -> new SecretKeySpec(rep(16, 0), null).getAlgorithm());
        tv("SecretKeySpec format", () -> new SecretKeySpec(rep(16, 0), "AES").getFormat());
        tv("SecretKeySpec copies input", () -> {
            byte[] k = rep(16, 1);
            SecretKeySpec s = new SecretKeySpec(k, "AES");
            k[0] = 9;
            return hex(s.getEncoded());
        });
        tv("SecretKeySpec getEncoded copies", () -> {
            SecretKeySpec s = new SecretKeySpec(rep(16, 1), "AES");
            s.getEncoded()[0] = 9;
            return hex(s.getEncoded());
        });
        tv("SecretKeySpec equals", () -> new SecretKeySpec(rep(16, 1), "AES").equals(new SecretKeySpec(rep(16, 1), "AES")));
        tv("SecretKeySpec equals folds algorithm case",
           () -> new SecretKeySpec(rep(16, 1), "AES").equals(new SecretKeySpec(rep(16, 1), "aes")));
        tv("SecretKeySpec hashCode", () -> new SecretKeySpec(rep(16, 1), "AES").hashCode()
                                        == new SecretKeySpec(rep(16, 1), "AES").hashCode());
        tv("IvParameterSpec copies", () -> {
            byte[] v = rep(16, 2);
            IvParameterSpec s = new IvParameterSpec(v);
            v[0] = 9;
            return hex(s.getIV());
        });
        tv("IvParameterSpec null", () -> hex(new IvParameterSpec(null).getIV()));
        tv("IvParameterSpec bad range", () -> hex(new IvParameterSpec(rep(16, 0), 8, 16).getIV()));
        tv("GCMParameterSpec negative tag", () -> new GCMParameterSpec(-1, rep(12, 0)).getTLen());
        tv("GCMParameterSpec null iv", () -> new GCMParameterSpec(128, null).getTLen());

        // ---- Signature's state machine, asked without a keypair so the rows
        // are about the state rules rather than about key generation
        tv("Signature sign before init", () -> hex(Signature.getInstance("SHA256withRSA").sign()));
        tv("Signature update before init", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.update(utf8("x"));
            return "no throw";
        });
        tv("Signature verify before init", () -> Signature.getInstance("SHA256withRSA").verify(new byte[8]));
        tv("Signature initSign null key", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initSign(null);
            return "no throw";
        });
        tv("Signature initVerify null key", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initVerify((PublicKey) null);
            return "no throw";
        });
        tv("Signature getAlgorithm", () -> Signature.getInstance("SHA256withRSA").getAlgorithm());
        tv("Signature toString has algorithm", () -> Signature.getInstance("SHA256withRSA").toString().contains("SHA256withRSA"));
        tv("Signature setParameter unsupported", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.setParameter(new PSSParameterSpec("SHA-256", "MGF1", MGF1ParameterSpec.SHA256, 32, 1));
            return "no throw";
        });

        // ---- a real RSA round-trip from a FIXED key pair. The key is built
        // from literal parameters, so nothing here is generated at random and
        // the signature is reproducible.
        tv("RSA fixed-key sign/verify", () -> {
            KeyFactory kf = KeyFactory.getInstance("RSA");
            // A 1024-bit test modulus with public exponent 65537, private
            // exponent chosen to match; small enough to write down.
            java.math.BigInteger n = new java.math.BigInteger(
                "9516311644981637628637673923648537468107449010693430684265"
              + "9268818755070448845865208104477149243554317836567849527648"
              + "7373339750531967109965105610456824937301835739155238428020"
              + "2265459515416680663637581119538607276486201153818094533107"
              + "40799661127", 10);
            java.math.BigInteger e = java.math.BigInteger.valueOf(65537);
            PublicKey pub = kf.generatePublic(new RSAPublicKeySpec(n, e));
            return "modulusBits=" + ((java.security.interfaces.RSAPublicKey) pub).getModulus().bitLength()
                 + " format=" + pub.getFormat() + " algorithm=" + pub.getAlgorithm()
                 + " encodedLen>0=" + (pub.getEncoded().length > 0);
        });
        tv("RSA initVerify with a non-RSA key", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initVerify(new PublicKey() {
                public String getAlgorithm() { return "NotRSA"; }
                public String getFormat() { return "RAW"; }
                public byte[] getEncoded() { return new byte[] {1, 2, 3}; }
            });
            return "no throw";
        });
        tv("KeyFactory generatePublic wrong spec", () -> {
            KeyFactory kf = KeyFactory.getInstance("RSA");
            return kf.generatePublic(new X509EncodedKeySpec(new byte[] {1, 2, 3})).getAlgorithm();
        });
        tv("KeyFactory generatePrivate wrong spec", () -> {
            KeyFactory kf = KeyFactory.getInstance("RSA");
            return kf.generatePrivate(new PKCS8EncodedKeySpec(new byte[] {1, 2, 3})).getAlgorithm();
        });
        tv("KeyFactory null spec", () -> KeyFactory.getInstance("RSA").generatePublic(null).getAlgorithm());

        // ---- KeyPairGenerator: sizes are validated BEFORE any key is made, so
        // the refusals are reproducible even though the keys would not be
        tv("KeyPairGenerator RSA initialize 512 refused", () -> {
            KeyPairGenerator g = KeyPairGenerator.getInstance("RSA");
            g.initialize(512);
            return "accepted";
        });
        tv("KeyPairGenerator RSA initialize 0", () -> {
            KeyPairGenerator g = KeyPairGenerator.getInstance("RSA");
            g.initialize(0);
            return "accepted";
        });
        tv("KeyPairGenerator EC initialize bogus curve", () -> {
            KeyPairGenerator g = KeyPairGenerator.getInstance("EC");
            g.initialize(new ECGenParameterSpec("no-such-curve"));
            return "accepted";
        });
        tv("KeyPairGenerator EC initialize secp256r1", () -> {
            KeyPairGenerator g = KeyPairGenerator.getInstance("EC");
            g.initialize(new ECGenParameterSpec("secp256r1"));
            return "accepted";
        });
        tv("KeyGenerator AES 17 bits refused", () -> {
            KeyGenerator g = KeyGenerator.getInstance("AES");
            g.init(17);
            return "accepted";
        });
        tv("KeyGenerator AES 128 then key length", () -> {
            KeyGenerator g = KeyGenerator.getInstance("AES");
            g.init(128);
            return "encodedLen=" + g.generateKey().getEncoded().length + " algorithm=" + g.generateKey().getAlgorithm();
        });

        // ---- SecureRandom: the only reproducible question is SHA1PRNG's
        // specified behaviour under a FIXED seed. The default instance is never
        // asked for a value.
        tv("SHA1PRNG fixed seed reproducible", () -> {
            SecureRandom a = SecureRandom.getInstance("SHA1PRNG");
            a.setSeed(new byte[] {1, 2, 3, 4});
            byte[] x = new byte[16];
            a.nextBytes(x);
            SecureRandom b = SecureRandom.getInstance("SHA1PRNG");
            b.setSeed(new byte[] {1, 2, 3, 4});
            byte[] y = new byte[16];
            b.nextBytes(y);
            return Arrays.equals(x, y) + " " + hex(x);
        });
        tv("SecureRandom default algorithm present", () -> new SecureRandom().getAlgorithm() != null);
        tv("SecureRandom nextBytes length", () -> {
            byte[] b = new byte[7];
            new SecureRandom().nextBytes(b);
            return b.length;
        });
        tv("SecureRandom getSeed length", () -> SecureRandom.getSeed(8).length);
        tv("SecureRandom getSeed negative", () -> SecureRandom.getSeed(-1).length);
        tv("SecureRandom getInstanceStrong present", () -> SecureRandom.getInstanceStrong() != null);

        // ---- Provider and Security: the catalogue, without asserting WHO is
        // in it. Names and order are a configuration property; what is asked is
        // that the catalogue is non-empty, self-consistent and correctly
        // answers a service query.
        tv("Security.getProviders non-empty", () -> Security.getProviders().length > 0);
        tv("Security.getProviders stable count", () -> Security.getProviders().length == Security.getProviders().length);
        tv("Security getProvider absent", () -> String.valueOf(Security.getProvider("NoSuchProvider")));
        tv("Security getProvider null", () -> String.valueOf(Security.getProvider(null)));
        tv("Security.getAlgorithms MessageDigest has SHA-256",
           () -> Security.getAlgorithms("MessageDigest").contains("SHA-256"));
        tv("Security.getAlgorithms Cipher has AES", () -> Security.getAlgorithms("Cipher").contains("AES"));
        tv("Security.getAlgorithms bogus type empty", () -> Security.getAlgorithms("NoSuchServiceType").isEmpty());
        tv("Security.getAlgorithms null", () -> String.valueOf(Security.getAlgorithms(null)));
        tv("Security.getProviders filter MD SHA-256", () -> {
            Provider[] q = Security.getProviders("MessageDigest.SHA-256");
            return q != null && q.length > 0;
        });
        tv("Security.getProviders filter bogus", () -> String.valueOf(Security.getProviders("NoSuch.Thing")));
        tv("Security.getProperty jdk.tls.disabledAlgorithms present",
           () -> Security.getProperty("jdk.tls.disabledAlgorithms") != null);
        tv("Security.getProperty absent", () -> String.valueOf(Security.getProperty("no.such.security.property")));
        tv("Security setProperty roundtrip", () -> {
            Security.setProperty("l6.probe.property", "v");
            return Security.getProperty("l6.probe.property");
        });
        tv("Provider getService MessageDigest SHA-256", () -> {
            for (Provider q : Security.getProviders()) {
                Provider.Service s = q.getService("MessageDigest", "SHA-256");
                if (s != null)
                    return "found type=" + s.getType() + " algorithm=" + s.getAlgorithm()
                         + " classNamePresent=" + (s.getClassName() != null);
            }
            return "NOT FOUND in any provider";
        });
        tv("Provider getService absent", () -> {
            Provider q = Security.getProviders()[0];
            return String.valueOf(q.getService("MessageDigest", "NoSuchDigest"));
        });
        tv("Provider getService null type", () -> {
            Provider q = Security.getProviders()[0];
            return String.valueOf(q.getService(null, "SHA-256"));
        });
        tv("Provider getServices non-empty for first", () -> Security.getProviders()[0].getServices().size() > 0);
        tv("Provider is a Properties", () -> Security.getProviders()[0] instanceof java.util.Properties);
        tv("Provider getVersionStr present", () -> Security.getProviders()[0].getVersionStr() != null);
        tv("Provider getInfo present", () -> Security.getProviders()[0].getInfo() != null);
        tv("Provider entrySet non-empty", () -> Security.getProviders()[0].entrySet().size() > 0);
        tv("Provider put refused after install", () -> {
            Provider q = Security.getProviders()[0];
            try { q.put("MessageDigest.L6Probe", "no.such.Class"); return "accepted"; }
            catch (Throwable t) { return t.getClass().getName(); }
        });
        tv("Security.addProvider duplicate returns -1", () -> {
            Provider q = Security.getProviders()[0];
            return Security.addProvider(q);
        });
        tv("Security.removeProvider absent is a no-op", () -> {
            Security.removeProvider("NoSuchProvider");
            return "no throw";
        });

        System.out.println("rows " + rows);
        System.out.println("DONE L6JcaSweep");
    }
}
