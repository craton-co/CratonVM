import java.security.NoSuchAlgorithmException;
import java.util.Arrays;
import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.Mac;
import javax.crypto.SecretKey;
import javax.crypto.spec.IvParameterSpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * The three crypto defects, measured rather than asserted.
 *
 * Every one of these was a FABRICATED SUCCESS: the call returned a plausible
 * value and nothing errored, so any probe printing "ok" would have passed. So
 * this prints BYTES. A reader diffing two transcripts sees the defect; a reader
 * of a verdict would not have.
 *
 *   1. KeyGenerator.getInstance("AES").generateKey() returned 32 ZERO bytes --
 *      SecretKeySpec aliased the caller's array where the JDK clones, and the
 *      generator scrubbed its buffer immediately after constructing the key, so
 *      the scrub landed on the key itself.
 *   2. Cipher.getInstance("ChaCha20") and "ChaCha20-Poly1305" produced
 *      AES-256-ECB. The name was discarded, a missing mode defaulted to ECB,
 *      and a 32-byte ChaCha key is a valid AES-256 key so nothing errored. The
 *      nonce was dropped and there was no AEAD tag, so TAMPERED CIPHERTEXT
 *      DECRYPTED CLEANLY.
 *   3. Blowfish and RC4 both produced AES-128-ECB -- byte-identical ciphertext
 *      to each other, which is the tell.
 *
 * Fixed-key, fixed-IV, fixed-plaintext throughout: the output must be
 * reproducible so two VMs can be diffed. Nothing here is a security control;
 * these are known-answer vectors.
 */
public final class CryptoTrioProbe {

    public static void main(String[] args) {
        keyGenerationIsRandom();
        chaCha20IsNotSecretlyAes();
        distinctAlgorithmsAreDistinct();
        macLengthMatchesTheAlgorithm();
        System.out.println("PROBE-DONE");
    }

    // ---- 1. an all-zero key is the failure that reads as success -----------

    private static void keyGenerationIsRandom() {
        for (String algo : new String[] {"AES", "HmacSHA256", "DESede", "Blowfish"}) {
            try {
                KeyGenerator kg = KeyGenerator.getInstance(algo);
                byte[] k1 = kg.generateKey().getEncoded();
                byte[] k2 = kg.generateKey().getEncoded();
                // Report the three things that distinguish a real key from a
                // scrubbed buffer: length, all-zero, and two draws differing.
                line("keygen." + algo,
                        "len=" + k1.length
                                + " allZero=" + allZero(k1)
                                + " twoDrawsDiffer=" + (!Arrays.equals(k1, k2)));
            } catch (Throwable t) {
                line("keygen." + algo, thrown(t));
            }
        }

        // The aliasing itself, isolated: SecretKeySpec must COPY, so scrubbing
        // the caller's array afterwards must not reach the key.
        try {
            byte[] raw = new byte[16];
            Arrays.fill(raw, (byte) 0x41);
            SecretKeySpec spec = new SecretKeySpec(raw, "AES");
            Arrays.fill(raw, (byte) 0); // the scrub that used to land on the key
            line("SecretKeySpec.copiesOnConstruct", hex(spec.getEncoded()));
        } catch (Throwable t) {
            line("SecretKeySpec.copiesOnConstruct", thrown(t));
        }

        // ...and on the way out, so a caller cannot scrub the live key either.
        try {
            byte[] raw = new byte[16];
            Arrays.fill(raw, (byte) 0x42);
            SecretKeySpec spec = new SecretKeySpec(raw, "AES");
            byte[] out = spec.getEncoded();
            Arrays.fill(out, (byte) 0);
            line("SecretKeySpec.copiesOnGetEncoded", hex(spec.getEncoded()));
        } catch (Throwable t) {
            line("SecretKeySpec.copiesOnGetEncoded", thrown(t));
        }
    }

    // ---- 2. ChaCha20 must be ChaCha20, or must refuse ----------------------

    private static void chaCha20IsNotSecretlyAes() {
        byte[] key32 = new byte[32];
        for (int i = 0; i < 32; i++) {
            key32[i] = (byte) i;
        }
        byte[] nonce12 = new byte[12];
        for (int i = 0; i < 12; i++) {
            nonce12[i] = (byte) (0x10 + i);
        }
        byte[] pt = "the quick brown fox jumps over t".getBytes(); // 32 bytes

        // Whatever ChaCha20 answers, it must NOT equal AES-256-ECB of the same
        // key and plaintext. That equality was the whole defect, and it is the
        // one assertion that cannot be satisfied by accident.
        String aesEcb = encryptOrThrow("AES/ECB/NoPadding", "AES", key32, null, pt);
        line("aes256ecb.ct", aesEcb);

        for (String name : new String[] {"ChaCha20", "ChaCha20-Poly1305"}) {
            String ct = encryptOrThrow(name, "ChaCha20", key32, nonce12, pt);
            line(name + ".ct", ct);
            line(name + ".equalsAesEcb", String.valueOf(ct.equals(aesEcb)));
        }

        // The AEAD contract: flipping one ciphertext byte must fail the tag.
        // A cipher that cannot fail here is worse than no cipher, because
        // callers build integrity guarantees on it.
        try {
            Cipher c = Cipher.getInstance("ChaCha20-Poly1305");
            c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key32, "ChaCha20"),
                    new IvParameterSpec(nonce12));
            byte[] ct = c.doFinal(pt);
            ct[0] ^= 0x01;
            Cipher d = Cipher.getInstance("ChaCha20-Poly1305");
            d.init(Cipher.DECRYPT_MODE, new SecretKeySpec(key32, "ChaCha20"),
                    new IvParameterSpec(nonce12));
            byte[] out = d.doFinal(ct);
            line("ChaCha20-Poly1305.tamperedDecrypts", "NO-THROW len=" + out.length);
        } catch (Throwable t) {
            line("ChaCha20-Poly1305.tamperedDecrypts", thrown(t));
        }
    }

    // ---- 3. two different ciphers must not be one cipher -------------------

    private static void distinctAlgorithmsAreDistinct() {
        byte[] key16 = new byte[16];
        for (int i = 0; i < 16; i++) {
            key16[i] = (byte) (0x30 + i);
        }
        byte[] pt = "sixteen byte msg".getBytes(); // 16 bytes

        String aes = encryptOrThrow("AES/ECB/NoPadding", "AES", key16, null, pt);
        String blowfish = encryptOrThrow("Blowfish", "Blowfish", key16, null, pt);
        String rc4 = encryptOrThrow("RC4", "RC4", key16, null, pt);

        line("aes128ecb.ct", aes);
        line("Blowfish.ct", blowfish);
        line("RC4.ct", rc4);
        // The tell: Blowfish and RC4 produced the SAME bytes as each other and
        // as AES. Three different algorithms cannot agree.
        //
        // These comparisons are only meaningful between two ACTUAL ciphertexts.
        // Comparing two refusals compares the string "NoSuchAlgorithmException"
        // with itself and reports `true`, which reads exactly like the defect
        // -- an equality that means "still the same cipher" when it means
        // "neither ran". That is this campaign's dominant failure shape
        // appearing inside the instrument built to detect it, so `sameCipher`
        // answers `n/a` unless both sides produced bytes.
        line("Blowfish.equalsAes", sameCipher(blowfish, aes));
        line("RC4.equalsAes", sameCipher(rc4, aes));
        line("Blowfish.equalsRc4", sameCipher(blowfish, rc4));
    }

    // ---- the same species one class over -----------------------------------

    private static void macLengthMatchesTheAlgorithm() {
        byte[] key = new byte[32];
        Arrays.fill(key, (byte) 0x0b);
        byte[] msg = "hi".getBytes();
        // HmacSHA224 is 28 bytes. It used to be served by HMAC-SHA-256 and
        // report 32 -- a wrong MAC that verifies against nothing.
        for (String algo : new String[] {"HmacSHA256", "HmacSHA224", "HmacSHA512"}) {
            try {
                Mac m = Mac.getInstance(algo);
                m.init(new SecretKeySpec(key, algo));
                byte[] tag = m.doFinal(msg);
                line("mac." + algo, "len=" + tag.length + " " + hex(tag));
            } catch (Throwable t) {
                line("mac." + algo, thrown(t));
            }
        }
    }

    // ---- helpers -----------------------------------------------------------

    private static String encryptOrThrow(
            String transformation, String keyAlgo, byte[] key, byte[] iv, byte[] pt) {
        try {
            Cipher c = Cipher.getInstance(transformation);
            SecretKey k = new SecretKeySpec(key, keyAlgo);
            if (iv == null) {
                c.init(Cipher.ENCRYPT_MODE, k);
            } else {
                c.init(Cipher.ENCRYPT_MODE, k, new IvParameterSpec(iv));
            }
            return hex(c.doFinal(pt));
        } catch (NoSuchAlgorithmException e) {
            // A refusal is a legitimate, correct answer here: better a missing
            // cipher than a wrong one. Report it as itself, not as a failure.
            return "NoSuchAlgorithmException";
        } catch (Throwable t) {
            return thrown(t);
        }
    }

    /**
     * `true`/`false` only when both operands are real ciphertext; `n/a` when
     * either is a refusal or a throw.
     *
     * Two refusals are equal as strings, and reporting that as `true` would
     * claim the two algorithms are the same cipher when in fact neither ran.
     */
    private static String sameCipher(String a, String b) {
        if (!isHex(a) || !isHex(b)) {
            return "n/a(" + (isHex(a) ? "ok" : "no-ct") + "," + (isHex(b) ? "ok" : "no-ct") + ")";
        }
        return String.valueOf(a.equals(b));
    }

    private static boolean isHex(String s) {
        if (s == null || s.isEmpty()) {
            return false;
        }
        for (int i = 0; i < s.length(); i++) {
            if (Character.digit(s.charAt(i), 16) < 0) {
                return false;
            }
        }
        return true;
    }

    private static boolean allZero(byte[] b) {
        for (byte x : b) {
            if (x != 0) {
                return false;
            }
        }
        return true;
    }

    private static String hex(byte[] b) {
        if (b == null) {
            return "null";
        }
        StringBuilder sb = new StringBuilder(b.length * 2);
        for (byte x : b) {
            sb.append(Character.forDigit((x >> 4) & 0xf, 16));
            sb.append(Character.forDigit(x & 0xf, 16));
        }
        return sb.toString();
    }

    private static String thrown(Throwable t) {
        String m = t.getMessage();
        return t.getClass().getName() + (m == null ? "" : ": " + m);
    }

    private static void line(String name, String value) {
        System.out.println(name + "=" + value);
    }

    private CryptoTrioProbe() {}
}
