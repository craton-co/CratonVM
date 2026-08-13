import java.security.*;
import java.util.*;
import javax.crypto.*;
import javax.crypto.spec.*;

/**
 * Regression: JCA crypto — digests, HMAC, symmetric (AES-GCM) and asymmetric
 * (RSA OAEP + PKCS1 + sign/verify) round-trips. The RSA paths regressed once
 * (a constant-time-unpad bug broke decryption), so they are exercised here.
 *
 * WHY THE NEGATIVE CONTROLS BELOW EXIST. Four of this vector's original seven
 * checks were ROUND TRIPS whose "expected" value was produced by the same
 * implementation under test, and a round trip cannot catch a wrong algorithm,
 * a missing authentication tag, or a verifier that answers true. A JCA provider
 * installed at position 1 whose engineVerify returns true unconditionally and
 * whose AES-GCM is the identity function with no tag produced BYTE-IDENTICAL
 * output from this file (W7-51-vacuous-sweep-round-2.md §2.5). The principle
 * was already written down in this very repository — RChaCha20Cipher.java:34,
 * "a round-trip test cannot catch any of that" — and implemented correctly in
 * RJdkSecurity.java. RCrypto had no such arm; it does now.
 *
 * Three shapes do the work, and each kills a different fake:
 *
 *   * a KNOWN ANSWER for AES-256-GCM. The key and IV are fixed, so the
 *     ciphertext and tag are a constant of the ALGORITHM, measured on Temurin
 *     25.0.3+9. An identity cipher, a cipher running a different algorithm
 *     under this name, and a cipher that omits the tag all produce a different
 *     string. This is the assertion RCrypto most conspicuously lacked.
 *   * TAMPER arms. AES-GCM is authenticated: a flipped ciphertext byte, a
 *     flipped tag byte, a wrong key and mismatched AAD must each REFUSE. A
 *     cipher with no tag accepts all four. RSA-OAEP and RSA-PKCS1 must
 *     likewise refuse a corrupted ciphertext and the wrong private key.
 *   * a signature verifier must answer FALSE. Against a different message,
 *     against a different key, and against a corrupted signature. The original
 *     asserted only `ver.verify(sigBytes)` == true, which `return true;`
 *     satisfies.
 *
 * The observables are printed on CK lines because run.sh diffs only those, and
 * because a value the diff can see is a second, independent instrument: the
 * known-answer hex fails locally on THIS VM and cross-VM against HotSpot, and
 * the two fail for different reasons.
 *
 * Determinism: the RSA key pairs are freshly generated, so no RSA ciphertext or
 * signature is printed — only the OUTCOMES, which are constants.
 */
public class RCrypto {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }
    static String hex(byte[] b) { StringBuilder s = new StringBuilder(); for (byte x : b) s.append(String.format("%02x", x)); return s.toString(); }
    static byte[] unhex(String h) {
        byte[] r = new byte[h.length() / 2];
        for (int i = 0; i < r.length; i++) {
            r[i] = (byte) Integer.parseInt(h.substring(2 * i, 2 * i + 2), 16);
        }
        return r;
    }

    /** The name of the exception a body raised, or "NONE" when it returned. */
    interface Body { void run() throws Exception; }
    static String refused(Body body) {
        try {
            body.run();
            return "NONE";
        } catch (Exception e) {
            return e.getClass().getSimpleName();
        }
    }

    /**
     * The exception a body raised, PREFIXED with whether it was checked.
     *
     * THE ASSERTION THIS FILE MOST CONSPICUOUSLY LACKED. `refused()` above
     * reports the class NAME, and a name comparison cannot see the property
     * that actually decides whether a caller's handler runs: a
     * `javax.crypto.BadPaddingException` is CHECKED, and the
     * `IllegalStateException` CratonVM raised in its place is not — so
     * `catch (BadPaddingException e)`, the exception `Cipher.doFinal` DECLARES,
     * did not run and the failure escaped through code that believed it had
     * handled it.
     *
     * `Throwable` and not `Exception`: an `Error` is unchecked too, and a VM
     * that raises a name which does not resolve produces `NoClassDefFoundError`
     * — a wrong-exception defect turned into a worse one. Catching only
     * `Exception` would let that sail past this instrument exactly as it sails
     * past the caller's.
     */
    static String kind(Body body) {
        try {
            body.run();
            return "NONE";
        } catch (Throwable t) {
            boolean unchecked = (t instanceof RuntimeException) || (t instanceof Error);
            return (unchecked ? "UNCHECKED:" : "checked:") + t.getClass().getSimpleName();
        }
    }

    /**
     * The AEAD trap, and the reason it needs its own helper.
     *
     * `AEADBadTagException extends BadPaddingException`, so
     * `catch (BadPaddingException)` catches BOTH and a probe that only checks
     * the base class cannot tell them apart. Catching the SUBCLASS first is
     * what makes the distinction observable: a GCM tag failure reported as the
     * bare base class returns the BASE-ONLY token, which is a real finding —
     * this campaign already shipped a cipher that served AES-256-ECB for
     * ChaCha20 with no AEAD tag at all, so tampered ciphertext decrypted
     * cleanly.
     */
    static String aead(Body body) {
        try {
            body.run();
            return "NONE";
        } catch (AEADBadTagException e) {
            return "AEADBadTagException";
        } catch (BadPaddingException e) {
            return "BadPaddingException-BASE-ONLY";
        } catch (Throwable t) {
            boolean unchecked = (t instanceof RuntimeException) || (t instanceof Error);
            return (unchecked ? "UNCHECKED:" : "checked:") + t.getClass().getSimpleName();
        }
    }

    /** A copy of $b with one bit flipped at $i — the smallest possible corruption. */
    static byte[] flip(byte[] b, int i) {
        byte[] c = b.clone();
        c[i] ^= 0x01;
        return c;
    }

    public static void main(String[] a) throws Exception {
        // ---- SHA-256 known-answer (digest of "abc") ----
        byte[] sha = MessageDigest.getInstance("SHA-256").digest("abc".getBytes("UTF-8"));
        check(hex(sha).equals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), "SHA-256 KAT");
        check(MessageDigest.getInstance("SHA-1").digest(new byte[0]).length == 20, "SHA-1 length");
        System.out.println("CK RCrypto sha256=" + hex(sha));

        // ---- HMAC-SHA256 known-answer (key "key", msg "The quick brown fox jumps over the lazy dog") ----
        Mac mac = Mac.getInstance("HmacSHA256");
        mac.init(new SecretKeySpec("key".getBytes("UTF-8"), "HmacSHA256"));
        byte[] h = mac.doFinal("The quick brown fox jumps over the lazy dog".getBytes("UTF-8"));
        check(hex(h).equals("f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"), "HMAC-SHA256 KAT");
        System.out.println("CK RCrypto hmacSha256=" + hex(h));

        // ---- AES-256-GCM: known answer, then four refusals ----
        byte[] keyBytes = new byte[32]; for (int i = 0; i < 32; i++) keyBytes[i] = (byte) (i + 1);
        SecretKey key = new SecretKeySpec(keyBytes, "AES");
        byte[] iv = new byte[12]; for (int i = 0; i < 12; i++) iv[i] = (byte) (i * 7);
        byte[] pt = "AES-GCM regression payload".getBytes("UTF-8");
        Cipher enc = Cipher.getInstance("AES/GCM/NoPadding");
        enc.init(Cipher.ENCRYPT_MODE, key, new GCMParameterSpec(128, iv));
        byte[] ct = enc.doFinal(pt);

        // The known answer. Fixed key + fixed IV make AES-256-GCM
        // deterministic, so this string is a property of the ALGORITHM and not
        // of the implementation that produced it. Measured on Temurin 25.0.3+9.
        // It is the assertion an identity cipher cannot pass, and the one no
        // round trip can replace.
        //
        // Measured is not the same as INDEPENDENT, so it is not the only KAT
        // here: the arm below runs a vector out of the GCM specification, whose
        // expected value was never produced by any implementation in this
        // process. That one is what makes this one trustworthy.
        check(hex(ct).equals(
                        "8d30ea48f41d24d7322cd928e00a19ac63d8bd89e32c239cf4a0"
                        + "3fd413eab9f6e1c2a8eb80deb2f39244"),
                "AES-256-GCM known answer: " + hex(ct));
        // The tag is the last 16 bytes of the 128-bit-tag output, so a cipher
        // that returned the plaintext (or omitted the tag) is caught by length
        // alone, before the content comparison.
        check(ct.length == pt.length + 16, "AES-GCM output carries a 16-byte tag, got " + ct.length);
        System.out.println("CK RCrypto gcmCt=" + hex(ct));

        Cipher dec = Cipher.getInstance("AES/GCM/NoPadding");
        dec.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, iv));
        check(Arrays.equals(dec.doFinal(ct), pt), "AES-GCM round-trip");

        // AES-GCM is AUTHENTICATED. Each of these must be refused, and a cipher
        // with no tag accepts all four.
        String gcmBody = refused(() -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, iv));
            c.doFinal(flip(ct, 0));
        });
        String gcmTag = refused(() -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, iv));
            c.doFinal(flip(ct, ct.length - 1));
        });
        String gcmKey = refused(() -> {
            byte[] other = keyBytes.clone(); other[0] ^= 0x01;
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, new SecretKeySpec(other, "AES"), new GCMParameterSpec(128, iv));
            c.doFinal(ct);
        });
        String gcmIv = refused(() -> {
            byte[] otherIv = iv.clone(); otherIv[0] ^= 0x01;
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, otherIv));
            c.doFinal(ct);
        });
        check(gcmBody.equals("AEADBadTagException"), "a flipped ciphertext byte must not decrypt: " + gcmBody);
        check(gcmTag.equals("AEADBadTagException"), "a flipped tag byte must not decrypt: " + gcmTag);
        check(gcmKey.equals("AEADBadTagException"), "the wrong key must not decrypt: " + gcmKey);
        check(gcmIv.equals("AEADBadTagException"), "the wrong IV must not decrypt: " + gcmIv);
        System.out.println("CK RCrypto gcmRefusals=" + gcmBody + "," + gcmTag + "," + gcmKey + "," + gcmIv);

        // AES-256-GCM out of the GCM specification (McGrew & Viega, test case
        // 16). Key, IV, plaintext, AAD and the expected ciphertext-plus-tag are
        // all PUBLISHED values: nothing in this process produced the expected
        // side, so unlike every other assertion in this file it cannot be
        // satisfied by an implementation agreeing with itself. It also covers
        // AAD, which the fixed-key arm above does not, and a 60-byte plaintext,
        // which crosses the 16-byte block boundary four times.
        byte[] tcKey = unhex("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308");
        byte[] tcIv = unhex("cafebabefacedbaddecaf888");
        byte[] tcPt = unhex("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c"
                + "3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39");
        byte[] tcAad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        String tcWant = "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa8cb0"
                + "8e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f66276fc6ece0f4e1768cddf885"
                + "3bb2d551b";
        Cipher tcE = Cipher.getInstance("AES/GCM/NoPadding");
        tcE.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(tcKey, "AES"), new GCMParameterSpec(128, tcIv));
        tcE.updateAAD(tcAad);
        String tcGot = hex(tcE.doFinal(tcPt));
        check(tcGot.equals(tcWant), "GCM spec test case 16: " + tcGot);
        System.out.println("CK RCrypto gcmSpecTc16=" + tcGot);

        // The AAD is authenticated but not encrypted, so a decrypt that never
        // fed the AAD to the tag computation still recovers the plaintext. That
        // is the failure this arm exists for: it must REFUSE.
        String gcmAad = refused(() -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, new SecretKeySpec(tcKey, "AES"), new GCMParameterSpec(128, tcIv));
            c.updateAAD(flip(tcAad, 0));
            c.doFinal(unhex(tcWant));
        });
        check(gcmAad.equals("AEADBadTagException"), "mismatched AAD must not decrypt: " + gcmAad);
        System.out.println("CK RCrypto gcmAadRefusal=" + gcmAad);

        // ---- RSA-2048: OAEP + PKCS1 encrypt/decrypt + sign/verify ----
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA"); kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        // A SECOND, unrelated key pair. Every "wrong key" arm below uses it, so
        // "this decrypted/verified under a key that never signed or encrypted
        // it" is a stated, failing assertion rather than an unwritten
        // assumption.
        KeyPair other = kpg.generateKeyPair();
        check(!Arrays.equals(kp.getPublic().getEncoded(), other.getPublic().getEncoded()),
                "the two generated key pairs must differ");
        byte[] msg = "rsa regression message".getBytes("UTF-8");

        Cipher rOaepE = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
        rOaepE.init(Cipher.ENCRYPT_MODE, kp.getPublic());
        byte[] oaepCt = rOaepE.doFinal(msg);
        Cipher rOaepD = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
        rOaepD.init(Cipher.DECRYPT_MODE, kp.getPrivate());
        check(Arrays.equals(rOaepD.doFinal(oaepCt), msg), "RSA-OAEP round-trip");
        // An RSA-2048 ciphertext is exactly the modulus width, and is never the
        // plaintext: both are false for an identity cipher.
        check(oaepCt.length == 256, "RSA-2048 ciphertext is 256 bytes, got " + oaepCt.length);
        check(!Arrays.equals(Arrays.copyOf(oaepCt, msg.length), msg), "RSA-OAEP must not echo the plaintext");
        String oaepWrongKey = refused(() -> {
            Cipher c = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
            c.init(Cipher.DECRYPT_MODE, other.getPrivate());
            c.doFinal(oaepCt);
        });
        String oaepCorrupt = refused(() -> {
            Cipher c = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
            c.init(Cipher.DECRYPT_MODE, kp.getPrivate());
            c.doFinal(flip(oaepCt, 200));
        });
        // NOT merely "must fail" — must fail as the CHECKED exception
        // `Cipher.doFinal` declares. Measured on Temurin 25.0.3+9:
        // `javax.crypto.BadPaddingException: Padding error in decryption` for
        // both. CratonVM answered `IllegalStateException` here, which is
        // unchecked, so a caller who wrote the JDK's own
        // `catch (BadPaddingException e)` did not catch it.
        check(oaepWrongKey.equals("BadPaddingException"),
                "OAEP under the wrong private key must raise BadPaddingException: " + oaepWrongKey);
        check(oaepCorrupt.equals("BadPaddingException"),
                "OAEP on a corrupted ciphertext must raise BadPaddingException: " + oaepCorrupt);
        System.out.println("CK RCrypto oaepRefusals=" + oaepWrongKey + "," + oaepCorrupt);

        Cipher rPkcsE = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        rPkcsE.init(Cipher.ENCRYPT_MODE, kp.getPublic());
        byte[] pkcsCt = rPkcsE.doFinal(msg);
        Cipher rPkcsD = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        rPkcsD.init(Cipher.DECRYPT_MODE, kp.getPrivate());
        check(Arrays.equals(rPkcsD.doFinal(pkcsCt), msg), "RSA-PKCS1 round-trip");
        check(pkcsCt.length == 256, "RSA-2048 PKCS1 ciphertext is 256 bytes, got " + pkcsCt.length);
        check(!Arrays.equals(Arrays.copyOf(pkcsCt, msg.length), msg), "RSA-PKCS1 must not echo the plaintext");
        // The constant-time-unpad regression this vector was written for lived
        // exactly here: on the wrong key the unpadder sees random bytes, and
        // the failure it must produce is a REFUSAL, not a plausible-looking
        // shorter plaintext.
        String pkcsWrongKey = refused(() -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.DECRYPT_MODE, other.getPrivate());
            c.doFinal(pkcsCt);
        });
        check(pkcsWrongKey.equals("BadPaddingException"),
                "PKCS1 under the wrong private key must raise BadPaddingException: " + pkcsWrongKey);
        System.out.println("CK RCrypto pkcs1Refusals=" + pkcsWrongKey);

        Signature sig = Signature.getInstance("SHA256withRSA");
        sig.initSign(kp.getPrivate()); sig.update(msg);
        byte[] sigBytes = sig.sign();
        Signature ver = Signature.getInstance("SHA256withRSA");
        ver.initVerify(kp.getPublic()); ver.update(msg);
        check(ver.verify(sigBytes), "RSA sign/verify");
        check(sigBytes.length == 256, "an RSA-2048 signature is 256 bytes, got " + sigBytes.length);

        // The three ways a verifier that answers `true` unconditionally is
        // caught. Each MUST be false; an exception is also acceptable for the
        // corrupted case, so it is measured rather than assumed.
        Signature vMsg = Signature.getInstance("SHA256withRSA");
        vMsg.initVerify(kp.getPublic()); vMsg.update("rsa regression messagf".getBytes("UTF-8"));
        boolean wrongMsg = vMsg.verify(sigBytes);
        Signature vKey = Signature.getInstance("SHA256withRSA");
        vKey.initVerify(other.getPublic()); vKey.update(msg);
        boolean wrongKey = vKey.verify(sigBytes);
        // A corrupted signature may legitimately be REFUSED with an exception
        // rather than answered false, so the outcome is captured as a token
        // instead of a boolean. Deliberately NOT written as a body that throws
        // AssertionError on success and is run through refused(): AssertionError
        // is an Error, not an Exception, so it would escape the helper — and the
        // `check` that named this arm would then be unreachable code that reads
        // like coverage. That is the defect this whole file is being repaired
        // for, one level down.
        Signature vSig = Signature.getInstance("SHA256withRSA");
        vSig.initVerify(kp.getPublic()); vSig.update(msg);
        String corrupt;
        try {
            corrupt = vSig.verify(flip(sigBytes, 100)) ? "VERIFIED" : "false";
        } catch (Exception e) {
            corrupt = e.getClass().getSimpleName();
        }
        check(!wrongMsg, "a signature must NOT verify against a different message");
        check(!wrongKey, "a signature must NOT verify against a different public key");
        check(!corrupt.equals("VERIFIED"), "a corrupted signature must NOT verify");
        System.out.println("CK RCrypto verifyNegatives=" + wrongMsg + "," + wrongKey + "," + corrupt);

        // ---- THE EXCEPTION-TYPE CENSUS ----
        //
        // Every arm above asked "did it refuse". These ask "with WHICH class,
        // and was it CHECKED", which is the only property a caller's `catch`
        // selects on. Each expected value is measured on Temurin 25.0.3+9;
        // none is guessed. The full census lives in
        // probes/JcaExceptionTypeProbe.java — what is here is the subset the
        // scheduled suite can afford to run on every build.

        // The hierarchy itself, asked of the VM rather than assumed. If
        // GeneralSecurityException were made to extend RuntimeException the
        // whole family would silently become unchecked and every class-NAME
        // comparison in this file would still pass.
        check(!RuntimeException.class.isAssignableFrom(GeneralSecurityException.class),
                "GeneralSecurityException must be CHECKED");
        check(BadPaddingException.class.isAssignableFrom(AEADBadTagException.class),
                "AEADBadTagException must extend BadPaddingException");
        check(GeneralSecurityException.class.isAssignableFrom(BadPaddingException.class)
                        && GeneralSecurityException.class.isAssignableFrom(IllegalBlockSizeException.class)
                        && GeneralSecurityException.class.isAssignableFrom(ShortBufferException.class)
                        && GeneralSecurityException.class.isAssignableFrom(SignatureException.class),
                "the JCA refusal family must be GeneralSecurityException");

        // RSA: padding vs LENGTH are different exceptions, and the short/long
        // asymmetry is HotSpot's own. A ciphertext SHORTER than the modulus is
        // a smaller integer that decrypts and fails to unpad; only a LONGER one
        // is a block-size failure. A `ct.length != k` check gets the class
        // wrong on both sides and the side wrong on one.
        String rsaChecked = kind(() -> {
            Cipher c = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
            c.init(Cipher.DECRYPT_MODE, other.getPrivate());
            c.doFinal(oaepCt);
        });
        String rsaShortCt = refused(() -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.DECRYPT_MODE, kp.getPrivate());
            c.doFinal(new byte[200]);
        });
        String rsaLongCt = refused(() -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.DECRYPT_MODE, kp.getPrivate());
            c.doFinal(new byte[300]);
        });
        String rsaPkcsTooLong = refused(() -> {
            Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            c.init(Cipher.ENCRYPT_MODE, kp.getPublic());
            c.doFinal(new byte[246]);
        });
        String rsaOaepTooLong = refused(() -> {
            Cipher c = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
            c.init(Cipher.ENCRYPT_MODE, kp.getPublic());
            c.doFinal(new byte[191]);
        });
        check(rsaChecked.equals("checked:BadPaddingException"),
                "an RSA padding failure must be CHECKED: " + rsaChecked);
        check(rsaShortCt.equals("BadPaddingException"),
                "a short RSA ciphertext is a padding failure: " + rsaShortCt);
        check(rsaLongCt.equals("IllegalBlockSizeException"),
                "an over-long RSA ciphertext is a block-size failure: " + rsaLongCt);
        check(rsaPkcsTooLong.equals("IllegalBlockSizeException"),
                "246 bytes under RSA-2048 PKCS1 is a block-size failure: " + rsaPkcsTooLong);
        check(rsaOaepTooLong.equals("IllegalBlockSizeException"),
                "191 bytes under RSA-2048 OAEP-SHA256 is a block-size failure: " + rsaOaepTooLong);
        System.out.println("CK RCrypto rsaExc=" + rsaChecked + "," + rsaShortCt + ","
                + rsaLongCt + "," + rsaPkcsTooLong + "," + rsaOaepTooLong);

        // AEAD, asked so the SUBCLASS is what answers. `gcmRefusals` above
        // compares class names and would read the same whether the throw was
        // checked or not; these ask the other question, and the BASE-ONLY token
        // is what a VM reporting the bare BadPaddingException returns.
        String gcmAead = aead(() -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, iv));
            c.doFinal(flip(ct, 0));
        });
        String gcmTruncated = aead(() -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, iv));
            c.doFinal(new byte[8]);
        });
        check(gcmAead.equals("AEADBadTagException"),
                "a GCM tag failure must be AEADBadTagException specifically: " + gcmAead);
        check(gcmTruncated.equals("AEADBadTagException"),
                "a ciphertext shorter than the tag must be AEADBadTagException: " + gcmTruncated);
        System.out.println("CK RCrypto gcmAeadClass=" + gcmAead + "," + gcmTruncated);

        // AES block modes: padding vs block size, again as different classes.
        Cipher ecbE = Cipher.getInstance("AES/ECB/PKCS5Padding");
        ecbE.init(Cipher.ENCRYPT_MODE, key);
        byte[] ecbCt = ecbE.doFinal(pt);
        String ecbWrongKey = kind(() -> {
            byte[] otherK = keyBytes.clone();
            otherK[0] ^= 0x01;
            Cipher c = Cipher.getInstance("AES/ECB/PKCS5Padding");
            c.init(Cipher.DECRYPT_MODE, new SecretKeySpec(otherK, "AES"));
            c.doFinal(ecbCt);
        });
        String ecbRagged = kind(() -> {
            Cipher c = Cipher.getInstance("AES/ECB/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, key);
            c.doFinal(new byte[17]);
        });
        check(ecbWrongKey.equals("checked:BadPaddingException"),
                "AES/ECB/PKCS5 under the wrong key must be a CHECKED BadPaddingException: "
                        + ecbWrongKey);
        check(ecbRagged.equals("checked:IllegalBlockSizeException"),
                "a ragged NoPadding input must be a CHECKED IllegalBlockSizeException: "
                        + ecbRagged);
        System.out.println("CK RCrypto aesExc=" + ecbWrongKey + "," + ecbRagged);

        // RFC 3394 key wrap. SunJCE answers an integrity failure with
        // IllegalBlockSizeException("Integrity check failed") — NOT
        // BadPaddingException, which is the sibling and not the parent, so a
        // `catch (IllegalBlockSizeException)` written against the JDK would not
        // have run.
        Cipher kwE = Cipher.getInstance("AES/KW/NoPadding");
        kwE.init(Cipher.ENCRYPT_MODE, key);
        byte[] wrapped = kwE.doFinal(new byte[16]);
        String kwTampered = kind(() -> {
            Cipher c = Cipher.getInstance("AES/KW/NoPadding");
            c.init(Cipher.DECRYPT_MODE, key);
            c.doFinal(flip(wrapped, 0));
        });
        check(kwTampered.equals("checked:IllegalBlockSizeException"),
                "a tampered AES/KW payload must be a CHECKED IllegalBlockSizeException: "
                        + kwTampered);
        System.out.println("CK RCrypto kwExc=" + kwTampered);

        // A too-small output buffer. The census answer for this overload was
        // "raises NOTHING of its own": the bytes were written past the end of
        // the caller's array.
        String shortBuf = kind(() -> {
            Cipher c = Cipher.getInstance("AES/ECB/PKCS5Padding");
            c.init(Cipher.ENCRYPT_MODE, key);
            c.doFinal(pt, 0, pt.length, new byte[1]);
        });
        check(shortBuf.equals("checked:ShortBufferException"),
                "a too-small output buffer must be a CHECKED ShortBufferException: " + shortBuf);
        System.out.println("CK RCrypto shortBufferExc=" + shortBuf);

        // Signature state. All three of these DECLARE SignatureException, so a
        // caller's catch is already written; raising the unchecked
        // IllegalStateException instead is what made it dead. `update` before
        // init is the one that raised NOTHING and silently accumulated, which
        // is worse: `sign()` then produced a signature over bytes the caller
        // never meant to sign.
        String sigUninitSign = kind(() -> Signature.getInstance("SHA256withRSA").sign());
        String sigUninitVerify =
                kind(() -> Signature.getInstance("SHA256withRSA").verify(new byte[3]));
        String sigUninitUpdate =
                kind(() -> Signature.getInstance("SHA256withRSA").update(msg));
        String sigWrongDirection = kind(() -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initVerify(kp.getPublic());
            s.update(msg);
            s.sign();
        });
        String sigPartial = kind(() -> {
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initSign(kp.getPrivate());
            s.update(msg);
            s.sign(new byte[4], 0, 4);
        });
        check(sigUninitSign.equals("checked:SignatureException"),
                "sign() before init must be a CHECKED SignatureException: " + sigUninitSign);
        check(sigUninitVerify.equals("checked:SignatureException"),
                "verify() before init must be a CHECKED SignatureException: " + sigUninitVerify);
        check(sigUninitUpdate.equals("checked:SignatureException"),
                "update() before init must be a CHECKED SignatureException: " + sigUninitUpdate);
        check(sigWrongDirection.equals("checked:SignatureException"),
                "sign() after initVerify must be a CHECKED SignatureException: "
                        + sigWrongDirection);
        check(sigPartial.equals("checked:SignatureException"),
                "a partial signature must be refused, not truncated and reported: " + sigPartial);
        System.out.println("CK RCrypto sigExc=" + sigUninitSign + "," + sigUninitVerify + ","
                + sigUninitUpdate + "," + sigWrongDirection + "," + sigPartial);

        // THE ROW WHERE UNCHECKED IS THE CORRECT ANSWER, and it is asserted for
        // exactly that reason: without it, a VM that made every JCA refusal
        // checked would pass every other row in this section. `Mac.doFinal`
        // DECLARES IllegalStateException and HotSpot raises it. Unchecked is
        // not wrong by itself — it is wrong when the JDK's own signature says
        // otherwise.
        String macUninit = kind(() -> Mac.getInstance("HmacSHA256").doFinal(msg));
        check(macUninit.equals("UNCHECKED:IllegalStateException"),
                "Mac before init is HotSpot's own IllegalStateException: " + macUninit);
        System.out.println("CK RCrypto macExc=" + macUninit);

        // W7-21 Patch A/C. Each fails on the pre-fix behaviour: the 2-arg
        // overload minted a KeyGenerator nothing could drive (NPE on this.spi),
        // and SecretKeySpec accepted a zero-length key.
        //
        // 32 is not a guess: SunJCE's AES default is 256-bit on JDK 25,
        // measured on the oracle (probes/JcaAdvertisedVsServedProbe.expected.txt,
        // "A.KeyGenerator[AES] = OK alg=AES keylen=32 prov=SunJCE"), not the 128
        // the deleted shim hardcoded. The all-zeros arm is the standing guard
        // for the SecretKeySpec aliasing defect: SunJCE's own generators scrub
        // their working buffer the instant the key object exists, so a
        // store-by-reference put the scrub through onto the key itself and
        // handed back 32 zero bytes with the right length and the right
        // algorithm name.
        KeyGenerator kg2 = KeyGenerator.getInstance("AES", "SunJCE");
        byte[] kg2Key = kg2.generateKey().getEncoded();
        check(kg2Key.length == 32,
                "getInstance(\"AES\",\"SunJCE\") default is 256-bit, got " + kg2Key.length * 8);
        check(!Arrays.equals(kg2Key, new byte[32]), "a generated AES key must not be all zeros");
        String emptyKey = refused(() -> new SecretKeySpec(new byte[0], "AES"));
        check(emptyKey.equals("IllegalArgumentException"),
                "an empty SecretKeySpec key must be refused: " + emptyKey);
        System.out.println("CK RCrypto keygen2arg=" + kg2Key.length + "," + emptyKey);

        // The wall the block above walked into, pinned directly. AESKeyGenerator's
        // constructor calls SecurityProviderConstants.getDefAESKeySize, which calls
        // Cipher.getMaxAllowedKeyLength("AES") — so the KeyGenerator checks above
        // depend on this and would not say so if it broke.
        //
        // Integer.MAX_VALUE is the unlimited-policy answer, which is what a stock
        // JDK 9+ install gives (crypto.policy=unlimited ships by default). These
        // methods do NOT check that the algorithm exists — "Bogus" answers
        // unlimited too — only that the transformation is well FORMED: one token,
        // or three separated by "/". "AES/GCM" is two, and is the refusal that
        // exercises the format check rather than the policy answer.
        int maxAes = Cipher.getMaxAllowedKeyLength("AES");
        check(maxAes == Integer.MAX_VALUE,
                "getMaxAllowedKeyLength(\"AES\") must be unlimited, got " + maxAes);
        check(Cipher.getMaxAllowedKeyLength("AES/GCM/NoPadding") == Integer.MAX_VALUE,
                "a three-part transformation is well formed and unlimited");
        check(Cipher.getMaxAllowedKeyLength("Bogus") == Integer.MAX_VALUE,
                "getMaxAllowedKeyLength does not validate the algorithm");
        check(Cipher.getMaxAllowedParameterSpec("AES") == null,
                "getMaxAllowedParameterSpec is null under the unlimited policy");
        String twoPart = refused(() -> Cipher.getMaxAllowedKeyLength("AES/GCM"));
        check(twoPart.equals("NoSuchAlgorithmException"),
                "a two-part transformation must be refused: " + twoPart);
        String noAlgo = refused(() -> Cipher.getMaxAllowedKeyLength("/ECB/PKCS5Padding"));
        check(noAlgo.equals("NoSuchAlgorithmException"),
                "an empty algorithm must be refused: " + noAlgo);
        String nullTr = refused(() -> Cipher.getMaxAllowedKeyLength(null));
        check(nullTr.equals("NullPointerException"),
                "a null transformation is NPE, not NoSuchAlgorithmException: " + nullTr);
        System.out.println("CK RCrypto maxKeyLen=" + maxAes + "," + twoPart + ","
                + noAlgo + "," + nullTr);

        System.out.println("CK RCrypto checks=" + checks);
        System.out.println("PASS RCrypto (" + checks + " checks)");
    }
}
