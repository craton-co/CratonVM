import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.*;
import java.security.cert.*;
import java.security.spec.*;
import java.util.*;
import javax.crypto.*;
import javax.crypto.spec.*;

/**
 * SBR wave 1, lane JCA: the JCA engine/value classes whose `Bridge` natives
 * won over real bytecode in the 2026-09-28 `--jdk-only` census
 * (`MessageDigest`, `Mac`, `Cipher`, `Signature`, `KeyFactory`,
 * `KeyPairGenerator`, `SecretKeyFactory`, `SecretKeySpec`, `KeyPair`,
 * `Security`, `Provider`, `Provider$Service`, `GetInstance`,
 * `CertificateFactory`).
 *
 * <p>Every answer is deterministic (fixed keys, fixed IVs, deterministic
 * signature schemes; randomized schemes print only verify results), so the
 * output diffs line for line against HotSpot 25. The FIRST run's lines are
 * printed; the same surface is then re-run {@code iters} times and every
 * line compared with the first run -- {@code drift} counts mismatches, so a
 * cold answer that the warm (compiled) path does not reproduce shows up
 * without a second process.
 *
 * <p>Arms (lane record): base --jdk-only, retired build --jdk-only, retired
 * build + CRATONVM_UNRETIRE_NATIVE_SHADOW, both builds --compatible.
 */
public class SbrW1Jca {

    static final int ITERS = Integer.getInteger("iters", 40);

    static final String RSA_PRIV = "MIICdQIBADANBgkqhkiG9w0BAQEFAASCAl8wggJbAgEAAoGBAJrH6sDKP7OKphF7ZiQY3DXolZBwuQFXeixMt+r93pnZh1ME6Eq5w3kVGi9bln1zvkK9dNmghXUuoV6Ijj8eGaFY/tCEq2sR0OFxXA899bE2iTE+6C55n3qyFPGg4GsebaETNG5g5Qm5olt5UvbK4PiNEMXHL1Sd0X2bEbdhGhQlAgMBAAECgYAxpa2VoGoVb4Q8OE43huffvNZ4qceV+Ba+re8Sh9MGm2FOTlUqXvKl3JB1H1YisGUXaEjaY6RbRZVKStgBw+hpN7xAbNhczIRUIc+9tfZqSREA10hUS4eSUhD4+UXmGZ6LWJdDjlLEH8huR8TsTRUr5eJlDnLHc/v67pQTbhT0MwJBAKHMM3HbKD1RgmWo+ASlGmMsfKuD53aMDYRR2g4eF2XmrdI/8HT0AHxQd4h9lC7ejnCi4Uf/ulOCk1SnSgqKfKMCQQD05eDDhmyNqy+ZNaafoHYx4/F7Bww2VCLazUpp2T9iIOYfywF2Gmx3INMNsvN17du2ggkkcYquUirtaaQ6hDCXAkBcwfjsWTxkiwo6xfgKkazjFmoS2ngkWn6VUWe1mELJxMn6M3XjVs/AdVFSCsydYjI/GOIWqOFlZwKYEaA9EtAJAkB6hZfRuqsMpZyGtkTKj+kSRMxHlNsTQzy84wBbvIjtezAmr9XnVIj3gMCWlfUgBwRpZt1UEkojtJAtfzDe5cu5AkA3aMxl/ODq9hchVRQVldpfgJrmY2kZLUP6766XMayBGZjQ9qNaZa1OKZym45nyI83yJzYQGDUZTfaX8kcDf2l/";
    static final String RSA_PUB = "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQCax+rAyj+ziqYRe2YkGNw16JWQcLkBV3osTLfq/d6Z2YdTBOhKucN5FRovW5Z9c75CvXTZoIV1LqFeiI4/HhmhWP7QhKtrEdDhcVwPPfWxNokxPugueZ96shTxoOBrHm2hEzRuYOUJuaJbeVL2yuD4jRDFxy9UndF9mxG3YRoUJQIDAQAB";
    static final String EC_PRIV = "MEECAQAwEwYHKoZIzj0CAQYIKoZIzj0DAQcEJzAlAgEBBCAmgTd+krPlp0+POAOGG/vdGRGggLagQ14UWfCayzmKkw==";
    static final String EC_PUB = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEoX2P+hwK/UhuypILXF8oNv4fYE/pQ7FAnzw8i7POf0ZkJslWKBvINxMYb1NXLDAy1OPPkfbMMPTOmYRFFonuMQ==";
    static final String CERT = "MIICADCCAWmgAwIBAgIINbepKR/b6XEwDQYJKoZIhvcNAQEMBQAwMTELMAkGA1UEBhMCVVMxDzANBgNVBAoTBkNyYXRvbjERMA8GA1UEAxMIU2JyIFRlc3QwHhcNMjYwOTI5MDAxMjI1WhcNMzYwOTI2MDAxMjI1WjAxMQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQ3JhdG9uMREwDwYDVQQDEwhTYnIgVGVzdDCBnzANBgkqhkiG9w0BAQEFAAOBjQAwgYkCgYEAmYwENtAkyU5AePcYyGY4fg8fG7Thg3L241V3DyNP8kITfEgt+Kc0xbDk+z2GbJYfuPdxr8uj+/n7v362uW9oROav5wVgao33IwuRLYE2syy0pPDbtSomFm31o4IGoLDWrweqwybkgHFE4+uMsM4/IRp4n6VoVMjFhQIImVKdAz0CAwEAAaMhMB8wHQYDVR0OBBYEFL33zXH3O7WnXKWuSTGkjNIRvA8aMA0GCSqGSIb3DQEBDAUAA4GBAD8unn6kTex7MHkBfR/e4IiTrE3BaJOUcRRXvShyRJcEmzzcdOsEiadX5przVLT1XlplTaEXq162hR7N0OUFDFff6rQ6dlQAuwZziPyheyuJY7ir746n+RxoD37GuFtPer0T2z35gbpFmRYpPeh5RA1+/tA+aWgHJnS0X7ldYVY/";

    static final byte[] DATA = new byte[64 * 1024];
    static final byte[] KEY16 = new byte[16];
    static final byte[] KEY32 = new byte[32];
    static final byte[] IV12 = new byte[12];
    static final byte[] IV16 = new byte[16];
    static {
        for (int i = 0; i < DATA.length; i++) DATA[i] = (byte) (i * 31 + (i >>> 8));
        for (int i = 0; i < 16; i++) { KEY16[i] = (byte) (i + 1); IV16[i] = (byte) (0xA0 + i); }
        for (int i = 0; i < 32; i++) KEY32[i] = (byte) (0x40 + i);
        for (int i = 0; i < 12; i++) IV12[i] = (byte) (0x10 + i);
    }

    static String hex(byte[] b) {
        if (b == null) return "null";
        StringBuilder sb = new StringBuilder();
        for (byte x : b) sb.append(Character.forDigit((x >> 4) & 15, 16)).append(Character.forDigit(x & 15, 16));
        return sb.toString();
    }

    static String sha(byte[] b) throws Exception {
        // A short fingerprint for long outputs, computed by a JDK-independent path.
        long h = 1125899906842597L;
        for (byte x : b) h = 31 * h + x;
        return b.length + ":" + Long.toHexString(h);
    }

    interface Check { Object run() throws Throwable; }

    static void c(List<String> out, String name, Check k) {
        String v;
        try {
            Object r = k.run();
            v = (r instanceof byte[]) ? hex((byte[]) r) : String.valueOf(r);
        } catch (Throwable t) {
            v = "THROW " + t.getClass().getName() + ": " + t.getMessage();
        }
        out.add(name + " = " + v);
    }

    static PrivateKey rsaPriv() throws Exception {
        return KeyFactory.getInstance("RSA").generatePrivate(new PKCS8EncodedKeySpec(Base64.getDecoder().decode(RSA_PRIV)));
    }
    static PublicKey rsaPub() throws Exception {
        return KeyFactory.getInstance("RSA").generatePublic(new X509EncodedKeySpec(Base64.getDecoder().decode(RSA_PUB)));
    }
    static PrivateKey ecPriv() throws Exception {
        return KeyFactory.getInstance("EC").generatePrivate(new PKCS8EncodedKeySpec(Base64.getDecoder().decode(EC_PRIV)));
    }
    static PublicKey ecPub() throws Exception {
        return KeyFactory.getInstance("EC").generatePublic(new X509EncodedKeySpec(Base64.getDecoder().decode(EC_PUB)));
    }

    static List<String> run() {
        List<String> o = new ArrayList<>();

        // --- SecretKeySpec / KeyPair (value classes) ---
        c(o, "sks.alg", () -> new SecretKeySpec(KEY16, "AES").getAlgorithm());
        c(o, "sks.fmt", () -> new SecretKeySpec(KEY16, "AES").getFormat());
        c(o, "sks.enc", () -> new SecretKeySpec(KEY16, "AES").getEncoded());
        c(o, "sks.off", () -> new SecretKeySpec(KEY32, 4, 8, "DES").getEncoded());
        c(o, "sks.eq", () -> new SecretKeySpec(KEY16, "AES").equals(new SecretKeySpec(KEY16, "aes")));
        c(o, "sks.hash", () -> new SecretKeySpec(KEY16, "AES").hashCode());
        c(o, "sks.null", () -> new SecretKeySpec(null, "AES"));
        c(o, "sks.empty", () -> new SecretKeySpec(new byte[0], "AES"));
        c(o, "sks.copy", () -> { byte[] k = KEY16.clone(); SecretKeySpec s = new SecretKeySpec(k, "AES"); k[0] = 99; return s.getEncoded()[0]; });
        c(o, "kp.same", () -> { PublicKey p = rsaPub(); PrivateKey q = rsaPriv(); KeyPair kp = new KeyPair(p, q); return (kp.getPublic() == p) + "," + (kp.getPrivate() == q); });
        c(o, "kp.null", () -> { KeyPair kp = new KeyPair(null, null); return kp.getPublic() + "," + kp.getPrivate(); });

        // --- MessageDigest ---
        for (String alg : new String[] {"SHA-256", "SHA-1", "MD5", "SHA-512", "SHA-384", "SHA3-256", "SHA-224", "SHA-512/256"}) {
            c(o, "md." + alg, () -> MessageDigest.getInstance(alg).digest(DATA));
        }
        c(o, "md.alg", () -> MessageDigest.getInstance("sha-256").getAlgorithm());
        c(o, "md.len", () -> MessageDigest.getInstance("SHA-384").getDigestLength());
        c(o, "md.prov", () -> MessageDigest.getInstance("SHA-256").getProvider().getName());
        c(o, "md.toString", () -> MessageDigest.getInstance("SHA-256").toString());
        c(o, "md.class", () -> MessageDigest.getInstance("SHA-256").getClass().getName());
        c(o, "md.incr", () -> { MessageDigest m = MessageDigest.getInstance("SHA-256"); m.update(DATA, 0, 1000); m.update((byte) 7); m.update(DATA); m.update(ByteBuffer.wrap(DATA, 5, 300)); return m.digest(); });
        c(o, "md.clone", () -> { MessageDigest m = MessageDigest.getInstance("SHA-256"); m.update(DATA, 0, 777); MessageDigest n = (MessageDigest) m.clone(); m.update((byte) 1); n.update((byte) 2); return hex(m.digest()) + "/" + hex(n.digest()); });
        c(o, "md.reset", () -> { MessageDigest m = MessageDigest.getInstance("MD5"); m.update(DATA); m.reset(); m.update(DATA, 0, 10); return m.digest(); });
        c(o, "md.reuse", () -> { MessageDigest m = MessageDigest.getInstance("SHA-1"); m.digest(DATA); return m.digest(); });
        c(o, "md.into", () -> { MessageDigest m = MessageDigest.getInstance("SHA-256"); m.update(DATA, 0, 99); byte[] b = new byte[40]; int n = m.digest(b, 4, 32); return n + ":" + hex(b); });
        c(o, "md.intoShort", () -> { MessageDigest m = MessageDigest.getInstance("SHA-256"); return m.digest(new byte[40], 20, 20); });
        c(o, "md.nope", () -> MessageDigest.getInstance("NOPE-1"));
        c(o, "md.nullAlg", () -> MessageDigest.getInstance(null));
        c(o, "md.provName", () -> MessageDigest.getInstance("SHA-256", "SUN").getAlgorithm());
        c(o, "md.provObj", () -> MessageDigest.getInstance("MD5", Security.getProvider("SUN")).digest(DATA));
        c(o, "md.badProv", () -> MessageDigest.getInstance("SHA-256", "NoSuchProv"));
        c(o, "md.isEqual", () -> MessageDigest.isEqual(KEY16, KEY16.clone()));

        // --- Mac ---
        c(o, "mac.hs256", () -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(KEY32, "HmacSHA256")); return m.doFinal(DATA); });
        c(o, "mac.hs1", () -> { Mac m = Mac.getInstance("HmacSHA1"); m.init(new SecretKeySpec(KEY16, "HmacSHA1")); m.update(DATA, 3, 1000); m.update((byte) 9); return m.doFinal(); });
        c(o, "mac.hs512", () -> { Mac m = Mac.getInstance("HmacSHA512"); m.init(new SecretKeySpec(KEY32, "RAW")); m.update(ByteBuffer.wrap(DATA)); return m.doFinal(); });
        c(o, "mac.len", () -> Mac.getInstance("HmacSHA384").getMacLength());
        c(o, "mac.alg", () -> Mac.getInstance("hmacsha256").getAlgorithm());
        c(o, "mac.prov", () -> Mac.getInstance("HmacSHA256").getProvider().getName());
        c(o, "mac.reuse", () -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(KEY16, "H")); m.doFinal(DATA); m.update(KEY16); return m.doFinal(); });
        c(o, "mac.reset", () -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(KEY16, "H")); m.update(DATA); m.reset(); return m.doFinal(KEY16); });
        c(o, "mac.clone", () -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(KEY16, "H")); m.update(DATA, 0, 50); Mac n = (Mac) m.clone(); m.update((byte) 1); return hex(m.doFinal()) + "/" + hex(n.doFinal()); });
        c(o, "mac.into", () -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(KEY16, "H")); byte[] b = new byte[40]; m.doFinal(b, 8); return b; });
        c(o, "mac.noInit", () -> Mac.getInstance("HmacSHA256").doFinal());
        c(o, "mac.nullKey", () -> { Mac.getInstance("HmacSHA256").init(null); return "ok"; });
        c(o, "mac.nope", () -> Mac.getInstance("HmacNOPE"));

        // --- Cipher ---
        c(o, "gcm.enc", () -> { Cipher x = Cipher.getInstance("AES/GCM/NoPadding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(128, IV12)); x.updateAAD(KEY32); return sha(x.doFinal(DATA)); });
        c(o, "gcm.small", () -> { Cipher x = Cipher.getInstance("AES/GCM/NoPadding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY32, "AES"), new GCMParameterSpec(96, IV12)); x.updateAAD(KEY16, 2, 5); return x.doFinal(DATA, 0, 45); });
        c(o, "gcm.rt", () -> {
            Cipher e = Cipher.getInstance("AES/GCM/NoPadding"); e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(128, IV12)); e.updateAAD(KEY32); byte[] ct = e.doFinal(DATA, 0, 4000);
            Cipher d = Cipher.getInstance("AES/GCM/NoPadding"); d.init(Cipher.DECRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(128, IV12)); d.updateAAD(KEY32); byte[] pt = d.doFinal(ct);
            return Arrays.equals(pt, Arrays.copyOf(DATA, 4000)); });
        c(o, "gcm.tamper", () -> {
            Cipher e = Cipher.getInstance("AES/GCM/NoPadding"); e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(128, IV12)); byte[] ct = e.doFinal(DATA, 0, 64); ct[3] ^= 1;
            Cipher d = Cipher.getInstance("AES/GCM/NoPadding"); d.init(Cipher.DECRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(128, IV12)); return d.doFinal(ct); });
        c(o, "cbc.enc", () -> { Cipher x = Cipher.getInstance("AES/CBC/PKCS5Padding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); return x.doFinal(DATA, 0, 100); });
        c(o, "cbc.parts", () -> { Cipher x = Cipher.getInstance("AES/CBC/PKCS5Padding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); byte[] a = x.update(DATA, 0, 37); byte[] b = x.doFinal(DATA, 37, 63); return hex(a) + "|" + hex(b); });
        c(o, "cbc.decParts", () -> { Cipher x = Cipher.getInstance("AES/CBC/PKCS5Padding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); byte[] ct = x.doFinal(DATA, 0, 100); Cipher d = Cipher.getInstance("AES/CBC/PKCS5Padding"); d.init(Cipher.DECRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); byte[] a = d.update(ct, 0, 48); byte[] b = d.update(ct, 48, 16); byte[] e = d.doFinal(ct, 64, ct.length - 64); return a.length + "," + b.length + "," + e.length + "," + hex(a).substring(0, 8); });
        c(o, "ecb.parts", () -> { Cipher x = Cipher.getInstance("AES/ECB/NoPadding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES")); byte[] a = x.update(DATA, 0, 20); byte[] b = x.update(DATA, 20, 12); byte[] e = x.doFinal(); return hex(a) + "|" + hex(b) + "|" + hex(e); });
        c(o, "ctr.parts", () -> { Cipher x = Cipher.getInstance("AES/CTR/NoPadding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); byte[] a = x.update(DATA, 0, 5); byte[] b = x.update(DATA, 5, 30); byte[] e = x.doFinal(DATA, 35, 3); return hex(a) + "|" + hex(b) + "|" + hex(e); });
        c(o, "cbc.out", () -> { Cipher x = Cipher.getInstance("AES/CBC/PKCS5Padding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); byte[] out = new byte[128]; int n = x.doFinal(DATA, 0, 50, out); return n + ":" + hex(out); });
        c(o, "cbc.dec", () -> { Cipher x = Cipher.getInstance("AES/CBC/PKCS5Padding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); byte[] ct = x.doFinal(DATA, 0, 100); Cipher d = Cipher.getInstance("AES/CBC/PKCS5Padding"); d.init(Cipher.DECRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); return Arrays.equals(d.doFinal(ct), Arrays.copyOf(DATA, 100)); });
        c(o, "ecb.enc", () -> { Cipher x = Cipher.getInstance("AES/ECB/NoPadding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY32, "AES")); return x.doFinal(DATA, 0, 32); });
        c(o, "ctr.enc", () -> { Cipher x = Cipher.getInstance("AES/CTR/NoPadding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); return x.doFinal(DATA, 0, 50); });
        c(o, "chacha.enc", () -> { Cipher x = Cipher.getInstance("ChaCha20-Poly1305"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY32, "ChaCha20"), new IvParameterSpec(IV12)); x.updateAAD(KEY16); return x.doFinal(DATA, 0, 70); });
        c(o, "cipher.meta", () -> { Cipher x = Cipher.getInstance("AES/CBC/PKCS5Padding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); return x.getAlgorithm() + "," + x.getBlockSize() + "," + x.getOutputSize(17) + "," + hex(x.getIV()) + "," + x.getProvider().getName(); });
        c(o, "cipher.gcmOut", () -> { Cipher x = Cipher.getInstance("AES/GCM/NoPadding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(128, IV12)); return x.getOutputSize(100) + "," + x.getBlockSize(); });
        c(o, "cipher.params", () -> { Cipher x = Cipher.getInstance("AES/CBC/PKCS5Padding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); return x.getParameters().getAlgorithm(); });
        c(o, "cipher.bb", () -> { Cipher x = Cipher.getInstance("AES/CTR/NoPadding"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new IvParameterSpec(IV16)); ByteBuffer in = ByteBuffer.wrap(DATA, 0, 40); ByteBuffer out = ByteBuffer.allocate(64); int n = x.doFinal(in, out); return n + ":" + hex(Arrays.copyOf(out.array(), n)); });
        c(o, "cipher.wrap", () -> { Cipher w = Cipher.getInstance("AESWrap"); w.init(Cipher.WRAP_MODE, new SecretKeySpec(KEY16, "AES")); byte[] wr = w.wrap(new SecretKeySpec(KEY32, "AES")); Cipher u = Cipher.getInstance("AESWrap"); u.init(Cipher.UNWRAP_MODE, new SecretKeySpec(KEY16, "AES")); Key k = u.unwrap(wr, "AES", Cipher.SECRET_KEY); return hex(wr) + "|" + hex(k.getEncoded()); });
        c(o, "cipher.rsa", () -> { Cipher x = Cipher.getInstance("RSA/ECB/PKCS1Padding"); x.init(Cipher.DECRYPT_MODE, rsaPriv()); Cipher e = Cipher.getInstance("RSA/ECB/PKCS1Padding"); e.init(Cipher.ENCRYPT_MODE, rsaPub()); return Arrays.equals(x.doFinal(e.doFinal(KEY32)), KEY32); });
        c(o, "cipher.nope", () -> Cipher.getInstance("AES/XYZ/NoPadding"));
        c(o, "cipher.null", () -> Cipher.getInstance(null));
        c(o, "cipher.noInit", () -> Cipher.getInstance("AES/CBC/PKCS5Padding").doFinal(DATA, 0, 16));
        c(o, "cipher.badKey", () -> { Cipher.getInstance("AES/CBC/PKCS5Padding").init(Cipher.ENCRYPT_MODE, new SecretKeySpec(new byte[5], "AES"), new IvParameterSpec(IV16)); return "ok"; });

        // --- Signature ---
        c(o, "sig.rsa", () -> { Signature s = Signature.getInstance("SHA256withRSA"); s.initSign(rsaPriv()); s.update(DATA); return s.sign(); });
        c(o, "sig.rsaV", () -> { Signature s = Signature.getInstance("SHA256withRSA"); s.initSign(rsaPriv()); s.update(DATA, 10, 500); s.update((byte) 3); byte[] g = s.sign(); Signature v = Signature.getInstance("SHA256withRSA"); v.initVerify(rsaPub()); v.update(DATA, 10, 500); v.update((byte) 3); boolean ok = v.verify(g); v.update(DATA, 10, 500); return ok + "," + v.verify(g); });
        c(o, "sig.rsaInto", () -> { Signature s = Signature.getInstance("SHA1withRSA"); s.initSign(rsaPriv()); s.update(ByteBuffer.wrap(DATA, 0, 100)); byte[] b = new byte[140]; int n = s.sign(b, 6, 134); return n + ":" + sha(b); });
        c(o, "sig.ec", () -> { Signature s = Signature.getInstance("SHA256withECDSA"); s.initSign(ecPriv()); s.update(DATA); byte[] g = s.sign(); Signature v = Signature.getInstance("SHA256withECDSA"); v.initVerify(ecPub()); v.update(DATA); boolean ok = v.verify(g); v.update(KEY16); return ok + "," + v.verify(g); });
        c(o, "sig.meta", () -> { Signature s = Signature.getInstance("SHA256withRSA"); String a = s.getAlgorithm(); s.initSign(rsaPriv()); return a + "," + s.getProvider().getName() + "," + s.toString().startsWith("Signature object: SHA256withRSA"); });
        c(o, "sig.pss", () -> { Signature s = Signature.getInstance("RSASSA-PSS"); s.setParameter(new PSSParameterSpec("SHA-256", "MGF1", MGF1ParameterSpec.SHA256, 32, 1)); s.initSign(rsaPriv()); s.update(DATA); byte[] g = s.sign(); Signature v = Signature.getInstance("RSASSA-PSS"); v.setParameter(new PSSParameterSpec("SHA-256", "MGF1", MGF1ParameterSpec.SHA256, 32, 1)); v.initVerify(rsaPub()); v.update(DATA); return g.length + "," + v.verify(g); });
        c(o, "sig.noInit", () -> { Signature s = Signature.getInstance("SHA256withRSA"); s.update(DATA); return "ok"; });
        c(o, "sig.nullKey", () -> { Signature.getInstance("SHA256withRSA").initSign(null); return "ok"; });
        c(o, "sig.nope", () -> Signature.getInstance("NOPEwithRSA"));
        c(o, "sig.null", () -> Signature.getInstance(null));

        // --- KeyFactory / KeyPairGenerator / SecretKeyFactory ---
        c(o, "kf.rsaPub", () -> { PublicKey p = rsaPub(); return p.getAlgorithm() + "," + p.getFormat() + "," + sha(p.getEncoded()); });
        c(o, "kf.rsaPriv", () -> { PrivateKey p = rsaPriv(); return p.getAlgorithm() + "," + p.getFormat() + "," + sha(p.getEncoded()); });
        c(o, "kf.ecPub", () -> { PublicKey p = ecPub(); return p.getAlgorithm() + "," + p.getFormat() + "," + hex(p.getEncoded()); });
        c(o, "kf.spec", () -> { RSAPublicKeySpec s = KeyFactory.getInstance("RSA").getKeySpec(rsaPub(), RSAPublicKeySpec.class); return s.getPublicExponent() + "," + s.getModulus().bitLength(); });
        c(o, "kf.fromSpec", () -> { RSAPublicKeySpec s = KeyFactory.getInstance("RSA").getKeySpec(rsaPub(), RSAPublicKeySpec.class); return sha(KeyFactory.getInstance("RSA").generatePublic(s).getEncoded()); });
        c(o, "kf.meta", () -> { KeyFactory f = KeyFactory.getInstance("EC"); return f.getAlgorithm() + "," + f.getProvider().getName(); });
        c(o, "kf.bad", () -> KeyFactory.getInstance("RSA").generatePublic(new X509EncodedKeySpec(new byte[] {1, 2, 3})));
        c(o, "kf.null", () -> KeyFactory.getInstance(null));
        c(o, "kpg.ec", () -> { KeyPairGenerator g = KeyPairGenerator.getInstance("EC"); g.initialize(256); KeyPair kp = g.generateKeyPair(); return g.getAlgorithm() + "," + g.getProvider().getName() + "," + kp.getPublic().getAlgorithm() + "," + kp.getPublic().getFormat() + "," + kp.getPublic().getEncoded().length + "," + kp.getPrivate().getFormat(); });
        c(o, "kpg.ecSpec", () -> { KeyPairGenerator g = KeyPairGenerator.getInstance("EC"); g.initialize(new ECGenParameterSpec("secp384r1")); KeyPair kp = g.genKeyPair(); return kp.getPublic().getEncoded().length; });
        c(o, "kpg.rsa", () -> { KeyPairGenerator g = KeyPairGenerator.getInstance("RSA"); g.initialize(1024, new SecureRandom()); KeyPair kp = g.generateKeyPair(); return ((java.security.interfaces.RSAPublicKey) kp.getPublic()).getModulus().bitLength() + "," + kp.getPrivate().getAlgorithm(); });
        c(o, "kpg.ed", () -> { KeyPairGenerator g = KeyPairGenerator.getInstance("Ed25519"); KeyPair kp = g.generateKeyPair(); Signature s = Signature.getInstance("Ed25519"); s.initSign(kp.getPrivate()); s.update(DATA, 0, 100); byte[] sg = s.sign(); Signature v = Signature.getInstance("Ed25519"); v.initVerify(kp.getPublic()); v.update(DATA, 0, 100); return sg.length + "," + v.verify(sg); });
        c(o, "kpg.bad", () -> { KeyPairGenerator.getInstance("RSA").initialize(0); return "ok"; });
        c(o, "skf.pbkdf2", () -> { SecretKeyFactory f = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256"); SecretKey k = f.generateSecret(new PBEKeySpec("password".toCharArray(), KEY16, 1000, 256)); return k.getAlgorithm() + "," + hex(k.getEncoded()); });
        c(o, "skf.meta", () -> { SecretKeyFactory f = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA1"); return f.getAlgorithm() + "," + f.getProvider().getName(); });
        c(o, "skf.des", () -> { SecretKeyFactory f = SecretKeyFactory.getInstance("DESede"); return hex(f.generateSecret(new DESedeKeySpec(new byte[24])).getEncoded()); });

        // --- Security / Provider / Service / GetInstance ---
        c(o, "sec.providers", () -> { StringBuilder sb = new StringBuilder(); for (Provider p : Security.getProviders()) sb.append(p.getName()).append(' '); return sb.toString().trim(); });
        c(o, "sec.getProvider", () -> Security.getProvider("SunJCE").getName() + "," + Security.getProvider("NoSuch"));
        c(o, "sec.prop", () -> Security.getProperty("jdk.certpath.disabledAlgorithms") != null);
        c(o, "sec.propMissing", () -> Security.getProperty("sbr.no.such.property"));
        c(o, "sec.algs", () -> new TreeSet<>(Security.getAlgorithms("MessageDigest")).toString());
        c(o, "sec.algsMac", () -> Security.getAlgorithms("Mac").contains("HMACSHA256"));
        c(o, "sec.filter", () -> { Provider[] ps = Security.getProviders("MessageDigest.SHA-256"); return ps == null ? "null" : ps.length + ":" + ps[0].getName(); });
        c(o, "prov.meta", () -> { Provider p = Security.getProvider("SUN"); return p.getName() + "," + p.getVersionStr() + "," + p.getInfo().startsWith("SUN ") + "," + p.toString(); });
        c(o, "prov.svc", () -> { Provider.Service s = Security.getProvider("SUN").getService("MessageDigest", "SHA-256"); return s.getType() + "," + s.getAlgorithm() + "," + s.getClassName() + "," + s.getProvider().getName(); });
        c(o, "prov.svcAlias", () -> { Provider.Service s = Security.getProvider("SUN").getService("MessageDigest", "SHA256"); return s == null ? "null" : s.getAlgorithm(); });
        c(o, "prov.svcAttr", () -> Security.getProvider("SUN").getService("MessageDigest", "SHA-256").getAttribute("ImplementedIn"));
        c(o, "prov.svcNew", () -> Security.getProvider("SUN").getService("MessageDigest", "SHA-256").newInstance(null).getClass().getName());
        c(o, "prov.svcStr", () -> Security.getProvider("SunJCE").getService("Cipher", "AES").toString().split("\n")[0]);
        c(o, "prov.svcMissing", () -> Security.getProvider("SUN").getService("MessageDigest", "NOPE"));
        // Class only: HotSpot's own C2 drops the helpful-NPE text once hot.
        c(o, "prov.svcNull", () -> { try { return Security.getProvider("SUN").getService(null, "SHA-256"); } catch (NullPointerException e) { return "NPE"; } });
        c(o, "prov.map", () -> { Provider p = Security.getProvider("SUN"); return p.containsKey("MessageDigest.SHA-256") + "," + p.get("MessageDigest.SHA-256") + "," + p.getProperty("MessageDigest.SHA-1") + "," + (p.size() > 10) + "," + p.isEmpty(); });
        c(o, "prov.services", () -> { int n = 0; for (Provider.Service s : Security.getProvider("SunJCE").getServices()) if (s.getType().equals("Cipher")) n++; return n > 5; });
        c(o, "prov.keys", () -> { int n = 0; for (Object k : Security.getProvider("SUN").keySet()) if (String.valueOf(k).startsWith("MessageDigest.")) n++; return n > 5; });
        c(o, "prov.custom", () -> {
            Provider p = new Provider("SbrProv", "1.0", "sbr test provider") {};
            p.put("MessageDigest.SBR-HASH", "sun.security.provider.SHA2$SHA256");
            Provider.Service s = p.getService("MessageDigest", "SBR-HASH");
            return p.getName() + "," + p.getVersionStr() + "," + p.getInfo() + "," + (s == null ? "null" : s.getClassName()) + "," + hex(MessageDigest.getInstance("SBR-HASH", p).digest(KEY16));
        });
        c(o, "prov.customSvc", () -> {
            Provider p = new Provider("SbrProv2", "2.0", "x") {};
            Provider.Service s = new Provider.Service(p, "Mac", "SBR-MAC", "com.sun.crypto.provider.HmacCore$HmacSHA256", List.of("SBRMAC"), Map.of("K", "V")) {};
            return s.getType() + "," + s.getAlgorithm() + "," + s.getAttribute("K") + "," + s.getClassName();
        });

        // --- engines outside the family that resolve through GetInstance /
        // Security / Provider: a retirement there changes their path too ---
        c(o, "ka.ecdh", () -> { KeyAgreement k = KeyAgreement.getInstance("ECDH"); k.init(ecPriv()); k.doPhase(ecPub(), true); return sha(k.generateSecret()) + "," + k.getProvider().getName(); });
        c(o, "ka.x25519", () -> { KeyPairGenerator g = KeyPairGenerator.getInstance("X25519"); KeyPair a = g.generateKeyPair(), b = g.generateKeyPair(); KeyAgreement k1 = KeyAgreement.getInstance("X25519"); k1.init(a.getPrivate()); k1.doPhase(b.getPublic(), true); KeyAgreement k2 = KeyAgreement.getInstance("XDH"); k2.init(b.getPrivate()); k2.doPhase(a.getPublic(), true); return Arrays.equals(k1.generateSecret(), k2.generateSecret()); });
        c(o, "kg.aes", () -> { KeyGenerator g = KeyGenerator.getInstance("AES"); g.init(256); SecretKey k = g.generateKey(); return k.getAlgorithm() + "," + k.getEncoded().length + "," + g.getProvider().getName(); });
        c(o, "kg.hmac", () -> { KeyGenerator g = KeyGenerator.getInstance("HmacSHA256"); return g.generateKey().getEncoded().length; });
        c(o, "ap.ec", () -> { AlgorithmParameters p = AlgorithmParameters.getInstance("EC"); p.init(new ECGenParameterSpec("secp256r1")); return p.getParameterSpec(ECParameterSpec.class).getOrder().bitLength() + "," + p.getProvider().getName(); });
        c(o, "ap.gcm", () -> { AlgorithmParameters p = AlgorithmParameters.getInstance("GCM"); p.init(new GCMParameterSpec(128, IV12)); return hex(p.getEncoded()); });
        c(o, "sr.drbg", () -> { SecureRandom r = SecureRandom.getInstance("DRBG"); byte[] b = new byte[16]; r.nextBytes(b); return r.getAlgorithm() + "," + r.getProvider().getName() + "," + b.length; });
        c(o, "sr.sha1", () -> { SecureRandom r = SecureRandom.getInstance("SHA1PRNG"); r.setSeed(42L); byte[] b = new byte[8]; r.nextBytes(b); return r.getAlgorithm() + "," + b.length; });
        c(o, "ks.p12", () -> { KeyStore k = KeyStore.getInstance("PKCS12"); k.load(null, null); return k.getType() + "," + k.size() + "," + k.getProvider().getName(); });
        c(o, "ks.jks", () -> { KeyStore k = KeyStore.getInstance(KeyStore.getDefaultType()); k.load(null, null); return k.getType(); });
        c(o, "tmf", () -> { javax.net.ssl.TrustManagerFactory f = javax.net.ssl.TrustManagerFactory.getInstance(javax.net.ssl.TrustManagerFactory.getDefaultAlgorithm()); return f.getAlgorithm() + "," + f.getProvider().getName(); });
        c(o, "cpv", () -> CertPathValidator.getInstance("PKIX").getAlgorithm());
        c(o, "sig.ed", () -> { KeyPairGenerator g = KeyPairGenerator.getInstance("Ed25519"); KeyPair kp = g.generateKeyPair(); Signature s = Signature.getInstance("EdDSA"); s.initSign(kp.getPrivate()); s.update(KEY32); byte[] sg = s.sign(); Signature v = Signature.getInstance("Ed25519"); v.initVerify(kp.getPublic()); v.update(KEY32); return v.verify(sg); });
        c(o, "kf.x25519", () -> KeyFactory.getInstance("X25519").getAlgorithm());
        c(o, "kf.xdh", () -> { KeyPairGenerator g = KeyPairGenerator.getInstance("XDH"); g.initialize(new NamedParameterSpec("X448")); return g.generateKeyPair().getPublic().getAlgorithm(); });
        c(o, "cipher.provName", () -> Cipher.getInstance("AES/GCM/NoPadding", "SunJCE").getProvider().getName());
        c(o, "mac.provObj", () -> { Mac m = Mac.getInstance("HmacSHA256", Security.getProvider("SunJCE")); m.init(new SecretKeySpec(KEY16, "H")); return m.doFinal(KEY16); });
        c(o, "sec.addRemove", () -> { Provider p = new Provider("SbrProv3", "3.0", "x") {}; int a = Security.addProvider(p); int b = Security.addProvider(p); Security.removeProvider("SbrProv3"); return (a > 0) + "," + b + "," + Security.getProvider("SbrProv3"); });

        // --- wave 2: third-party providers installed through the REAL
        // Security.addProvider, as seen by every engine; provider identity ---
        c(o, "tp.all", () -> {
            Provider p = new Provider("SbrTp", "1.0", "sbr third party") {};
            p.put("MessageDigest.SBR-HASH2", "sun.security.provider.SHA2$SHA256");
            p.put("Mac.SBR-HMAC", "com.sun.crypto.provider.HmacCore$HmacSHA256");
            p.put("Cipher.SBR-AES", "com.sun.crypto.provider.AESCipher$General");
            p.put("Signature.SBR-SIG", "sun.security.rsa.RSASignature$SHA256withRSA");
            p.put("KeyFactory.SBR-KF", "sun.security.rsa.RSAKeyFactory$Legacy");
            StringBuilder sb = new StringBuilder();
            int pos = Security.addProvider(p);
            sb.append(pos > 0).append(';');
            try {
                for (Check k : new Check[] {
                    () -> hex(MessageDigest.getInstance("SBR-HASH2").digest(KEY16)),
                    () -> MessageDigest.getInstance("SBR-HASH2", "SbrTp").getProvider().getName(),
                    () -> { Mac m = Mac.getInstance("SBR-HMAC"); m.init(new SecretKeySpec(KEY16, "H")); return hex(m.doFinal(KEY16)); },
                    () -> { Cipher x = Cipher.getInstance("SBR-AES"); x.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES")); return hex(x.doFinal(KEY16)); },
                    () -> Cipher.getInstance("SBR-AES", "SbrTp").getProvider().getName(),
                    () -> { Signature s = Signature.getInstance("SBR-SIG"); s.initSign(rsaPriv()); s.update(KEY16); return sha(s.sign()); },
                    () -> KeyFactory.getInstance("SBR-KF").generatePublic(new X509EncodedKeySpec(Base64.getDecoder().decode(RSA_PUB))).getAlgorithm(),
                }) {
                    String v;
                    try { v = String.valueOf(k.run()); } catch (Throwable t) { v = "THROW " + t.getClass().getName() + ": " + t.getMessage(); }
                    sb.append(v).append(';');
                }
            } finally {
                Security.removeProvider("SbrTp");
            }
            return sb.toString();
        });
        c(o, "id.md", () -> MessageDigest.getInstance("SHA-256").getProvider() == Security.getProvider("SUN"));
        c(o, "id.mac", () -> Mac.getInstance("HmacSHA256").getProvider() == Security.getProvider("SunJCE"));
        c(o, "id.cipher", () -> Cipher.getInstance("AES/GCM/NoPadding").getProvider() == Security.getProvider("SunJCE"));
        c(o, "id.sig", () -> Signature.getInstance("SHA256withRSA").getProvider().getName());
        c(o, "id.kpg", () -> KeyPairGenerator.getInstance("EC").getProvider() == Security.getProvider("SunEC"));
        c(o, "prov.class", () -> Security.getProvider("SUN").getClass().getName() + "," + Security.getProvider("SunJCE").getClass().getName());
        c(o, "prov.svcAttrs", () -> { Provider.Service s = Security.getProvider("SunJCE").getService("Mac", "HmacSHA256"); return s.getAttribute("SupportedKeyFormats") + "," + s.getClass().getName(); });
        c(o, "prov.svcEc", () -> { Provider.Service s = Security.getProvider("SunEC").getService("KeyFactory", "X25519"); return s.getClassName() + "," + s.newInstance(null).getClass().getName(); });

        // --- CertificateFactory ---
        c(o, "cf.x509", () -> {
            CertificateFactory f = CertificateFactory.getInstance("X.509");
            X509Certificate x = (X509Certificate) f.generateCertificate(new java.io.ByteArrayInputStream(Base64.getDecoder().decode(CERT)));
            return f.getType() + "," + x.getSubjectX500Principal().getName() + "," + x.getSerialNumber().toString(16) + "," + x.getSigAlgName() + "," + x.getPublicKey().getAlgorithm() + "," + x.getVersion();
        });
        c(o, "cf.verify", () -> {
            X509Certificate x = (X509Certificate) CertificateFactory.getInstance("X.509").generateCertificate(new java.io.ByteArrayInputStream(Base64.getDecoder().decode(CERT)));
            x.verify(x.getPublicKey()); x.checkValidity(new Date(1900000000000L)); return "verified";
        });
        c(o, "cf.certs", () -> CertificateFactory.getInstance("X.509").generateCertificates(new java.io.ByteArrayInputStream(Base64.getDecoder().decode(CERT))).size());
        c(o, "cf.bad", () -> CertificateFactory.getInstance("X.509").generateCertificate(new java.io.ByteArrayInputStream(new byte[] {0x30, 0x03, 1, 2, 3})));
        c(o, "cf.nope", () -> CertificateFactory.getInstance("NOPE"));
        wave5(o);
        return o;
    }

    // --- wave 5 (lane cryptoeng): the SUN/SunJCE engines' @IntrinsicCandidate
    // kernels, now reviewed Intrinsics under real engine bytecode. Random
    // data (fixed seed), every size class around the block boundaries, split
    // updates, in-place and overlapping buffers, the non-AES feedback-mode
    // fallbacks, and the ChaCha20 counter limit. ---
    static final byte[] RND = new byte[70_000];
    static { new Random(0x5B5_C0DEL).nextBytes(RND); }
    static final int[] SIZES = {0, 1, 15, 16, 17, 55, 56, 63, 64, 65, 111, 112, 127, 128, 129, 1000, 4095, 4096, 4097, 65_536 + 5};

    static void wave5(List<String> o) {
        for (String alg : new String[] {"SHA-1", "SHA-224", "SHA-256", "SHA-384", "SHA-512", "SHA-512/224", "SHA-512/256", "MD5", "MD2", "SHA3-224", "SHA3-256", "SHA3-384", "SHA3-512"}) {
            c(o, "w5.md." + alg, () -> {
                StringBuilder sb = new StringBuilder();
                for (int n : SIZES) sb.append(hex(MessageDigest.getInstance(alg).digest(Arrays.copyOfRange(RND, 3, 3 + n))), 0, 12).append(',');
                MessageDigest m = MessageDigest.getInstance(alg);
                // split updates that straddle block boundaries, then single bytes
                for (int off = 0, step = 1; off < 9000; off += step, step = step * 3 % 257 + 1) m.update(RND, off, Math.min(step, 9000 - off));
                for (int i = 0; i < 70; i++) m.update(RND[i]);
                MessageDigest cl = (MessageDigest) m.clone();
                m.update(RND, 100, 300);
                sb.append(hex(m.digest())).append('/').append(hex(cl.digest()));
                return sb.toString();
            });
        }
        c(o, "w5.md.bb", () -> { MessageDigest m = MessageDigest.getInstance("SHA-256"); m.update(ByteBuffer.wrap(RND, 7, 5000)); ByteBuffer d = ByteBuffer.allocateDirect(3000); d.put(RND, 0, 3000).flip(); m.update(d); return m.digest(); });
        c(o, "w5.md.into", () -> { MessageDigest m = MessageDigest.getInstance("SHA-512"); m.update(RND, 0, 129); byte[] b = new byte[80]; return m.digest(b, 3, 64) + ":" + hex(b); });
        for (String alg : new String[] {"HmacSHA1", "HmacSHA224", "HmacSHA256", "HmacSHA384", "HmacSHA512", "HmacMD5", "HmacSHA3-256", "HmacSHA512/256"}) {
            c(o, "w5.mac." + alg, () -> {
                StringBuilder sb = new StringBuilder();
                for (int klen : new int[] {1, 32, 64, 65, 200}) {
                    Mac m = Mac.getInstance(alg);
                    m.init(new SecretKeySpec(Arrays.copyOfRange(RND, 500, 500 + klen), alg));
                    for (int n : SIZES) { m.update(RND, 11, n); }
                    sb.append(hex(m.doFinal()), 0, 16).append(',');
                }
                return sb.toString();
            });
        }
        c(o, "w5.pbkdf2", () -> hex(SecretKeyFactory.getInstance("PBKDF2WithHmacSHA512").generateSecret(new PBEKeySpec("pw".toCharArray(), KEY16, 2000, 512)).getEncoded()));

        // AES modes x key sizes x sizes, one-shot and split
        for (String tr : new String[] {"AES/ECB/NoPadding", "AES/ECB/PKCS5Padding", "AES/CBC/NoPadding", "AES/CBC/PKCS5Padding", "AES/CTR/NoPadding", "AES/CFB/NoPadding", "AES/OFB/NoPadding", "AES/PCBC/PKCS5Padding", "AES/CTS/NoPadding"}) {
            for (int kl : new int[] {16, 24, 32}) {
                c(o, "w5.aes." + tr + "." + kl, () -> {
                    boolean pad = tr.contains("PKCS5"), stream = tr.contains("CTR") || tr.contains("CFB/") || tr.contains("OFB"), cts = tr.contains("CTS");
                    SecretKeySpec k = new SecretKeySpec(Arrays.copyOfRange(RND, 900, 900 + kl), "AES");
                    StringBuilder sb = new StringBuilder();
                    for (int n : SIZES) {
                        if (!pad && !stream && !cts && n % 16 != 0) continue;
                        if (cts && n < 17) continue;
                        Cipher e = Cipher.getInstance(tr);
                        if (tr.contains("ECB")) e.init(Cipher.ENCRYPT_MODE, k); else e.init(Cipher.ENCRYPT_MODE, k, new IvParameterSpec(Arrays.copyOfRange(RND, 40, 56)));
                        byte[] ct = e.doFinal(RND, 5, n);
                        // split: update(7) update(rest) doFinal
                        Cipher e2 = Cipher.getInstance(tr);
                        if (tr.contains("ECB")) e2.init(Cipher.ENCRYPT_MODE, k); else e2.init(Cipher.ENCRYPT_MODE, k, new IvParameterSpec(Arrays.copyOfRange(RND, 40, 56)));
                        int a = Math.min(7, n);
                        byte[] p1 = e2.update(RND, 5, a); byte[] p2 = e2.update(RND, 5 + a, n - a); byte[] p3 = e2.doFinal();
                        byte[] joined = concat(p1, p2, p3);
                        Cipher d = Cipher.getInstance(tr);
                        if (tr.contains("ECB")) d.init(Cipher.DECRYPT_MODE, k); else d.init(Cipher.DECRYPT_MODE, k, new IvParameterSpec(Arrays.copyOfRange(RND, 40, 56)));
                        byte[] pt = d.doFinal(ct);
                        sb.append(sha(ct)).append(Arrays.equals(ct, joined) ? "" : "!split").append(Arrays.equals(pt, Arrays.copyOfRange(RND, 5, 5 + n)) ? "" : "!rt").append(',');
                    }
                    return sb.toString();
                });
            }
        }
        // in place, and overlapping input/output in one array
        for (String tr : new String[] {"AES/CBC/NoPadding", "AES/CTR/NoPadding", "AES/ECB/NoPadding", "AES/GCM/NoPadding", "ChaCha20-Poly1305"}) {
            c(o, "w5.inplace." + tr, () -> {
                StringBuilder sb = new StringBuilder();
                for (int shift : new int[] {0, 5, -5, 16, -16, 33}) {
                    byte[] buf = Arrays.copyOf(RND, 5000);
                    Cipher e = Cipher.getInstance(tr);
                    SecretKeySpec k = new SecretKeySpec(KEY32, tr.startsWith("Cha") ? "ChaCha20" : "AES");
                    if (tr.contains("GCM")) e.init(Cipher.ENCRYPT_MODE, k, new GCMParameterSpec(128, IV12));
                    else if (tr.startsWith("Cha")) e.init(Cipher.ENCRYPT_MODE, k, new IvParameterSpec(IV12));
                    else if (tr.contains("ECB")) e.init(Cipher.ENCRYPT_MODE, k);
                    else e.init(Cipher.ENCRYPT_MODE, k, new IvParameterSpec(IV16));
                    int in = 100, out = 100 + shift;
                    int n;
                    try { n = e.doFinal(buf, in, 4096, buf, out); } catch (Exception ex) { sb.append(ex.getClass().getSimpleName()).append(','); continue; }
                    sb.append(n).append(':').append(sha(Arrays.copyOfRange(buf, out, out + n))).append(',');
                }
                return sb.toString();
            });
        }
        // GCM: AAD, split updates, decrypt + tag failure, ByteBuffer (heap and direct)
        for (int kl : new int[] {16, 24, 32}) {
            c(o, "w5.gcm." + kl, () -> {
                SecretKeySpec k = new SecretKeySpec(Arrays.copyOfRange(RND, 77, 77 + kl), "AES");
                StringBuilder sb = new StringBuilder();
                for (int n : SIZES) {
                    Cipher e = Cipher.getInstance("AES/GCM/NoPadding");
                    e.init(Cipher.ENCRYPT_MODE, k, new GCMParameterSpec(128, Arrays.copyOfRange(RND, n, n + 12)));
                    e.updateAAD(RND, 17, n % 45);
                    byte[] a = e.update(RND, 1, n / 3); byte[] b = e.doFinal(RND, 1 + n / 3, n - n / 3);
                    byte[] ct = concat(a, b);
                    Cipher d = Cipher.getInstance("AES/GCM/NoPadding");
                    d.init(Cipher.DECRYPT_MODE, k, new GCMParameterSpec(128, Arrays.copyOfRange(RND, n, n + 12)));
                    d.updateAAD(RND, 17, n % 45);
                    boolean ok = Arrays.equals(d.doFinal(ct), Arrays.copyOfRange(RND, 1, 1 + n));
                    sb.append(sha(ct)).append(ok ? "" : "!rt").append(',');
                }
                return sb.toString();
            });
        }
        c(o, "w5.gcm.iv16", () -> { Cipher e = Cipher.getInstance("AES/GCM/NoPadding"); e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(120, IV16)); return sha(e.doFinal(RND, 0, 3333)); });
        c(o, "w5.gcm.tag", () -> { Cipher e = Cipher.getInstance("AES/GCM/NoPadding"); e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(128, IV12)); byte[] ct = e.doFinal(RND, 0, 1000); ct[999] ^= 4; Cipher d = Cipher.getInstance("AES/GCM/NoPadding"); d.init(Cipher.DECRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(128, IV12)); return d.doFinal(ct); });
        c(o, "w5.gcm.bb", () -> {
            Cipher e = Cipher.getInstance("AES/GCM/NoPadding"); e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY16, "AES"), new GCMParameterSpec(128, IV12));
            ByteBuffer src = ByteBuffer.allocateDirect(20000); src.put(RND, 0, 20000).flip(); ByteBuffer dst = ByteBuffer.allocateDirect(20100);
            int n = e.doFinal(src, dst); dst.flip(); byte[] r = new byte[dst.remaining()]; dst.get(r); return n + ":" + sha(r);
        });
        // non-AES block ciphers through the same feedback-mode intrinsics
        for (String tr : new String[] {"DESede/CBC/PKCS5Padding", "DESede/CTR/NoPadding", "DES/ECB/PKCS5Padding", "Blowfish/CBC/PKCS5Padding", "Blowfish/CTR/NoPadding", "RC2/ECB/PKCS5Padding", "DESede/ECB/NoPadding"}) {
            c(o, "w5.nonaes." + tr, () -> {
                String alg = tr.substring(0, tr.indexOf('/'));
                byte[] kb = Arrays.copyOfRange(RND, 300, 300 + (alg.equals("DES") ? 8 : alg.equals("DESede") ? 24 : 16));
                SecretKeySpec k = new SecretKeySpec(kb, alg);
                StringBuilder sb = new StringBuilder();
                for (int n : new int[] {0, 8, 13, 64, 1000}) {
                    if (tr.endsWith("ECB/NoPadding") && n % 8 != 0) continue;
                    Cipher e = Cipher.getInstance(tr);
                    if (tr.contains("ECB")) e.init(Cipher.ENCRYPT_MODE, k); else e.init(Cipher.ENCRYPT_MODE, k, new IvParameterSpec(Arrays.copyOfRange(RND, 0, 8)));
                    byte[] ct = e.doFinal(RND, 9, n);
                    Cipher d = Cipher.getInstance(tr);
                    if (tr.contains("ECB")) d.init(Cipher.DECRYPT_MODE, k); else d.init(Cipher.DECRYPT_MODE, k, new IvParameterSpec(Arrays.copyOfRange(RND, 0, 8)));
                    sb.append(sha(ct)).append(Arrays.equals(d.doFinal(ct), Arrays.copyOfRange(RND, 9, 9 + n)) ? "" : "!rt").append(',');
                }
                return sb.toString();
            });
        }
        // ChaCha20 (stream) and ChaCha20-Poly1305
        c(o, "w5.chacha20", () -> {
            StringBuilder sb = new StringBuilder();
            for (int n : SIZES) {
                Cipher e = Cipher.getInstance("ChaCha20");
                e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY32, "ChaCha20"), new javax.crypto.spec.ChaCha20ParameterSpec(Arrays.copyOfRange(RND, n, n + 12), 7));
                byte[] a = e.update(RND, 2, n / 2); byte[] b = e.doFinal(RND, 2 + n / 2, n - n / 2);
                sb.append(sha(concat(a, b))).append(',');
            }
            return sb.toString();
        });
        c(o, "w5.chacha20.limit", () -> {
            // block counter crossing 2^32: the 32-bit counter word wraps
            // inside a multi-block key-stream refill
            StringBuilder sb = new StringBuilder();
            for (int start : new int[] {-40, -17, -16, -3, -1}) {
                Cipher e = Cipher.getInstance("ChaCha20");
                e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY32, "ChaCha20"), new javax.crypto.spec.ChaCha20ParameterSpec(IV12, start));
                byte[] acc = new byte[0];
                try { for (int i = 0; i < 60; i++) acc = concat(acc, e.update(RND, 0, 64 + i % 3)); sb.append(sha(acc)); }
                catch (Exception ex) { sb.append(acc.length).append(':').append(ex.getClass().getSimpleName()).append(':').append(ex.getMessage()); }
                sb.append(',');
            }
            return sb.toString();
        });
        c(o, "w5.chachapoly", () -> {
            StringBuilder sb = new StringBuilder();
            for (int n : SIZES) {
                byte[] iv = Arrays.copyOfRange(RND, 3 * n % 1000, 3 * n % 1000 + 12);
                Cipher e = Cipher.getInstance("ChaCha20-Poly1305");
                e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(Arrays.copyOfRange(RND, 64, 96), "ChaCha20"), new IvParameterSpec(iv));
                e.updateAAD(RND, 5, n % 37);
                byte[] a = e.update(RND, 9, n / 3); byte[] b = e.doFinal(RND, 9 + n / 3, n - n / 3);
                byte[] ct = concat(a, b);
                Cipher d = Cipher.getInstance("ChaCha20-Poly1305");
                d.init(Cipher.DECRYPT_MODE, new SecretKeySpec(Arrays.copyOfRange(RND, 64, 96), "ChaCha20"), new IvParameterSpec(iv));
                d.updateAAD(RND, 5, n % 37);
                boolean ok = Arrays.equals(d.doFinal(ct), Arrays.copyOfRange(RND, 9, 9 + n));
                sb.append(sha(ct)).append(ok ? "" : "!rt").append(',');
            }
            return sb.toString();
        });
        c(o, "w5.chachapoly.tamper", () -> { Cipher e = Cipher.getInstance("ChaCha20-Poly1305"); e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY32, "ChaCha20"), new IvParameterSpec(IV12)); byte[] ct = e.doFinal(RND, 0, 500); ct[10] ^= 1; Cipher d = Cipher.getInstance("ChaCha20-Poly1305"); d.init(Cipher.DECRYPT_MODE, new SecretKeySpec(KEY32, "ChaCha20"), new IvParameterSpec(IV12)); return d.doFinal(ct); });
        c(o, "w5.chachapoly.bb", () -> { Cipher e = Cipher.getInstance("ChaCha20-Poly1305"); e.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(KEY32, "ChaCha20"), new IvParameterSpec(IV12)); ByteBuffer in = ByteBuffer.wrap(RND, 0, 9000); ByteBuffer out = ByteBuffer.allocate(9100); int n = e.doFinal(in, out); return n + ":" + sha(Arrays.copyOf(out.array(), n)); });
        c(o, "w5.aeswrap", () -> { Cipher w = Cipher.getInstance("AESWrapPad"); w.init(Cipher.WRAP_MODE, new SecretKeySpec(KEY32, "AES")); return w.wrap(new SecretKeySpec(Arrays.copyOf(RND, 21), "RAW")); });
        c(o, "w5.gmac.pbe", () -> { Cipher p = Cipher.getInstance("PBEWithHmacSHA256AndAES_128"); p.init(Cipher.ENCRYPT_MODE, SecretKeyFactory.getInstance("PBEWithHmacSHA256AndAES_128").generateSecret(new PBEKeySpec("pw".toCharArray())), new PBEParameterSpec(KEY16, 1000, new IvParameterSpec(IV16))); return sha(p.doFinal(RND, 0, 777)); });
    }

    static byte[] concat(byte[]... parts) {
        int n = 0;
        for (byte[] p : parts) n += p == null ? 0 : p.length;
        byte[] r = new byte[n];
        int o = 0;
        for (byte[] p : parts) if (p != null) { System.arraycopy(p, 0, r, o, p.length); o += p.length; }
        return r;
    }

    public static void main(String[] args) throws Exception {
        long t0 = System.nanoTime();
        List<String> first = run();
        long t1 = System.nanoTime();
        for (String s : first) System.out.println(s);
        // 1 MiB SHA-256, once, as the brief's hot-path witness.
        byte[] big = new byte[1 << 20];
        for (int i = 0; i < big.length; i++) big[i] = (byte) (i ^ (i >>> 9));
        System.out.println("md.sha256.1MiB = " + hex(MessageDigest.getInstance("SHA-256").digest(big)));
        int drift = 0;
        for (int it = 0; it < ITERS; it++) {
            List<String> again = run();
            for (int i = 0; i < first.size(); i++) {
                String a = first.get(i), b = i < again.size() ? again.get(i) : "<missing>";
                // Randomized outputs (EC key gen, RSA key gen, ECDSA) print only
                // structural facts, so every line is expected to be identical.
                if (!a.equals(b)) {
                    drift++;
                    if (drift <= 5) System.out.println("DRIFT it=" + it + " " + b);
                }
            }
        }
        System.out.println("lines = " + first.size());
        System.out.println("drift = " + drift + " over " + ITERS + " iterations");
        System.err.println("first-run ms = " + (t1 - t0) / 1_000_000);
    }
}
