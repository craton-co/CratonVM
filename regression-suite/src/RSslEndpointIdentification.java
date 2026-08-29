import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SocketChannel;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.Signature;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLEngineResult;
import javax.net.ssl.SSLException;
import javax.net.ssl.SSLParameters;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManagerFactory;

/**
 * {@code SSLEngine} endpoint identification: {@code CVE-2018-8034}, the shape Tomcat's
 * {@code WsWebSocketContainer.createSSLEngine} exists to close.
 *
 * <p>A client that does
 *
 * <pre>{@code
 * SSLEngine engine = sslContext.createSSLEngine(host, port);
 * SSLParameters p = engine.getSSLParameters();
 * p.setEndpointIdentificationAlgorithm("HTTPS");
 * engine.setSSLParameters(p);
 * }</pre>
 *
 * must refuse a handshake with a server certificate that does not match {@code host} -- RFC 2818
 * / RFC 6125 identity checking, run automatically during the handshake once the algorithm is set,
 * independent of whatever {@code TrustManager} is installed (an accepting {@code TrustManager}
 * does not switch this check off; see
 * {@code fixed-bugs/testsecurity2018-endpoint-identification-never-enforced-FIXED.md} in the internal tree).
 *
 * <p>This is deliberately NOT the same surface {@link RSslLiveSession}'s {@code verifier()} test
 * covers. That test drives {@code HttpsURLConnection}, whose hostname verification was already
 * correct before the fix this vector guards; the bug lived one layer down, in the raw {@code
 * SSLEngine} client path {@code HttpsURLConnection} does not use. A test that only exercises
 * {@code HttpsURLConnection} would pass whether or not this specific defect is present, which is
 * exactly the gap that shipped the CVE. Reaching the real code path means driving {@code
 * SSLEngine} by hand: {@code beginHandshake()} and a manual wrap/unwrap loop, since {@code
 * SSLEngine} (unlike {@code SSLSocket}) never touches an I/O channel itself.
 *
 * <p>The server side does not need any of that: nothing about this bug was server-side, so the
 * server is a plain {@code SSLServerSocket} handing back {@code SSLSocket}s, which negotiate
 * their half of the handshake automatically. Buffers are fixed at 32 KiB -- generous for one
 * small self-signed leaf and its handshake messages, which sidesteps {@code SSLEngine}'s
 * BUFFER_OVERFLOW resize dance entirely; a production client would need to handle that, a
 * fixed-size test fixture does not.
 */
public class RSslEndpointIdentification {
    static int checks;
    static final int EXPECTED_CHECKS = 4;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    // ---- a minimal self-signed leaf, CN=localhost / SAN=DNS:localhost only ------------------
    // Verbatim copy of RSslLiveSession's DER-encoding helpers: no external tool (keytool) is
    // available to a self-contained regression-suite vector, so the certificate is built by
    // hand from raw ASN.1 TLVs. Kept as a second copy rather than shared, deliberately -- see
    // "Why a second copy" below main().

    static byte[] tlv(int tag, byte[] body) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        o.write(tag);
        int n = body.length;
        if (n < 128) {
            o.write(n);
        } else if (n < 256) {
            o.write(0x81);
            o.write(n);
        } else {
            o.write(0x82);
            o.write((n >> 8) & 0xff);
            o.write(n & 0xff);
        }
        o.write(body, 0, body.length);
        return o.toByteArray();
    }

    static byte[] cat(byte[]... parts) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        for (byte[] p : parts) {
            o.write(p, 0, p.length);
        }
        return o.toByteArray();
    }

    static byte[] seq(byte[]... parts) {
        return tlv(0x30, cat(parts));
    }

    static byte[] oid(int... enc) {
        byte[] b = new byte[enc.length];
        for (int i = 0; i < enc.length; i++) {
            b[i] = (byte) enc[i];
        }
        return tlv(0x06, b);
    }

    static byte[] ascii(int tag, String s) {
        return tlv(tag, s.getBytes(java.nio.charset.StandardCharsets.US_ASCII));
    }

    static X509Certificate selfSigned(KeyPair kp) throws Exception {
        byte[] alg = seq(oid(0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b),
                new byte[] { 0x05, 0x00 });
        byte[] name = seq(tlv(0x31, seq(oid(0x55, 0x04, 0x03), ascii(0x13, "localhost"))));
        byte[] validity = seq(ascii(0x17, "200101000000Z"), ascii(0x17, "491231235959Z"));
        byte[] san = seq(ascii(0x82, "localhost"));
        byte[] exts = tlv(0xa3, seq(seq(oid(0x55, 0x1d, 0x11), tlv(0x04, san))));
        byte[] tbs = seq(tlv(0xa0, new byte[] { 0x02, 0x01, 0x02 }),
                new byte[] { 0x02, 0x01, 0x2a }, alg, name, validity, name,
                kp.getPublic().getEncoded(), exts);
        Signature signer = Signature.getInstance("SHA256withRSA");
        signer.initSign(kp.getPrivate());
        signer.update(tbs);
        byte[] der = seq(tbs, alg, tlv(0x03, cat(new byte[] { 0 }, signer.sign())));
        return (X509Certificate) CertificateFactory.getInstance("X.509")
                .generateCertificate(new java.io.ByteArrayInputStream(der));
    }

    static final char[] PW = "cratonvm-endpointid".toCharArray();
    static SSLContext serverCtx;
    static SSLContext clientCtx;

    static void buildContexts() throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        X509Certificate leaf = selfSigned(kp);
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, null);
        ks.setKeyEntry("k", kp.getPrivate(), PW, new Certificate[] { leaf });
        KeyStore ts = KeyStore.getInstance("PKCS12");
        ts.load(null, null);
        ts.setCertificateEntry("ca", leaf);
        KeyManagerFactory kmf =
                KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, PW);
        serverCtx = SSLContext.getInstance("TLS");
        serverCtx.init(kmf.getKeyManagers(), null, null);
        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ts);
        clientCtx = SSLContext.getInstance("TLS");
        // An accepting TrustManager would defeat the point: the whole finding behind this
        // vector is that endpoint identification is a SEPARATE gate from trust, and installing
        // a permissive one here would prove nothing about it. The real, RFC-6125-checking
        // default trust manager (built from `ts`, which trusts this one leaf) stays in place.
        clientCtx.init(null, tmf.getTrustManagers(), null);
    }

    // ---- server: a plain SSLServerSocket, handshake handled by the JDK ----------------------

    static int port;

    /** Accept one connection, read one line, answer with a fixed line, then stop. */
    static Thread startServer(SSLServerSocket ss) {
        Thread t = new Thread(() -> {
            try (SSLSocket s = (SSLSocket) ss.accept()) {
                s.setSoTimeout(20000);
                byte[] buf = new byte[64];
                int n = s.getInputStream().read(buf);
                if (n > 0) {
                    s.getOutputStream().write("server-ok\n".getBytes(java.nio.charset.StandardCharsets.UTF_8));
                    s.getOutputStream().flush();
                }
            } catch (Exception e) {
                // Expected for the mismatch case: the client aborts the handshake before any
                // application data is exchanged, which the server sees as a closed/reset
                // socket. Not a failure of this vector -- only recorded if BOTH connections
                // (mismatch and control) hit it, which the two checks below catch.
            }
        }, "RSslEndpointIdentification-server");
        t.setDaemon(true);
        t.start();
        return t;
    }

    // ---- client: raw SSLEngine, manual handshake loop ----------------------------------------

    static final int BUF = 32 * 1024;

    /**
     * Drive {@code engine}'s handshake to completion over {@code ch}, or let whatever it throws
     * propagate. A handshake that fails identity checking throws from {@code unwrap} while
     * processing the server's certificate message -- the exact call this exists to reach.
     */
    static void runHandshake(SocketChannel ch, SSLEngine engine) throws Exception {
        ByteBuffer netOut = ByteBuffer.allocate(BUF);
        ByteBuffer netIn = ByteBuffer.allocate(BUF);
        ByteBuffer appIn = ByteBuffer.allocate(BUF);
        ByteBuffer empty = ByteBuffer.allocate(0);

        engine.beginHandshake();
        SSLEngineResult.HandshakeStatus hs = engine.getHandshakeStatus();
        while (hs != SSLEngineResult.HandshakeStatus.FINISHED
                && hs != SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING) {
            switch (hs) {
                case NEED_WRAP: {
                    netOut.clear();
                    SSLEngineResult r = engine.wrap(empty, netOut);
                    hs = r.getHandshakeStatus();
                    netOut.flip();
                    while (netOut.hasRemaining()) {
                        ch.write(netOut);
                    }
                    break;
                }
                case NEED_UNWRAP: {
                    if (netIn.position() == 0 || netIn.remaining() == 0) {
                        int n = ch.read(netIn);
                        if (n < 0) {
                            throw new IOException("server closed the channel mid-handshake");
                        }
                    }
                    netIn.flip();
                    SSLEngineResult r;
                    try {
                        r = engine.unwrap(netIn, appIn);
                    } finally {
                        netIn.compact();
                    }
                    hs = r.getHandshakeStatus();
                    if (r.getStatus() == SSLEngineResult.Status.BUFFER_UNDERFLOW) {
                        int n = ch.read(netIn);
                        if (n < 0) {
                            throw new IOException("server closed the channel mid-handshake");
                        }
                    }
                    break;
                }
                case NEED_TASK: {
                    Runnable r;
                    while ((r = engine.getDelegatedTask()) != null) {
                        r.run();
                    }
                    hs = engine.getHandshakeStatus();
                    break;
                }
                default:
                    throw new IllegalStateException("unexpected handshake status " + hs);
            }
        }
    }

    static SSLEngine clientEngine(String host) {
        SSLEngine engine = clientCtx.createSSLEngine(host, port);
        engine.setUseClientMode(true);
        SSLParameters params = engine.getSSLParameters();
        params.setEndpointIdentificationAlgorithm("HTTPS");
        engine.setSSLParameters(params);
        return engine;
    }

    /** THE CASE: connect to 127.0.0.1 with a cert issued only for "localhost". Must be refused. */
    static void mismatchIsRefused(SSLServerSocket ss) throws Exception {
        Thread server = startServer(ss);
        try (SocketChannel ch = SocketChannel.open(
                new InetSocketAddress(InetAddress.getByName("127.0.0.1"), port))) {
            ch.configureBlocking(true);
            SSLEngine engine = clientEngine("127.0.0.1");
            SSLException thrown = null;
            try {
                runHandshake(ch, engine);
            } catch (SSLException e) {
                thrown = e;
            }
            check(thrown != null,
                    "SSLEngine must refuse a handshake with a certificate that does not match "
                            + "the connected host, when endpoint identification is set to HTTPS "
                            + "-- got no exception at all (CVE-2018-8034 shape)");
            check(engine.getHandshakeStatus() != SSLEngineResult.HandshakeStatus.FINISHED,
                    "a refused handshake must not report FINISHED");
        }
        server.join(5000);
    }

    /** THE CONTROL: the SAME check, against the hostname the cert actually names. Must succeed. */
    static void matchingHostSucceeds(SSLServerSocket ss) throws Exception {
        Thread server = startServer(ss);
        try (SocketChannel ch = SocketChannel.open(
                new InetSocketAddress(InetAddress.getByName("127.0.0.1"), port))) {
            ch.configureBlocking(true);
            SSLEngine engine = clientEngine("localhost");
            runHandshake(ch, engine); // must not throw
            check(engine.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.FINISHED
                            || engine.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING,
                    "a handshake against the certificate's own name must complete");

            // Prove the connection is genuinely usable, not just a completed crypto handshake:
            // wrap one line of application data through, and read the server's fixed reply.
            ByteBuffer plain = ByteBuffer.wrap("ping\n".getBytes(java.nio.charset.StandardCharsets.UTF_8));
            ByteBuffer netOut = ByteBuffer.allocate(BUF);
            engine.wrap(plain, netOut);
            netOut.flip();
            while (netOut.hasRemaining()) {
                ch.write(netOut);
            }
            ByteBuffer netIn = ByteBuffer.allocate(BUF);
            ByteBuffer appIn = ByteBuffer.allocate(BUF);
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
            String reply = "";
            while (!reply.contains("server-ok") && System.nanoTime() < deadline) {
                int n = ch.read(netIn);
                if (n < 0) {
                    break;
                }
                netIn.flip();
                engine.unwrap(netIn, appIn);
                netIn.compact();
                appIn.flip();
                byte[] got = new byte[appIn.remaining()];
                appIn.get(got);
                appIn.clear();
                reply += new String(got, java.nio.charset.StandardCharsets.UTF_8);
            }
            check(reply.contains("server-ok"),
                    "application data must actually flow once the handshake succeeds; got: ["
                            + reply + "]");
        }
        server.join(5000);
    }

    public static void main(String[] args) throws Exception {
        buildContexts();
        AtomicReference<Throwable> failure = new AtomicReference<>();
        Thread wd = new Thread(() -> {
            try {
                Thread.sleep(60000);
            } catch (InterruptedException e) {
                return;
            }
            System.out.println("CK RSslEndpointIdentification FAILED watchdog-60s");
            System.out.flush();
            Runtime.getRuntime().halt(4);
        }, "RSslEndpointIdentification-watchdog");
        wd.setDaemon(true);
        wd.start();

        // One listener for the whole run, bound once. Both phases run sequentially and each
        // owns exactly one accept() on it -- rebinding a fresh listener to the same ephemeral
        // port between phases would risk a spurious BindException while the OS still has the
        // old socket in TIME_WAIT, which a previous version of this file did.
        try (SSLServerSocket ss = (SSLServerSocket) serverCtx.getServerSocketFactory()
                .createServerSocket(0, 8, InetAddress.getByName("127.0.0.1"))) {
            port = ss.getLocalPort();
            mismatchIsRefused(ss);
            matchingHostSucceeds(ss);
        } catch (Throwable t) {
            failure.set(t);
        }

        if (failure.get() != null) {
            System.out.println("CK RSslEndpointIdentification FAILED "
                    + failure.get().getClass().getName() + ": " + failure.get().getMessage());
            System.out.flush();
            failure.get().printStackTrace();
            System.exit(1);
        }
        if (checks != EXPECTED_CHECKS) {
            throw new AssertionError(
                    "check count moved: expected " + EXPECTED_CHECKS + ", ran " + checks);
        }
        System.out.println("CK RSslEndpointIdentification checks=" + checks);
        System.out.println("PASS RSslEndpointIdentification (" + checks + " checks)");
    }

    // Why a second copy of the cert-generation helpers instead of sharing RSslLiveSession's:
    // that file is a large (845-line), carefully-tuned session-state vector with its own
    // watchdog, port, and accept-count wiring (`serve(ss, 4)` accepts exactly four
    // connections, matched to its five call sites). Adding a fifth connection with a
    // different failure mode (a REFUSED handshake, which the server sees as an aborted
    // socket rather than a clean close) risks the existing file's accept bookkeeping for a
    // property that has nothing to do with what that vector tests. A second ~60-line copy
    // of ASN.1 boilerplate is cheap; a subtle regression in a file already guarding five
    // other JSSE properties is not.
}
