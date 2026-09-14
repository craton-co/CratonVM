import java.security.InvalidAlgorithmParameterException;
import java.security.InvalidKeyException;
import java.util.Arrays;
import java.util.HexFormat;
import javax.crypto.AEADBadTagException;
import javax.crypto.Cipher;
import javax.crypto.IllegalBlockSizeException;
import javax.crypto.spec.ChaCha20ParameterSpec;
import javax.crypto.spec.IvParameterSpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * {@code javax.crypto.Cipher} must actually run ChaCha20 — and the AEAD form
 * must actually authenticate.
 *
 * WHY THIS EXISTS. {@code Cipher.getInstance("ChaCha20")} succeeded and then
 * encrypted with **AES-256-ECB**. The engine discarded the cipher name
 * ({@code let (_cipher_name, mode_str, pad) = parse_transformation(&algo)}),
 * defaulted a mode-less transformation to ECB, and a 32-byte ChaCha20 key is a
 * VALID AES-256 key — so the key schedule was built without error and the ECB
 * arm ran. Three consequences, none of which raised anything:
 *
 * <ul>
 *   <li>the nonce was discarded entirely, so output was deterministic per
 *       (key, plaintext block) — the property ChaCha20's nonce exists to
 *       destroy;</li>
 *   <li>the block counter was discarded, so seeking into a stream was
 *       impossible and two different counters produced identical output;</li>
 *   <li>for {@code ChaCha20-Poly1305} there was no AEAD tag AT ALL, so
 *       decrypting attacker-modified ciphertext returned "plaintext" with no
 *       authentication failure.</li>
 * </ul>
 *
 * A round-trip test cannot catch any of that: encrypting and decrypting with
 * the same wrong algorithm round-trips perfectly. (The tree had exactly such a
 * test — {@code t2_6_5_chacha20_poly1305_round_trip} — and it stayed green
 * throughout, because it drove the Rust crate directly and never touched
 * {@code Cipher}.) So every assertion below is against a value the algorithm
 * fixes: RFC 8439's published vectors, and a tamper that must be REFUSED.
 *
 * The vectors are RFC 8439's own (§2.4.2 uses nonce {@code 00..004a0000 0000};
 * §2.8.2 the AEAD example), independently confirmed byte-for-byte against
 * SunJCE on OpenJDK 25.0.4.
 *
 * Determinism: every case pins a nonce, so nothing depends on the CSPRNG. The
 * one generated-nonce case asserts only that two encryptions DIFFER, which is
 * the property, and never a value.
 */
public class RChaCha20Cipher {
    static final HexFormat HEX = HexFormat.of();
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RChaCha20Cipher: " + m);
        }
    }

    static byte[] key32() {
        byte[] k = new byte[32];
        for (int i = 0; i < 32; i++) {
            k[i] = (byte) i;
        }
        return k;
    }

    static SecretKeySpec sk(byte[] k) {
        return new SecretKeySpec(k, "ChaCha20");
    }

    /** RFC 8439 §2.4.2 — the published ChaCha20 vector, counter 1. */
    static void rfc8439EncryptionVector() throws Exception {
        byte[] nonce = HEX.parseHex("000000000000004a00000000");
        byte[] pt = ("Ladies and Gentlemen of the class of '99: If I could offer you only one tip "
                + "for the future, sunscreen would be it.").getBytes("US-ASCII");
        Cipher c = Cipher.getInstance("ChaCha20");
        c.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(nonce, 1));
        String ct = HEX.formatHex(c.doFinal(pt));
        check(ct.equals("6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b"
                + "f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8"
                + "07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736"
                + "5af90bbf74a35be6b40b8eedf2785e42874d"), "RFC 8439 2.4.2 ciphertext: " + ct);
        System.out.println("CK RChaCha20Cipher rfc2_4_2=" + ct.substring(0, 16));
    }

    /**
     * The nonce and the counter are BOTH inputs. An engine that discards either
     * — as the AES-ECB substitution discarded both — passes a round trip and
     * fails here.
     */
    static void nonceAndCounterChangeTheKeystream() throws Exception {
        byte[] pt = "the same plaintext, three times.".getBytes("US-ASCII");
        byte[] n1 = HEX.parseHex("000000090000004a00000000");
        byte[] n2 = HEX.parseHex("000000090000004a00000001");

        Cipher a = Cipher.getInstance("ChaCha20");
        a.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(n1, 1));
        byte[] ctA = a.doFinal(pt);

        Cipher b = Cipher.getInstance("ChaCha20");
        b.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(n2, 1));
        byte[] ctB = b.doFinal(pt);
        check(!Arrays.equals(ctA, ctB), "a different NONCE must change the ciphertext");

        Cipher d = Cipher.getInstance("ChaCha20");
        d.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(n1, 2));
        byte[] ctC = d.doFinal(pt);
        check(!Arrays.equals(ctA, ctC), "a different COUNTER must change the ciphertext");

        // …and the ciphertext is not a block cipher's: ChaCha20 is a stream
        // cipher, so the output length equals the input length exactly. The
        // ECB substitution padded to a 16-byte multiple.
        check(ctA.length == pt.length,
                "ChaCha20 output length must equal input length, got " + ctA.length
                        + " for " + pt.length);
        System.out.println("CK RChaCha20Cipher streamLength=" + ctA.length);

        // A repeated block must NOT produce a repeated ciphertext block —
        // the defining giveaway of ECB.
        byte[] repeated = new byte[64];
        Cipher e = Cipher.getInstance("ChaCha20");
        e.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(n1, 5));
        byte[] ct = e.doFinal(repeated);
        check(!Arrays.equals(Arrays.copyOfRange(ct, 0, 16), Arrays.copyOfRange(ct, 16, 32)),
                "identical plaintext blocks must not give identical ciphertext blocks (ECB tell)");
        System.out.println("CK RChaCha20Cipher notEcb=ok");
    }

    /** RFC 8439 §2.8.2 — the AEAD vector, ciphertext AND tag. */
    static void rfc8439AeadVector() throws Exception {
        byte[] key = HEX.parseHex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        byte[] nonce = HEX.parseHex("070000004041424344454647");
        byte[] aad = HEX.parseHex("50515253c0c1c2c3c4c5c6c7");
        byte[] pt = ("Ladies and Gentlemen of the class of '99: If I could offer you only one tip "
                + "for the future, sunscreen would be it.").getBytes("US-ASCII");

        Cipher c = Cipher.getInstance("ChaCha20-Poly1305");
        c.init(Cipher.ENCRYPT_MODE, sk(key), new IvParameterSpec(nonce));
        c.updateAAD(aad);
        byte[] out = c.doFinal(pt);
        String hex = HEX.formatHex(out);
        check(hex.endsWith("1ae10b594f09e26a7e902ecbd0600691"),
                "RFC 8439 2.8.2 tag: " + hex.substring(hex.length() - 32));
        check(out.length == pt.length + 16, "AEAD output is ciphertext||tag, got " + out.length);
        check(hex.startsWith("d31a8d34648e60db7b86afbc53ef7ec2"),
                "RFC 8439 2.8.2 ciphertext: " + hex.substring(0, 32));
        System.out.println("CK RChaCha20Cipher aeadTag=" + hex.substring(hex.length() - 32));

        // Round trip, with the AAD.
        Cipher d = Cipher.getInstance("ChaCha20-Poly1305");
        d.init(Cipher.DECRYPT_MODE, sk(key), new IvParameterSpec(nonce));
        d.updateAAD(aad);
        check(Arrays.equals(d.doFinal(out), pt), "AEAD round trip");
        System.out.println("CK RChaCha20Cipher aeadRoundTrip=ok");
    }

    /**
     * THE POINT OF THE AEAD. Every one of these must raise; before the fix
     * every one of them returned bytes.
     */
    static void aeadRefusesTampering() throws Exception {
        byte[] key = HEX.parseHex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        byte[] nonce = HEX.parseHex("070000004041424344454647");
        byte[] aad = HEX.parseHex("50515253c0c1c2c3c4c5c6c7");
        byte[] pt = "authenticate me".getBytes("US-ASCII");

        Cipher e = Cipher.getInstance("ChaCha20-Poly1305");
        e.init(Cipher.ENCRYPT_MODE, sk(key), new IvParameterSpec(nonce));
        e.updateAAD(aad);
        byte[] sealed = e.doFinal(pt);

        // 1. a flipped ciphertext bit
        byte[] badCt = sealed.clone();
        badCt[0] ^= 1;
        check(aeadFails(key, nonce, aad, badCt), "a modified ciphertext must fail authentication");

        // 2. a flipped TAG bit
        byte[] badTag = sealed.clone();
        badTag[badTag.length - 1] ^= 1;
        check(aeadFails(key, nonce, aad, badTag), "a modified tag must fail authentication");

        // 3. the wrong AAD
        check(aeadFails(key, nonce, new byte[] { 9, 9 }, sealed), "a modified AAD must fail");

        // 4. the wrong nonce
        byte[] otherNonce = nonce.clone();
        otherNonce[0] ^= 1;
        check(aeadFails(key, otherNonce, aad, sealed), "a wrong nonce must fail");

        // 5. truncated below the tag length
        check(aeadFails(key, nonce, aad, new byte[4]), "a truncated input must fail");

        // 6. …and the honest one still succeeds, so the checks above are not
        //    passing because everything fails.
        check(!aeadFails(key, nonce, aad, sealed), "the UNMODIFIED sealed message must decrypt");
        System.out.println("CK RChaCha20Cipher aeadRejects=5");
    }

    static boolean aeadFails(byte[] key, byte[] nonce, byte[] aad, byte[] sealed) throws Exception {
        Cipher d = Cipher.getInstance("ChaCha20-Poly1305");
        d.init(Cipher.DECRYPT_MODE, sk(key), new IvParameterSpec(nonce));
        d.updateAAD(aad);
        try {
            d.doFinal(sealed);
            return false;
        } catch (AEADBadTagException ex) {
            return true;
        }
    }

    /**
     * SunJCE refuses a second ENCRYPT {@code init} with the SAME key and nonce
     * on the SAME Cipher object — ChaCha20 is a stream cipher, so that reuse
     * emits one keystream twice and leaks the XOR of the two plaintexts.
     *
     * The scope is the instance: a fresh Cipher may use the same pair, which is
     * why every other test here can pin a nonce.
     */
    static void nonceReuseIsRefusedPerInstance() throws Exception {
        byte[] nonce = HEX.parseHex("0000000900000abc00000000");
        Cipher c = Cipher.getInstance("ChaCha20");
        c.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(nonce, 1));
        c.doFinal("first".getBytes("US-ASCII"));
        boolean refused = false;
        try {
            c.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(nonce, 1));
        } catch (InvalidKeyException ex) {
            refused = true;
        }
        check(refused, "re-initialising ENCRYPT with the same key and nonce must be refused");

        // A DIFFERENT nonce on the same instance is fine…
        byte[] other = HEX.parseHex("0000000900000abc00000001");
        c.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(other, 1));
        check(c.doFinal("second".getBytes("US-ASCII")).length == 6, "a new nonce re-inits fine");

        // …and so is the SAME pair on a FRESH instance, which is what makes the
        // guard per-object rather than a process-wide ban.
        Cipher fresh = Cipher.getInstance("ChaCha20");
        fresh.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(nonce, 1));
        check(fresh.doFinal("third".getBytes("US-ASCII")).length == 5,
                "a fresh Cipher may use a pair another instance used");

        // DECRYPT is exempt: re-decrypting the same message is ordinary.
        Cipher d = Cipher.getInstance("ChaCha20");
        d.init(Cipher.DECRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(nonce, 1));
        d.init(Cipher.DECRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(nonce, 1));
        System.out.println("CK RChaCha20Cipher nonceReuse=ok");
    }

    /** The spec-type rules, which are not interchangeable between the two. */
    static void specTypeRules() throws Exception {
        byte[] nonce = HEX.parseHex("000000090000004a00000000");
        boolean refusedIv = false;
        try {
            Cipher c = Cipher.getInstance("ChaCha20");
            c.init(Cipher.ENCRYPT_MODE, sk(key32()), new IvParameterSpec(nonce));
        } catch (InvalidAlgorithmParameterException ex) {
            refusedIv = true;
        }
        check(refusedIv, "ChaCha20 must require a ChaCha20ParameterSpec");

        boolean refusedCc20 = false;
        try {
            Cipher c = Cipher.getInstance("ChaCha20-Poly1305");
            c.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(nonce, 1));
        } catch (InvalidAlgorithmParameterException ex) {
            refusedCc20 = true;
        }
        check(refusedCc20, "ChaCha20-Poly1305 must require an IvParameterSpec");

        // A 256-bit key is the only legal one.
        boolean shortKey = false;
        try {
            Cipher c = Cipher.getInstance("ChaCha20");
            c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(new byte[16], "ChaCha20"),
                    new ChaCha20ParameterSpec(nonce, 1));
            c.doFinal(new byte[8]);
        } catch (InvalidKeyException ex) {
            shortKey = true;
        }
        check(shortKey, "a 128-bit key must be refused — it is a valid AES key, which is the bug");

        // A block-cipher mode on ChaCha20 is not a transformation SunJCE has.
        boolean noEcb = false;
        try {
            Cipher.getInstance("ChaCha20/ECB/NoPadding");
        } catch (java.security.NoSuchAlgorithmException ex) {
            noEcb = true;
        }
        check(noEcb, "ChaCha20/ECB/NoPadding must not resolve");

        // …while the qualified spelling SunJCE does have works.
        Cipher q = Cipher.getInstance("ChaCha20/None/NoPadding");
        q.init(Cipher.ENCRYPT_MODE, sk(key32()), new ChaCha20ParameterSpec(nonce, 1));
        check(q.doFinal(new byte[10]).length == 10, "ChaCha20/None/NoPadding works");
        System.out.println("CK RChaCha20Cipher specRules=ok");
    }

    /** The AES key wraps, which reached doFinal only as an unchecked throw. */
    static void aesKeyWraps() throws Exception {
        byte[] kek = HEX.parseHex("000102030405060708090a0b0c0d0e0f");
        byte[] d16 = HEX.parseHex("00112233445566778899aabbccddeeff");
        byte[] d20 = HEX.parseHex("00112233445566778899aabbccddeeff00112233");

        check(wrap("AES/KW/NoPadding", kek, d16)
                .equals("1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5"), "AES/KW/NoPadding");
        check(wrap("AES/KW/PKCS5Padding", kek, d16)
                .equals("b05471fa00ab70570ea62b3cfc244f1001af95366e5fe1f430ed8ac55b16c5da"),
                "AES/KW/PKCS5Padding 16");
        check(wrap("AES/KW/PKCS5Padding", kek, d20)
                .equals("a694a9bc72fdcf00782dd32f2ed1e75859b7b87730d71efbba4e6a5e5f9bb8bd"),
                "AES/KW/PKCS5Padding 20");
        check(wrap("AES/KWP/NoPadding", kek, d16)
                .equals("2cef0c9e30de26016c230cb78bc60d51b1fe083ba0c79cd5"), "AES/KWP 16");
        check(wrap("AES/KWP/NoPadding", kek, d20)
                .equals("23cf017f0dc30969899318b8b400c0eca73290dba36289217fbb33d964653ae9"),
                "AES/KWP 20");

        // Round trips, at the ORIGINAL length — RFC 5649's length field.
        for (String t : new String[] { "AES/KW/PKCS5Padding", "AES/KWP/NoPadding" }) {
            for (int len : new int[] { 8, 9, 15, 16, 17, 24 }) {
                byte[] data = new byte[len];
                for (int i = 0; i < len; i++) {
                    data[i] = (byte) (i * 13 + 1);
                }
                Cipher e = Cipher.getInstance(t);
                e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(kek, "AES"));
                byte[] w = e.doFinal(data);
                Cipher d = Cipher.getInstance(t);
                d.init(Cipher.DECRYPT_MODE, new SecretKeySpec(kek, "AES"));
                check(Arrays.equals(d.doFinal(w), data), t + " round trip at " + len);
            }
        }

        // A length RFC 3394 cannot wrap is a CHECKED exception, not an
        // unchecked IllegalStateException that no caller can catch.
        boolean checkedRefusal = false;
        try {
            Cipher e = Cipher.getInstance("AES/KW/NoPadding");
            e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(kek, "AES"));
            e.doFinal(d20);
        } catch (IllegalBlockSizeException ex) {
            checkedRefusal = true;
        }
        check(checkedRefusal, "a 20-byte AES/KW payload must raise IllegalBlockSizeException");
        System.out.println("CK RChaCha20Cipher keyWraps=ok");
    }

    static String wrap(String transform, byte[] kek, byte[] data) throws Exception {
        Cipher c = Cipher.getInstance(transform);
        c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(kek, "AES"));
        return HEX.formatHex(c.doFinal(data));
    }

    public static void main(String[] args) throws Exception {
        rfc8439EncryptionVector();
        nonceAndCounterChangeTheKeystream();
        rfc8439AeadVector();
        aeadRefusesTampering();
        nonceReuseIsRefusedPerInstance();
        specTypeRules();
        aesKeyWraps();
        System.out.println("CK RChaCha20Cipher checks=" + checks);
        System.out.println("PASS RChaCha20Cipher (" + checks + " checks)");
    }
}
