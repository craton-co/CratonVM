import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.security.*;
import java.security.spec.*;
import javax.crypto.*;
import javax.crypto.spec.*;

/**
 * The JCA exception-TYPE census.
 *
 * The question this probe asks is not "does the call fail" — every VM in this
 * campaign fails the calls below. It is "**which class** does it fail with",
 * because the class is the only thing a caller's `catch` selects on. A
 * `javax.crypto.BadPaddingException` is CHECKED; the `IllegalStateException`
 * that CratonVM raised in its place is not, so a caller who wrote the JDK's own
 * `catch (BadPaddingException e)` around an RSA decrypt did not catch it and
 * the failure escaped as an unchecked throw through code that believed it had
 * handled it.
 *
 * Two design rules, both paid for earlier in this campaign:
 *
 *  * **Comparing two refusals reports `true`.** Every row prints the exception
 *    CLASS NAME verbatim, never a boolean "did the two agree". Two engines that
 *    both throw compare one exception name against itself.
 *  * **A round trip cannot catch a wrong algorithm.** Nothing here round-trips
 *    to decide an outcome. Every negative arm corrupts something specific — one
 *    bit of a ciphertext, one bit of a tag, the key, the AAD — so the arm names
 *    which input the engine failed to consult.
 *
 * A third rule specific to AEAD: `AEADBadTagException` EXTENDS
 * `BadPaddingException`, so `catch (BadPaddingException)` catches both and a
 * probe that prints only the caught type cannot tell them apart. Every AEAD row
 * therefore prints `getClass().getName()` of the thrown object, and the
 * `H.` section prints the hierarchy itself so a VM whose
 * `AEADBadTagException` is not a `BadPaddingException` is visible directly.
 *
 * Sections:
 *   H.  the exception hierarchy as this VM reports it (assignability + checked)
 *   C.  javax.crypto.Cipher
 *   S.  java.security.Signature
 *   M.  javax.crypto.Mac
 *   D.  java.security.MessageDigest
 *   K.  java.security.KeyStore / KeyFactory / SecretKeyFactory / KeyAgreement
 *
 * Oracle transcript: JcaExceptionTypeProbe.expected.txt (Temurin 25.0.3+9).
 */
public class JcaExceptionTypeProbe {

    interface Body { void run() throws Exception; }

    /**
     * The fully-qualified name of what {@code body} threw, or {@code NONE} when
     * it returned. `Throwable` and not `Exception`: an `Error` escaping a JCA
     * call is itself a finding (a `NoClassDefFoundError` is what you get when a
     * VM raises an exception class that does not resolve, which converts a
     * wrong-exception defect into a worse one), and catching only `Exception`
     * would let it sail past this instrument exactly as it sails past the
     * caller's.
     */
    static String thrown(Body body) {
        try {
            body.run();
            return "NONE";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static void row(String name, String value) {
        System.out.println("CK " + name + " = " + value);
    }

    static void ex(String name, Body body) {
        row(name, thrown(body));
    }

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x));
        return s.toString();
    }

    static byte[] flip(byte[] b, int i) {
        byte[] c = b.clone();
        c[i] ^= 0x01;
        return c;
    }

    // ---------------------------------------------------------------- H
    /**
     * The hierarchy, asked of the VM rather than assumed.
     *
     * `AEADBadTagException extends BadPaddingException extends
     * GeneralSecurityException extends Exception` — every link is load-bearing.
     * If `GeneralSecurityException` were made to extend `RuntimeException` the
     * whole family would silently become unchecked and every `C.` row below
     * would still print the name the oracle prints. `checked` is therefore
     * printed independently of the class name.
     */
    static void hierarchy() {
        Class<?>[] cs = {
            javax.crypto.BadPaddingException.class,
            javax.crypto.AEADBadTagException.class,
            javax.crypto.IllegalBlockSizeException.class,
            javax.crypto.NoSuchPaddingException.class,
            javax.crypto.ShortBufferException.class,
            java.security.InvalidKeyException.class,
            java.security.InvalidAlgorithmParameterException.class,
            java.security.SignatureException.class,
            java.security.KeyStoreException.class,
            java.security.UnrecoverableKeyException.class,
            java.security.NoSuchAlgorithmException.class,
            java.security.DigestException.class,
            java.security.spec.InvalidKeySpecException.class,
            java.security.GeneralSecurityException.class,
        };
        for (Class<?> c : cs) {
            boolean checked = !RuntimeException.class.isAssignableFrom(c)
                    && !Error.class.isAssignableFrom(c);
            row("H." + c.getSimpleName(),
                    "super=" + c.getSuperclass().getName() + " checked=" + checked);
        }
        row("H.AEADBadTagException.isBadPadding",
                String.valueOf(javax.crypto.BadPaddingException.class
                        .isAssignableFrom(javax.crypto.AEADBadTagException.class)));
    }

    public static void main(String[] args) throws Exception {
        hierarchy();

        // ------------------------------------------------------------ setup
        byte[] aesKeyBytes = new byte[32];
        for (int i = 0; i < 32; i++) aesKeyBytes[i] = (byte) (i + 1);
        SecretKey aes = new SecretKeySpec(aesKeyBytes, "AES");
        byte[] wrongAesBytes = aesKeyBytes.clone();
        wrongAesBytes[0] ^= 0x01;
        SecretKey wrongAes = new SecretKeySpec(wrongAesBytes, "AES");
        byte[] iv16 = new byte[16];
        for (int i = 0; i < 16; i++) iv16[i] = (byte) (i * 3);
        byte[] iv12 = new byte[12];
        for (int i = 0; i < 12; i++) iv12[i] = (byte) (i * 7);
        byte[] pt = "jca exception census payload".getBytes("UTF-8");

        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        KeyPair other = kpg.generateKeyPair();

        // ------------------------------------------------------------ C: Cipher

        // --- RSA. This is the row the regression suite sampled.
        Cipher oaepE = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
        oaepE.init(Cipher.ENCRYPT_MODE, kp.getPublic());
        final byte[] oaepCt = oaepE.doFinal(pt);
        Cipher pkcsE = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        pkcsE.init(Cipher.ENCRYPT_MODE, kp.getPublic());
        final byte[] pkcsCt = pkcsE.doFinal(pt);

        ex("C.rsa.oaep.wrongKey", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
            c.init(Cipher.DECRYPT_MODE, other.getPrivate());
            c.doFinal(oaepCt);
        });
        ex("C.rsa.oaep.corruptCt", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
            c.init(Cipher.DECRYPT_MODE, kp.getPrivate());
            c.doFinal(flip(oaepCt, 200));
        });
        ex("C.rsa.pkcs1.wrongKey", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.DECRYPT_MODE, other.getPrivate());
            c.doFinal(pkcsCt);
        });
        ex("C.rsa.pkcs1.corruptCt", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.DECRYPT_MODE, kp.getPrivate());
            c.doFinal(flip(pkcsCt, 200));
        });
        // A ciphertext that is not the modulus width. Structural, not a padding
        // failure, and SunJCE distinguishes the two.
        ex("C.rsa.pkcs1.shortCt", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.DECRYPT_MODE, kp.getPrivate());
            c.doFinal(new byte[200]);
        });
        ex("C.rsa.pkcs1.longCt", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.DECRYPT_MODE, kp.getPrivate());
            c.doFinal(new byte[300]);
        });
        // Too much plaintext for the padding. RSA-2048 PKCS#1 v1.5 tops out at
        // 245 bytes; OAEP-SHA-256 at 190.
        ex("C.rsa.pkcs1.ptTooLong", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.ENCRYPT_MODE, kp.getPublic());
            c.doFinal(new byte[246]);
        });
        ex("C.rsa.oaep.ptTooLong", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
            c.init(Cipher.ENCRYPT_MODE, kp.getPublic());
            c.doFinal(new byte[191]);
        });
        // NoPadding: raw modexp. A ciphertext numerically >= n is the only
        // structural refusal it has.
        ex("C.rsa.raw.ptTooLong", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, kp.getPublic());
            c.doFinal(new byte[257]);
        });
        // Decrypting under a PUBLIC key, and encrypting under a PRIVATE one,
        // are both legal RSA operations; what is not legal is initialising the
        // cipher with a key of the wrong TYPE for the algorithm.
        ex("C.rsa.init.aesKey", () -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.ENCRYPT_MODE, aes);
        });

        // --- AES block modes: padding vs block size are different failures.
        Cipher ecbE = Cipher.getInstance("AES/ECB/PKCS5Padding");
        ecbE.init(Cipher.ENCRYPT_MODE, aes);
        final byte[] ecbCt = ecbE.doFinal(pt);
        Cipher cbcE = Cipher.getInstance("AES/CBC/PKCS5Padding");
        cbcE.init(Cipher.ENCRYPT_MODE, aes, new IvParameterSpec(iv16));
        final byte[] cbcCt = cbcE.doFinal(pt);

        ex("C.aes.ecb.pkcs5.wrongKey", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/PKCS5Padding");
            c.init(Cipher.DECRYPT_MODE, wrongAes);
            c.doFinal(ecbCt);
        });
        ex("C.aes.cbc.pkcs5.wrongKey", () -> {
            Cipher c = Cipher.getInstance("AES/CBC/PKCS5Padding");
            c.init(Cipher.DECRYPT_MODE, wrongAes, new IvParameterSpec(iv16));
            c.doFinal(cbcCt);
        });
        ex("C.aes.cbc.pkcs5.wrongIv", () -> {
            Cipher c = Cipher.getInstance("AES/CBC/PKCS5Padding");
            c.init(Cipher.DECRYPT_MODE, aes, new IvParameterSpec(flip(iv16, 0)));
            c.doFinal(cbcCt);
        });
        ex("C.aes.ecb.nopad.encRagged", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, aes);
            c.doFinal(new byte[17]);
        });
        ex("C.aes.ecb.nopad.decRagged", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/NoPadding");
            c.init(Cipher.DECRYPT_MODE, aes);
            c.doFinal(new byte[17]);
        });
        ex("C.aes.cbc.pkcs5.decRagged", () -> {
            Cipher c = Cipher.getInstance("AES/CBC/PKCS5Padding");
            c.init(Cipher.DECRYPT_MODE, aes, new IvParameterSpec(iv16));
            c.doFinal(new byte[17]);
        });

        // --- AEAD. The one place the SUBCLASS is the answer.
        Cipher gcmE = Cipher.getInstance("AES/GCM/NoPadding");
        gcmE.init(Cipher.ENCRYPT_MODE, aes, new GCMParameterSpec(128, iv12));
        final byte[] gcmCt = gcmE.doFinal(pt);
        ex("C.aes.gcm.flipCt", () -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, aes, new GCMParameterSpec(128, iv12));
            c.doFinal(flip(gcmCt, 0));
        });
        ex("C.aes.gcm.flipTag", () -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, aes, new GCMParameterSpec(128, iv12));
            c.doFinal(flip(gcmCt, gcmCt.length - 1));
        });
        ex("C.aes.gcm.wrongKey", () -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, wrongAes, new GCMParameterSpec(128, iv12));
            c.doFinal(gcmCt);
        });
        ex("C.aes.gcm.truncated", () -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, aes, new GCMParameterSpec(128, iv12));
            c.doFinal(new byte[8]);
        });
        ex("C.aes.gcm.badTagLen", () -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, aes, new GCMParameterSpec(37, iv12));
        });
        // GCM refuses to encrypt twice under the same key+IV. Unchecked on
        // purpose in the JDK: it is a programming error, not a data failure.
        ex("C.aes.gcm.ivReuse", () -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, aes, new GCMParameterSpec(128, iv12));
            c.doFinal(pt);
            c.doFinal(pt);
        });

        byte[] cc20Key = aesKeyBytes.clone();
        Cipher ccE = Cipher.getInstance("ChaCha20-Poly1305");
        ccE.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(cc20Key, "ChaCha20"),
                new IvParameterSpec(iv12));
        final byte[] ccCt = ccE.doFinal(pt);
        ex("C.chacha20p1305.flipCt", () -> {
            Cipher c = Cipher.getInstance("ChaCha20-Poly1305");
            c.init(Cipher.DECRYPT_MODE, new SecretKeySpec(cc20Key, "ChaCha20"),
                    new IvParameterSpec(iv12));
            c.doFinal(flip(ccCt, 0));
        });
        ex("C.chacha20p1305.truncated", () -> {
            Cipher c = Cipher.getInstance("ChaCha20-Poly1305");
            c.init(Cipher.DECRYPT_MODE, new SecretKeySpec(cc20Key, "ChaCha20"),
                    new IvParameterSpec(iv12));
            c.doFinal(new byte[8]);
        });
        ex("C.chacha20.shortKey", () -> {
            Cipher c = Cipher.getInstance("ChaCha20-Poly1305");
            c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(new byte[16], "ChaCha20"),
                    new IvParameterSpec(iv12));
        });

        // --- AES key wrap: an integrity failure with no tag of its own.
        Cipher kwE = Cipher.getInstance("AES/KW/NoPadding");
        kwE.init(Cipher.ENCRYPT_MODE, aes);
        final byte[] wrapped = kwE.doFinal(new byte[16]);
        ex("C.aes.kw.tampered", () -> {
            Cipher c = Cipher.getInstance("AES/KW/NoPadding");
            c.init(Cipher.DECRYPT_MODE, aes);
            c.doFinal(flip(wrapped, 0));
        });
        ex("C.aes.kw.ragged", () -> {
            Cipher c = Cipher.getInstance("AES/KW/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, aes);
            c.doFinal(new byte[5]);
        });

        // --- init / getInstance surface.
        ex("C.getInstance.badPadding", () ->
                Cipher.getInstance("AES/ECB/NoSuchPadding5"));
        ex("C.getInstance.badMode", () ->
                Cipher.getInstance("AES/NOSUCHMODE/PKCS5Padding"));
        ex("C.getInstance.badAlgorithm", () ->
                Cipher.getInstance("NoSuchCipherAlgorithm"));
        ex("C.init.shortAesKey", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/PKCS5Padding");
            c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(new byte[5], "AES"));
        });
        ex("C.init.nullKey", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/PKCS5Padding");
            c.init(Cipher.ENCRYPT_MODE, (Key) null);
        });
        ex("C.init.cbcNoIvOnDecrypt", () -> {
            Cipher c = Cipher.getInstance("AES/CBC/PKCS5Padding");
            c.init(Cipher.DECRYPT_MODE, aes);
        });
        ex("C.init.cbcShortIv", () -> {
            Cipher c = Cipher.getInstance("AES/CBC/PKCS5Padding");
            c.init(Cipher.ENCRYPT_MODE, aes, new IvParameterSpec(new byte[7]));
        });
        ex("C.doFinal.notInitialized", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/PKCS5Padding");
            c.doFinal(pt);
        });
        ex("C.doFinal.shortOutputBuffer", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/PKCS5Padding");
            c.init(Cipher.ENCRYPT_MODE, aes);
            c.doFinal(pt, 0, pt.length, new byte[1]);
        });
        ex("C.update.shortOutputBuffer", () -> {
            Cipher c = Cipher.getInstance("AES/ECB/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, aes);
            c.update(new byte[32], 0, 32, new byte[1]);
        });
        ex("C.unwrap.garbage", () -> {
            Cipher c = Cipher.getInstance("AES/KW/NoPadding");
            c.init(Cipher.UNWRAP_MODE, aes);
            c.unwrap(flip(wrapped, 0), "AES", Cipher.SECRET_KEY);
        });

        // ------------------------------------------------------------ S: Signature
        Signature signer = Signature.getInstance("SHA256withRSA");
        signer.initSign(kp.getPrivate());
        signer.update(pt);
        final byte[] sigBytes = signer.sign();

        ex("S.sign.notInitialized", () -> Signature.getInstance("SHA256withRSA").sign());
        ex("S.update.notInitialized", () -> Signature.getInstance("SHA256withRSA").update(pt));
        ex("S.verify.notInitialized", () ->
                Signature.getInstance("SHA256withRSA").verify(sigBytes));
        ex("S.sign.afterInitVerify", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initVerify(kp.getPublic());
            s.update(pt);
            s.sign();
        });
        ex("S.verify.afterInitSign", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initSign(kp.getPrivate());
            s.update(pt);
            s.verify(sigBytes);
        });
        ex("S.initSign.nullKey", () ->
                Signature.getInstance("SHA256withRSA").initSign(null));
        ex("S.initVerify.nullKey", () ->
                Signature.getInstance("SHA256withRSA").initVerify((PublicKey) null));
        ex("S.initSign.wrongKeyFamily", () -> {
            KeyPairGenerator ec = KeyPairGenerator.getInstance("EC");
            ec.initialize(256);
            Signature.getInstance("SHA256withRSA").initSign(ec.generateKeyPair().getPrivate());
        });
        // A corrupted signature is DATA, not a programming error. `verify` may
        // answer false or raise the checked SignatureException; which one it
        // does is measured here rather than assumed.
        ex("S.verify.corruptSig", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initVerify(kp.getPublic());
            s.update(pt);
            s.verify(flip(sigBytes, 100));
        });
        ex("S.verify.garbageSig", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initVerify(kp.getPublic());
            s.update(pt);
            s.verify(new byte[]{1, 2, 3});
        });
        row("S.verify.wrongMessage", String.valueOf(verifyQuiet(kp.getPublic(),
                "a different message".getBytes("UTF-8"), sigBytes)));
        row("S.verify.wrongKey", String.valueOf(verifyQuiet(other.getPublic(), pt, sigBytes)));
        ex("S.getInstance.badAlgorithm", () -> Signature.getInstance("NoSuchSigAlgorithm"));
        ex("S.setParameter.unsupported", () -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.setParameter(new PSSParameterSpec("SHA-256", "MGF1",
                    MGF1ParameterSpec.SHA256, 32, 1));
        });

        // ------------------------------------------------------------ M: Mac
        ex("M.doFinal.notInitialized", () -> Mac.getInstance("HmacSHA256").doFinal(pt));
        ex("M.update.notInitialized", () -> Mac.getInstance("HmacSHA256").update(pt));
        ex("M.init.nullKey", () -> Mac.getInstance("HmacSHA256").init(null));
        ex("M.init.emptyKey", () ->
                Mac.getInstance("HmacSHA256").init(new SecretKeySpec(new byte[0], "HmacSHA256")));
        ex("M.getInstance.badAlgorithm", () -> Mac.getInstance("NoSuchMacAlgorithm"));
        ex("M.doFinal.shortOutputBuffer", () -> {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec("key".getBytes("UTF-8"), "HmacSHA256"));
            m.update(pt);
            m.doFinal(new byte[4], 0);
        });

        // ------------------------------------------------------------ D: MessageDigest
        ex("D.digest.shortOutputBuffer", () -> {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            md.update(pt);
            md.digest(new byte[4], 0, 4);
        });
        ex("D.digest.nullOutputBuffer", () -> {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            md.update(pt);
            md.digest(null, 0, 32);
        });
        ex("D.getInstance.badAlgorithm", () -> MessageDigest.getInstance("NoSuchDigest"));

        // ------------------------------------------------------------ K: keys and stores
        ex("K.keyFactory.badSpec", () -> {
            KeyFactory kf = KeyFactory.getInstance("RSA");
            kf.generatePublic(new X509EncodedKeySpec(new byte[]{1, 2, 3, 4}));
        });
        ex("K.keyFactory.wrongSpecType", () -> {
            KeyFactory kf = KeyFactory.getInstance("RSA");
            kf.getKeySpec(kp.getPublic(), DSAPublicKeySpec.class);
        });
        ex("K.secretKeyFactory.badSpec", () -> {
            SecretKeyFactory skf = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256");
            skf.generateSecret(new X509EncodedKeySpec(new byte[]{1, 2, 3, 4}));
        });
        ex("K.keyStore.getKeyUninitialized", () -> {
            KeyStore ks = KeyStore.getInstance("PKCS12");
            ks.getKey("alias", "pw".toCharArray());
        });
        ex("K.keyStore.setKeyUninitialized", () -> {
            KeyStore ks = KeyStore.getInstance("PKCS12");
            ks.setKeyEntry("alias", kp.getPrivate(), "pw".toCharArray(), null);
        });
        ex("K.keyStore.sizeUninitialized", () -> KeyStore.getInstance("PKCS12").size());
        ex("K.keyStore.getInstanceBadType", () -> KeyStore.getInstance("NoSuchStoreType"));

        // A real PKCS12 store, saved and reloaded, so the wrong-password arm
        // below is a genuine unrecoverable-key failure rather than a
        // never-initialised one — those are different exceptions and a probe
        // that conflates them measures neither. A SECRET key entry, not a
        // private one: a PKCS12 private-key entry needs a certificate chain,
        // and building a self-signed certificate would drag `keytool`'s
        // internal `CertAndKeyGen` into a probe whose subject is exceptions.
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, null);
        ks.setEntry("k", new KeyStore.SecretKeyEntry(aes),
                new KeyStore.PasswordProtection("storepw".toCharArray()));
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        ks.store(bos, "storepw".toCharArray());
        final byte[] p12 = bos.toByteArray();
        row("K.keyStore.p12Bytes", String.valueOf(p12.length > 0));
        ex("K.keyStore.loadWrongPassword", () -> {
            KeyStore k2 = KeyStore.getInstance("PKCS12");
            k2.load(new ByteArrayInputStream(p12), "wrongpw".toCharArray());
        });
        ex("K.keyStore.getKeyWrongPassword", () -> {
            KeyStore k2 = KeyStore.getInstance("PKCS12");
            k2.load(new ByteArrayInputStream(p12), "storepw".toCharArray());
            k2.getKey("k", "wrongpw".toCharArray());
        });
        ex("K.keyStore.loadGarbage", () -> {
            KeyStore k2 = KeyStore.getInstance("PKCS12");
            k2.load(new ByteArrayInputStream(new byte[]{1, 2, 3, 4, 5}),
                    "storepw".toCharArray());
        });

        ex("K.keyAgreement.generateUninitialized", () ->
                KeyAgreement.getInstance("DH").generateSecret());
        ex("K.keyAgreement.doPhaseUninitialized", () ->
                KeyAgreement.getInstance("DH").doPhase(kp.getPublic(), true));
        ex("K.keyAgreement.initWrongKey", () ->
                KeyAgreement.getInstance("DH").init(aes));

        System.out.println("PASS JcaExceptionTypeProbe");
    }

    static String verifyQuiet(PublicKey k, byte[] msg, byte[] sig) {
        try {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initVerify(k);
            s.update(msg);
            return String.valueOf(s.verify(sig));
        } catch (Exception e) {
            return e.getClass().getName();
        }
    }

}
