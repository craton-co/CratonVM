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

    /**
     * Advertised versus served: every name the provider chain publishes must be
     * one the engine will hand over, and no name it refuses may be published.
     *
     * The three records this closes -- W4-3-security-getalgorithms-short-list.md,
     * W7-29-jca-advertise-implement-gaps.md, W7-63-jca-advertise-vs-serve.md --
     * had their fixes ratcheted only by Rust unit tests over the seed map and by
     * a probe under probes/, which regression-suite/run.sh never runs. Every
     * assertion here fails on the pre-fix behaviour:
     *
     *   MD2                 advertised by SUN and refused by getInstance
     *   SHAKE128-256/256-512 neither implemented nor advertised
     *   SHAKE128 / SHAKE256 the ALIAS half -- resolvable on HotSpot, refused
     *                       here even after the primaries landed, because
     *                       getInstance's only gate is the digest engine's own
     *                       name table and nothing on that path reads the
     *                       provider chain's alias rows
     *   Signature           getInstance accepted EVERY string and deferred the
     *                       failure to sign()/verify() as the wrong exception
     *   getAlgorithms       returned a plain mutable HashSet
     *
     * Every vector is HotSpot 25's own answer, so this section holds on the
     * oracle as well as on both CratonVM modes. Two knowing divergences are
     * deliberately NOT asserted here because they would fail on HotSpot: SUN's
     * KeyFactory no longer advertises the ML-DSA umbrella, and SunJCE's no
     * longer advertises ML-KEM. The loops below assert the invariant those
     * removals restore -- advertised implies serviceable -- which is true on
     * both VMs by different routes.
     */
    static void advertisedVersusServed() throws Exception {
        // MD2, RFC 1319. Advertised by SUN for three waves while getInstance
        // refused it; implemented rather than de-advertised, because SunRsaSign
        // and SunMSCAPI both advertise MD2withRSA, which resolves MD2
        // internally. These are RFC 1319's own vectors, re-measured on HotSpot.
        MessageDigest md2 = MessageDigest.getInstance("MD2");
        check(md2.getDigestLength() == 16, "MD2 digest length: " + md2.getDigestLength());
        String md2Empty = hex(md2.digest(new byte[0]));
        check(md2Empty.equals("8350e5a3e24c153df2275c9f80692773"), "MD2 of empty: " + md2Empty);
        String md2Abc = hex(md2.digest("abc".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(md2Abc.equals("da853b0d3f88d99b30283a69e6ded6bb"), "MD2(abc): " + md2Abc);

        // The two SHAKE XOFs read out to the fixed length their JDK name names.
        MessageDigest shake128 = MessageDigest.getInstance("SHAKE128-256");
        check(shake128.getDigestLength() == 32,
                "SHAKE128-256 length: " + shake128.getDigestLength());
        String s128 = hex(shake128.digest("abc".getBytes(
                java.nio.charset.StandardCharsets.UTF_8)));
        check(s128.equals("5881092dd818bf5cf8a3ddb793fbcba74097d5c526a6d35f97b83351940f2cc8"),
                "SHAKE128-256(abc): " + s128);
        MessageDigest shake256 = MessageDigest.getInstance("SHAKE256-512");
        check(shake256.getDigestLength() == 64,
                "SHAKE256-512 length: " + shake256.getDigestLength());
        String s256 = hex(shake256.digest("abc".getBytes(
                java.nio.charset.StandardCharsets.UTF_8)));
        check(s256.equals("483366601360a8771c6863080cc4114d8db44530f8f1e1ee4f94ea37e78b5739"
                        + "d5a15bef186a5386c75744c0527e1faa9f8726e462a12a4feb06bd8801e751e4"),
                "SHAKE256-512(abc): " + s256);

        // The alias half. Alg.Alias.MessageDigest.SHAKE128 = SHAKE128-256 on
        // HotSpot: the bare spelling resolves and produces byte-identical
        // output, while getAlgorithms lists only the hyphenated primary.
        String alias128 = hex(MessageDigest.getInstance("SHAKE128").digest(
                "abc".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(alias128.equals(s128), "SHAKE128 is an alias of SHAKE128-256: " + alias128);
        String alias256 = hex(MessageDigest.getInstance("SHAKE256").digest(
                "abc".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(alias256.equals(s256), "SHAKE256 is an alias of SHAKE256-512: " + alias256);

        java.util.Set<String> digestNames = Security.getAlgorithms("MessageDigest");
        check(digestNames.contains("MD2"), "MessageDigest algorithms must include MD2");
        check(digestNames.contains("SHAKE128-256") && digestNames.contains("SHAKE256-512"),
                "MessageDigest algorithms must include both SHAKE primaries: " + digestNames);
        check(!digestNames.contains("SHAKE128") && !digestNames.contains("SHAKE256"),
                "an ALIAS must not be advertised as an algorithm: " + digestNames);

        // Advertised implies serviceable, in both engines whose lists drifted.
        // One check each rather than one per name, so the count does not move
        // with the size of the provider's list.
        List<String> refusedDigests = new ArrayList<>();
        for (String algo : digestNames) {
            try {
                MessageDigest.getInstance(algo);
            } catch (NoSuchAlgorithmException refused) {
                refusedDigests.add(algo);
            }
        }
        check(refusedDigests.isEmpty(),
                "every advertised MessageDigest must be serviceable, refused: " + refusedDigests);

        List<String> refusedFactories = new ArrayList<>();
        for (String algo : Security.getAlgorithms("KeyFactory")) {
            try {
                KeyFactory.getInstance(algo);
            } catch (NoSuchAlgorithmException refused) {
                refusedFactories.add(algo);
            }
        }
        check(refusedFactories.isEmpty(),
                "every advertised KeyFactory must be serviceable, refused: " + refusedFactories);

        // Signature.getInstance used to answer EVERY string with an object
        // whose getAlgorithm() was "Unknown", so a caller probing with
        // catch (NoSuchAlgorithmException) concluded the algorithm was present
        // and met a SignatureException much later instead.
        for (String bogus : new String[] { "NO-SUCH-SIG", "AES", "HmacSHA256", "" }) {
            boolean threw = false;
            try {
                Signature.getInstance(bogus);
            } catch (NoSuchAlgorithmException expected) {
                threw = true;
            }
            check(threw, "Signature.getInstance(\"" + bogus
                    + "\") must raise NoSuchAlgorithmException");
        }

        // Last, because it mutates: the returned set is a view of platform
        // state, not the caller's to edit. HotSpot answers
        // Collections$UnmodifiableSet on every path including the empty ones.
        boolean unmodifiable = false;
        try {
            digestNames.add("CRATONVM-NOT-AN-ALGORITHM");
        } catch (UnsupportedOperationException expected) {
            unmodifiable = true;
        }
        check(unmodifiable, "Security.getAlgorithms must return an unmodifiable set");

        System.out.println("CK RJdkSecurity md2=" + md2Abc + " shake128=" + s128.substring(0, 16)
                + " digests=" + digestNames.size());
    }

    /**
     * `javax.net.ssl.trustStore` is how an application says "trust THIS and
     * nothing else", and `TrustManagerFactory.init(null)` is what has to obey
     * it: JSSE's default trust store is the platform roots only while the
     * property is unset, and the property REPLACES them rather than adding to
     * them.
     *
     * CratonVM ignored it, which failed in the widening direction — an
     * application that pinned its trust to one CA was given the whole public
     * root set (122 anchors where HotSpot reported 1) and still rejected the
     * one certificate it had asked to trust.
     *
     * Nothing here prints a certificate, a subject or a platform anchor COUNT:
     * the two VMs legitimately ship different root sets (118 vs 122), and the
     * borrowed anchor below is simply "the first RSA root this VM has", which
     * also differs. What is diffed is the shape the property produces — one
     * anchor, its own certificate accepted, an unrelated one refused.
     */
    static void defaultTrustStoreProperty() throws Exception {
        String saved = System.getProperty("javax.net.ssl.trustStore");
        java.io.File f = java.io.File.createTempFile("rjdksec-trust", ".p12");
        try {
            java.security.cert.X509Certificate mine = null;
            java.security.cert.X509Certificate other = null;
            for (java.security.cert.X509Certificate c : platformAnchors()) {
                if (!"RSA".equals(c.getPublicKey().getAlgorithm())) {
                    continue;
                }
                if (mine == null) {
                    mine = c;
                } else if (!c.getSubjectX500Principal().equals(mine.getSubjectX500Principal())) {
                    other = c;
                    break;
                }
            }
            if (mine == null || other == null) {
                // Reported, never silently skipped: a run with no usable
                // platform anchors must not read as a pass of this stage.
                System.out.println("CK RJdkSecurity trustStoreProp=SKIPPED-no-rsa-anchors");
                checks++;
                return;
            }
            java.security.KeyStore ks =
                    java.security.KeyStore.getInstance(java.security.KeyStore.getDefaultType());
            ks.load(null, null);
            ks.setCertificateEntry("only", mine);
            try (java.io.OutputStream o = new java.io.FileOutputStream(f)) {
                ks.store(o, "changeit".toCharArray());
            }
            System.setProperty("javax.net.ssl.trustStore", f.getAbsolutePath());
            System.setProperty("javax.net.ssl.trustStorePassword", "changeit");

            javax.net.ssl.TrustManagerFactory tmf = javax.net.ssl.TrustManagerFactory
                    .getInstance(javax.net.ssl.TrustManagerFactory.getDefaultAlgorithm());
            tmf.init((java.security.KeyStore) null);
            javax.net.ssl.X509TrustManager x = null;
            for (javax.net.ssl.TrustManager tm : tmf.getTrustManagers()) {
                if (tm instanceof javax.net.ssl.X509TrustManager) {
                    x = (javax.net.ssl.X509TrustManager) tm;
                    break;
                }
            }
            if (x == null) {
                System.out.println("CK RJdkSecurity trustStoreProp=NO-X509-MANAGER");
                checks++;
                return;
            }
            java.security.cert.X509Certificate[] issuers = x.getAcceptedIssuers();
            System.out.println("CK RJdkSecurity trustStorePropAnchors="
                    + (issuers == null ? -1 : issuers.length) + " (expect 1)");
            checks++;
            System.out.println("CK RJdkSecurity trustStorePropOwnCert=" + verdict(x, mine));
            checks++;
            System.out.println("CK RJdkSecurity trustStorePropOtherCert=" + verdict(x, other));
            checks++;
        } finally {
            if (saved == null) {
                System.clearProperty("javax.net.ssl.trustStore");
                System.clearProperty("javax.net.ssl.trustStorePassword");
            } else {
                System.setProperty("javax.net.ssl.trustStore", saved);
            }
            f.delete();
        }
    }

    static java.util.List<java.security.cert.X509Certificate> platformAnchors() throws Exception {
        javax.net.ssl.TrustManagerFactory tmf = javax.net.ssl.TrustManagerFactory
                .getInstance(javax.net.ssl.TrustManagerFactory.getDefaultAlgorithm());
        tmf.init((java.security.KeyStore) null);
        for (javax.net.ssl.TrustManager tm : tmf.getTrustManagers()) {
            if (tm instanceof javax.net.ssl.X509TrustManager) {
                java.security.cert.X509Certificate[] a =
                        ((javax.net.ssl.X509TrustManager) tm).getAcceptedIssuers();
                return a == null ? Collections.emptyList() : Arrays.asList(a);
            }
        }
        return Collections.emptyList();
    }

    static String verdict(javax.net.ssl.X509TrustManager x,
            java.security.cert.X509Certificate cert) {
        try {
            x.checkServerTrusted(new java.security.cert.X509Certificate[] { cert }, "RSA");
            return "ACCEPTED";
        } catch (Exception e) {
            return "REJECTED";
        }
    }

    public static void main(String[] args) throws Exception {
        digests();
        secureRandoms();
        signatures();
        tls();
        defaultTrustStoreProperty();
        providers();
        advertisedVersusServed();
        System.out.println("CK RJdkSecurity checks=" + checks);
        System.out.println("PASS RJdkSecurity (" + checks + " checks)");
    }
}
