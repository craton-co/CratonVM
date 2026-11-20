import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.Signature;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.util.concurrent.atomic.AtomicLong;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLEngineResult;
import javax.net.ssl.TrustManager;
import javax.net.ssl.X509TrustManager;

/**
 * In-memory {@code SSLEngine} handshakes against a custom {@code X509TrustManager}
 * on BOTH sides, while peer threads allocate hard, so a moving collection lands
 * inside the native that consults the TrustManagers.
 *
 * <p>Under {@code --compatible} with G1 a handshake reaches
 * {@code engine_consult_trust_managers}, which used to pin each TrustManager
 * AFTER it had allocated the certificate chain, the X509 mirrors and the
 * authType string. The pins named addresses a young collection had already
 * moved, so the first {@code invoke_virtual} resolved the receiver's class as
 * {@code ClassId(0)} and raised {@code NoSuchMethodError: 'void
 * java.lang.Object.checkClientTrusted(...)'} (or {@code checkServerTrusted}) -
 * a {@code LinkageError} the engine does not turn into an alert, which is how
 * netty's {@code ParameterizedSslHandlerTest} came to wait forever on a promise
 * nothing would complete. Before the fix a launch hit it within the first 25-80
 * handshakes, every time; after it, thousands of handshakes run clean.
 *
 * <p>Everything printed is deterministic and equal to HotSpot's.
 */
public class TlsTrustManagerChurnProbe {

    static final int WORKERS = 2;
    static final int PRESSURE = 2;
    static final int PER_WORKER = Integer.getInteger("handshakes", 400);

    static final AtomicLong COMPLETED = new AtomicLong();
    static final AtomicLong FAILED = new AtomicLong();
    static final AtomicLong LINKAGE = new AtomicLong();
    static final AtomicLong ASKED = new AtomicLong();
    static volatile Object sink;

    /** Accepts everything, and counts that it was asked: the consultation is the point. */
    static final class AcceptAll implements X509TrustManager {
        @Override public void checkClientTrusted(X509Certificate[] c, String s) { ASKED.incrementAndGet(); }
        @Override public void checkServerTrusted(X509Certificate[] c, String s) { ASKED.incrementAndGet(); }
        @Override public X509Certificate[] getAcceptedIssuers() { return new X509Certificate[0]; }
    }

    // ---- tiny DER builder: the same self-signed certificate SbrW4Ssl assembles ------------------
    static byte[] tlv(int tag, byte[] body) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        o.write(tag);
        int n = body.length;
        if (n < 128) o.write(n);
        else if (n < 256) { o.write(0x81); o.write(n); }
        else { o.write(0x82); o.write((n >> 8) & 0xff); o.write(n & 0xff); }
        o.write(body, 0, n);
        return o.toByteArray();
    }
    static byte[] cat(byte[]... ps) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        for (byte[] p : ps) o.write(p, 0, p.length);
        return o.toByteArray();
    }
    static byte[] seq(byte[]... ps) { return tlv(0x30, cat(ps)); }
    static byte[] oid(int... e) {
        byte[] b = new byte[e.length];
        for (int i = 0; i < e.length; i++) b[i] = (byte) e[i];
        return tlv(0x06, b);
    }
    static byte[] ascii(int tag, String s) { return tlv(tag, s.getBytes(StandardCharsets.US_ASCII)); }

    static X509Certificate selfSigned(PrivateKey k, PublicKey p) throws Exception {
        byte[] alg = seq(oid(0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b), new byte[] {0x05, 0x00});
        byte[] name = seq(
                tlv(0x31, seq(oid(0x55, 0x04, 0x06), ascii(0x13, "US"))),
                tlv(0x31, seq(oid(0x55, 0x04, 0x0a), ascii(0x13, "Craton"))),
                tlv(0x31, seq(oid(0x55, 0x04, 0x03), ascii(0x13, "localhost"))));
        byte[] validity = seq(ascii(0x17, "200101000000Z"), ascii(0x17, "491231235959Z"));
        byte[] san = seq(ascii(0x82, "localhost"));
        byte[] exts = tlv(0xa3, seq(seq(oid(0x55, 0x1d, 0x11), tlv(0x04, san))));
        byte[] tbs = seq(tlv(0xa0, new byte[] {0x02, 0x01, 0x02}), new byte[] {0x02, 0x01, 0x01},
                alg, name, validity, name, p.getEncoded(), exts);
        Signature s = Signature.getInstance("SHA256withRSA");
        s.initSign(k);
        s.update(tbs);
        byte[] der = seq(tbs, alg, tlv(0x03, cat(new byte[] {0}, s.sign())));
        return (X509Certificate) CertificateFactory.getInstance("X.509")
                .generateCertificate(new ByteArrayInputStream(der));
    }

    static SSLContext context(KeyStore ks, char[] pw) throws Exception {
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, pw);
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(kmf.getKeyManagers(), new TrustManager[] {new AcceptAll()}, null);
        return ctx;
    }

    public static void main(String[] args) throws Exception {
        KeyPairGenerator g = KeyPairGenerator.getInstance("RSA");
        g.initialize(2048);
        KeyPair kp = g.generateKeyPair();
        X509Certificate cert = selfSigned(kp.getPrivate(), kp.getPublic());
        char[] pw = "churn".toCharArray();
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, null);
        ks.setKeyEntry("k", kp.getPrivate(), pw, new java.security.cert.Certificate[] {cert});
        final SSLContext serverCtx = context(ks, pw);
        final SSLContext clientCtx = context(ks, pw);

        for (int i = 0; i < PRESSURE; i++) {
            Thread t = new Thread(() -> {
                Object[] ring = new Object[256];
                int k = 0;
                while (true) {
                    ring[k++ & 255] = new byte[8192];
                }
            }, "gc-pressure-" + i);
            t.setDaemon(true);
            t.start();
        }

        Thread[] ts = new Thread[WORKERS];
        for (int w = 0; w < WORKERS; w++) {
            ts[w] = new Thread(() -> {
                for (int n = 0; n < PER_WORKER; n++) {
                    try {
                        if (handshake(serverCtx, clientCtx)) {
                            COMPLETED.incrementAndGet();
                        } else {
                            FAILED.incrementAndGet();
                        }
                    } catch (LinkageError e) {
                        LINKAGE.incrementAndGet();
                        System.err.println("LINKAGE " + e);
                    } catch (Throwable e) {
                        FAILED.incrementAndGet();
                        System.err.println("FAILED " + e);
                    }
                }
            }, "tm-churn-" + w);
            ts[w].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long expected = (long) WORKERS * PER_WORKER;
        // Explicit LF, not println: the expected file is LF on every platform.
        char nl = (char) 10;
        System.out.print("handshakes completed: " + (COMPLETED.get() == expected) + nl);
        System.out.print("failed handshakes: " + FAILED.get() + nl);
        System.out.print("linkage errors: " + LINKAGE.get() + nl);
        // Both sides consult their TrustManager once per handshake.
        System.out.print("trust managers consulted for every handshake: " + (ASKED.get() >= 2 * expected) + nl);
        System.out.flush();
        System.exit(0);
    }

    static boolean handshake(SSLContext serverCtx, SSLContext clientCtx) throws Exception {
        SSLEngine server = serverCtx.createSSLEngine();
        SSLEngine client = clientCtx.createSSLEngine("localhost", 443);
        server.setUseClientMode(false);
        server.setNeedClientAuth(true);
        client.setUseClientMode(true);
        server.beginHandshake();
        client.beginHandshake();
        ByteBuffer c2s = ByteBuffer.allocate(40000);
        ByteBuffer s2c = ByteBuffer.allocate(40000);
        ByteBuffer app = ByteBuffer.allocate(40000);
        ByteBuffer empty = ByteBuffer.allocate(0);
        for (int i = 0; i < 400; i++) {
            boolean a = step(client, empty, c2s, s2c, app);
            boolean b = step(server, empty, s2c, c2s, app);
            sink = new byte[512];
            if (!a && !b) {
                break;
            }
        }
        return client.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING
                && server.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING;
    }

    /** Advance one engine as far as it can go without input it does not have. */
    static boolean step(SSLEngine e, ByteBuffer empty, ByteBuffer out, ByteBuffer in, ByteBuffer app)
            throws Exception {
        boolean did = false;
        for (int k = 0; k < 16; k++) {
            switch (e.getHandshakeStatus()) {
                case NEED_TASK: {
                    Runnable r;
                    while ((r = e.getDelegatedTask()) != null) {
                        r.run();
                    }
                    did = true;
                    break;
                }
                case NEED_WRAP:
                    e.wrap(empty, out);
                    did = true;
                    break;
                case NEED_UNWRAP_AGAIN:
                    e.unwrap(empty, app);
                    app.clear();
                    did = true;
                    break;
                case NEED_UNWRAP: {
                    in.flip();
                    if (!in.hasRemaining()) {
                        in.compact();
                        return did;
                    }
                    SSLEngineResult r = e.unwrap(in, app);
                    in.compact();
                    app.clear();
                    did = true;
                    if (r.getStatus() == SSLEngineResult.Status.BUFFER_UNDERFLOW) {
                        return did;
                    }
                    break;
                }
                default:
                    return did;
            }
        }
        return did;
    }
}
