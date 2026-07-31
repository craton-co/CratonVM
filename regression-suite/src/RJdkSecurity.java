import java.math.BigInteger;
import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.security.SecureRandom;
import java.security.Security;
import java.security.Signature;
import java.security.SignatureException;
import java.security.spec.PKCS8EncodedKeySpec;
import java.security.spec.X509EncodedKeySpec;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Locale;
import javax.crypto.Cipher;
import javax.crypto.Mac;
import javax.crypto.SecretKey;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.PBEKeySpec;
import javax.crypto.spec.SecretKeySpec;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLParameters;

/**
 * JDK-only corpus: security -- {@code SecureRandom}, message digests,
 * signatures, TLS where supported.
 *
 * Complements RCrypto (which pins the SHA-256/HMAC/AES-GCM/RSA KATs). This
 * vector covers PROVIDER MACHINERY: algorithm lookup, service resolution,
 * failure modes for absent algorithms, key encoding round-trips and the
 * TLS/SSLEngine surface.
 *
 * TLS: a real loopback HANDSHAKE needs a key store, which cannot be generated
 * portably without internal APIs, so this vector goes as far as
 * {@code SSLContext} + {@code SSLEngine} construction and parameter shape --
 * see regression-suite/jdk-only-coverage.txt for that limitation.
 *
 * Determinism: SecureRandom output is asserted only through invariants (length,
 * "two draws differ"), NEVER printed. Signatures over RSA use randomised PKCS#1
 * padding, so only verify() booleans are printed, never the signature bytes.
 */
public class RJdkSecurity {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder(b.length * 2);
        for (byte x : b) {
            sb.append(Character.forDigit((x >> 4) & 0xf, 16));
            sb.append(Character.forDigit(x & 0xf, 16));
        }
        return sb.toString();
    }

    static void digests() throws Exception {
        // Known-answer tests: these are fixed by the standards, so they are safe
        // to print and are the strongest possible cross-VM check.
        MessageDigest sha256 = MessageDigest.getInstance("SHA-256");
        String empty = hex(sha256.digest(new byte[0]));
        check(empty.equals("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
                "SHA-256 of the empty input: " + empty);
        sha256.reset();
        String abc = hex(sha256.digest("abc".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(abc.equals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
                "SHA-256(abc): " + abc);
        check(sha256.getDigestLength() == 32, "SHA-256 length");
        check(sha256.getAlgorithm().equals("SHA-256"), "algorithm name");

        // Incremental update must equal the one-shot digest.
        MessageDigest inc = MessageDigest.getInstance("SHA-256");
        inc.update((byte) 'a');
        inc.update("bc".getBytes(java.nio.charset.StandardCharsets.UTF_8));
        check(hex(inc.digest()).equals(abc), "incremental digest must equal one-shot");

        // clone() must fork the running state.
        MessageDigest base = MessageDigest.getInstance("SHA-256");
        base.update((byte) 'a');
        MessageDigest forked = (MessageDigest) base.clone();
        forked.update("bc".getBytes(java.nio.charset.StandardCharsets.UTF_8));
        check(hex(forked.digest()).equals(abc), "cloned digest state");

        String sha512 = hex(MessageDigest.getInstance("SHA-512").digest(new byte[0]));
        check(sha512.length() == 128, "SHA-512 hex length");
        check(sha512.startsWith("cf83e1357eefb8bd"), "SHA-512 of empty: " + sha512.substring(0, 16));

        // MessageDigest.isEqual is the constant-time comparison.
        check(MessageDigest.isEqual(new byte[] { 1, 2 }, new byte[] { 1, 2 }), "isEqual true");
        check(!MessageDigest.isEqual(new byte[] { 1, 2 }, new byte[] { 1, 3 }), "isEqual false");

        // An unknown algorithm must fail, never be fabricated.
        boolean threw = false;
        try {
            MessageDigest.getInstance("SHA-NO-SUCH-ALG");
        } catch (NoSuchAlgorithmException expected) {
            threw = true;
        }
        check(threw, "an unknown digest must raise NoSuchAlgorithmException");

        // HMAC KAT (RFC 4231 test case 2).
        Mac hmac = Mac.getInstance("HmacSHA256");
        hmac.init(new SecretKeySpec("Jefe".getBytes(java.nio.charset.StandardCharsets.UTF_8),
                "HmacSHA256"));
        String mac = hex(hmac.doFinal(
                "what do ya want for nothing?".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(mac.equals("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"),
                "HmacSHA256 RFC 4231 #2: " + mac);
        System.out.println("CK RJdkSecurity sha256abc=" + abc);
        System.out.println("CK RJdkSecurity hmac=" + mac);
    }

    static void secureRandoms() throws Exception {
        SecureRandom sr = new SecureRandom();
        byte[] a = new byte[32];
        byte[] b = new byte[32];
        sr.nextBytes(a);
        sr.nextBytes(b);
        check(!Arrays.equals(a, b), "two SecureRandom draws must differ");
        check(a.length == 32, "nextBytes fills the array");
        boolean allZero = true;
        for (byte x : a) {
            allZero &= x == 0;
        }
        check(!allZero, "SecureRandom must not return all zeroes");
        check(sr.getAlgorithm() != null && !sr.getAlgorithm().isEmpty(), "algorithm name");
        check(sr.getProvider() != null, "provider");
        long l1 = sr.nextLong();
        long l2 = sr.nextLong();
        check(l1 != l2, "two nextLong draws must differ");
        check(SecureRandom.getSeed(8).length == 8, "getSeed length");
        sr.setSeed(12345L);      // must be additive, never reset to a fixed stream
        byte[] c = new byte[32];
        sr.nextBytes(c);
        check(!Arrays.equals(a, c), "setSeed must not replay an earlier stream");
        check(SecureRandom.getInstanceStrong() != null, "getInstanceStrong");

        // A named PRNG algorithm, when the platform provides one.
        String named = "none";
        try {
            SecureRandom drbg = SecureRandom.getInstance("SHA1PRNG");
            byte[] d = new byte[16];
            drbg.nextBytes(d);
            named = drbg.getAlgorithm();
        } catch (NoSuchAlgorithmException e) {
            named = "absent";
        }
        check(named.equals("SHA1PRNG") || named.equals("absent"), "named PRNG: " + named);

        boolean threw = false;
        try {
            SecureRandom.getInstance("NO-SUCH-PRNG");
        } catch (NoSuchAlgorithmException expected) {
            threw = true;
        }
        check(threw, "an unknown PRNG must raise NoSuchAlgorithmException");
        System.out.println("CK RJdkSecurity prng=" + named + " distinct=true");
    }

    static void signatures() throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        check(kp.getPublic().getAlgorithm().equals("RSA"), "public key algorithm");
        check(kp.getPublic().getFormat().equals("X.509"), "public key format");
        check(kp.getPrivate().getFormat().equals("PKCS#8"), "private key format");

        byte[] msg = "sign-me".getBytes(java.nio.charset.StandardCharsets.UTF_8);
        Signature signer = Signature.getInstance("SHA256withRSA");
        signer.initSign(kp.getPrivate());
        signer.update(msg);
        byte[] sig = signer.sign();
        check(sig.length == 256, "RSA-2048 signature length: " + sig.length);

        Signature verifier = Signature.getInstance("SHA256withRSA");
        verifier.initVerify(kp.getPublic());
        verifier.update(msg);
        check(verifier.verify(sig), "a valid signature must verify");

        verifier.initVerify(kp.getPublic());
        verifier.update("sign-me-not".getBytes(java.nio.charset.StandardCharsets.UTF_8));
        check(!verifier.verify(sig), "a tampered message must NOT verify");

        // A corrupted signature must fail, one way or the other, but never verify.
        byte[] bad = sig.clone();
        bad[0] ^= 0x55;
        verifier.initVerify(kp.getPublic());
        verifier.update(msg);
        boolean verified;
        try {
            verified = verifier.verify(bad);
        } catch (SignatureException e) {
            verified = false;
        }
        check(!verified, "a corrupted signature must not verify");

        // Key encoding round-trip through KeyFactory.
        KeyFactory kf = KeyFactory.getInstance("RSA");
        java.security.PublicKey pub2 = kf.generatePublic(
                new X509EncodedKeySpec(kp.getPublic().getEncoded()));
        java.security.PrivateKey priv2 = kf.generatePrivate(
                new PKCS8EncodedKeySpec(kp.getPrivate().getEncoded()));
        check(pub2.equals(kp.getPublic()), "public key encode/decode round-trip");
        check(Arrays.equals(priv2.getEncoded(), kp.getPrivate().getEncoded()),
                "private key encode/decode round-trip");

        // The re-decoded key must still verify a signature made with the original.
        Signature v2 = Signature.getInstance("SHA256withRSA");
        v2.initVerify(pub2);
        v2.update(msg);
        check(v2.verify(sig), "a round-tripped public key must still verify");

        // AES-GCM with a fixed key and IV is a KAT and is safe to print.
        SecretKey key = new SecretKeySpec(new byte[16], "AES");
        byte[] iv = new byte[12];
        Cipher enc = Cipher.getInstance("AES/GCM/NoPadding");
        enc.init(Cipher.ENCRYPT_MODE, key, new GCMParameterSpec(128, iv));
        byte[] ct = enc.doFinal(new byte[16]);
        check(hex(ct).equals("0388dace60b6a392f328c2b971b2fe78"
                + "ab6e47d42cec13bdf53a67b21257bddf"), "AES-GCM KAT: " + hex(ct));
        Cipher dec = Cipher.getInstance("AES/GCM/NoPadding");
        dec.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, iv));
        check(Arrays.equals(dec.doFinal(ct), new byte[16]), "AES-GCM round-trip");

        // PBKDF2 is a KAT too (RFC 6070-style, 1 iteration).
        SecretKeyFactory skf = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256");
        byte[] dk = skf.generateSecret(new PBEKeySpec("password".toCharArray(),
                "salt".getBytes(java.nio.charset.StandardCharsets.UTF_8), 1, 256)).getEncoded();
        check(dk.length == 32, "PBKDF2 output length");
        check(hex(dk).startsWith("120fb6cffcf8b32c"), "PBKDF2 KAT: " + hex(dk).substring(0, 16));

        // BigInteger modular arithmetic underpins all of the above.
        BigInteger p = new BigInteger("170141183460469231731687303715884105727");
        check(p.isProbablePrime(40), "2^127-1 must be prime");
        check(BigInteger.valueOf(3).modPow(BigInteger.valueOf(100), p).signum() > 0, "modPow");
        System.out.println("CK RJdkSecurity gcm=" + hex(ct)
                + " pbkdf2=" + hex(dk).substring(0, 16) + " verify=true");
    }

    static void tls() throws Exception {
        // The default context must exist and name a real protocol.
        SSLContext def = SSLContext.getDefault();
        check(def != null, "SSLContext.getDefault");
        check(def.getProtocol() != null, "default protocol");

        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(null, null, null);
        check(ctx.getProtocol().equals("TLS"), "TLS context protocol");
        check(ctx.getSocketFactory() != null, "socket factory");
        check(ctx.getServerSocketFactory() != null, "server socket factory");

        SSLEngine engine = ctx.createSSLEngine("localhost", 443);
        check(engine != null, "createSSLEngine");
        engine.setUseClientMode(true);
        check(engine.getUseClientMode(), "client mode");
        check(engine.getPeerHost().equals("localhost"), "peer host");
        check(engine.getPeerPort() == 443, "peer port");
        check(engine.getSupportedProtocols().length > 0, "supported protocols");
        check(engine.getSupportedCipherSuites().length > 0, "supported cipher suites");
        check(engine.getEnabledCipherSuites().length > 0, "enabled cipher suites");

        // Protocol NAMES are stable strings; the SET varies by JDK, so only
        // membership of the modern protocol is asserted and printed.
        List<String> protos = new ArrayList<>(Arrays.asList(engine.getSupportedProtocols()));
        Collections.sort(protos);
        check(protos.contains("TLSv1.2") || protos.contains("TLSv1.3"),
                "a modern TLS protocol must be supported: " + protos);
        String modern = protos.contains("TLSv1.3") ? "TLSv1.3" : "TLSv1.2";

        SSLParameters params = engine.getSSLParameters();
        check(params != null, "SSL parameters");
        params.setProtocols(new String[] { modern });
        engine.setSSLParameters(params);
        check(Arrays.asList(engine.getEnabledProtocols()).contains(modern),
                "enabled protocol after set");

        check(engine.getSession() != null, "pre-handshake session");
        check(engine.getHandshakeStatus() != null, "handshake status");

        boolean threw = false;
        try {
            SSLContext.getInstance("NO-SUCH-TLS");
        } catch (NoSuchAlgorithmException expected) {
            threw = true;
        }
        check(threw, "an unknown SSL protocol must raise NoSuchAlgorithmException");
        System.out.println("CK RJdkSecurity tls=" + modern
                + " engine=" + (engine.getUseClientMode() ? "client" : "server"));
    }

    static void providers() {
        // At least the SUN/SunJCE providers must be present, and the service
        // lookup must be case-insensitive per the JCA spec.
        List<String> names = new ArrayList<>();
        for (java.security.Provider p : Security.getProviders()) {
            names.add(p.getName());
        }
        check(names.contains("SUN"), "the SUN provider must be installed: " + names);
        check(Security.getProvider("SUN") != null, "getProvider(SUN)");
        check(Security.getProvider("cratonvm-no-such-provider") == null,
                "an unknown provider must be null, not fabricated");
        check(Security.getAlgorithms("MessageDigest").contains("SHA-256")
                || Security.getAlgorithms("MessageDigest").contains("SHA-256".toUpperCase(
                        Locale.ROOT)), "MessageDigest algorithms must include SHA-256");
        System.out.println("CK RJdkSecurity providerSun=true");
    }

    public static void main(String[] args) throws Exception {
        digests();
        secureRandoms();
        signatures();
        tls();
        providers();
        System.out.println("CK RJdkSecurity checks=" + checks);
        System.out.println("PASS RJdkSecurity (" + checks + " checks)");
    }
}
