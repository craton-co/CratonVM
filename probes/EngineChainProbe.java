import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.ByteBuffer;
import java.security.KeyStore;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLEngineResult;
import javax.net.ssl.SSLSession;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;

/**
 * The OTHER half of "the client captures only the leaf".
 *
 * `tls-client-captures-only-the-leaf-...` CONFIRMED the defect on the
 * `SSLSocket` path (`servlet::s2_tls_connect_on`) and left the `SSLEngine`
 * path — `t27_tls`, rustls-backed, the one netty and Tomcat drive — as NOT
 * VERIFIED, with the note that rustls exposes the full chain to its verifier
 * so it may well be unaffected. "May well be" is not a measurement, and a
 * green platform cannot close a bug the other one owns.
 *
 * So this is the same question asked of the engine: drive a real client
 * handshake against a real public host through `SSLEngine` alone, and read
 * `getSession().getPeerCertificates().length`. HotSpot answers 2-4. A 1 here
 * would mean the engine path has the identical defect and the SSLSocket fix
 * does not reach it.
 *
 * Two arms per host, exactly as `CustomTmProbe` does it, and for the same
 * reason: the default context and a context initialised with the JDK's OWN
 * default TrustManagers. A difference between the two isolates the chain
 * rather than the trust store.
 */
public class EngineChainProbe {
    static final String[] HOSTS = { "github.com", "www.google.com", "repo.maven.apache.org" };

    public static void main(String[] a) throws Exception {
        SSLContext def = SSLContext.getDefault();

        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init((KeyStore) null);
        TrustManager[] tms = tmf.getTrustManagers();
        SSLContext custom = SSLContext.getInstance("TLS");
        custom.init(null, tms, null);

        for (String host : HOSTS) {
            System.out.println("HOST " + host);
            System.out.println("   default-ctx-engine : " + attempt(def, host));
            System.out.println("   custom-TM-engine   : " + attempt(custom, host));
        }
        System.out.println("PROBE-DONE");
        Runtime.getRuntime().halt(0);
    }

    static String attempt(SSLContext ctx, String host) {
        Socket sock = null;
        try {
            SSLEngine engine = ctx.createSSLEngine(host, 443);
            engine.setUseClientMode(true);
            sock = new Socket();
            sock.connect(new InetSocketAddress(host, 443), 10000);
            sock.setSoTimeout(15000);
            handshake(engine, sock);
            SSLSession s = engine.getSession();
            int n = s.getPeerCertificates().length;
            return "OK  peerChainLen=" + n + " protocol=" + s.getProtocol();
        } catch (Throwable e) {
            String m = e.getMessage();
            if (m != null && m.length() > 110) {
                m = m.substring(0, 110);
            }
            return "FAIL " + e.getClass().getSimpleName() + ": " + m;
        } finally {
            if (sock != null) {
                try {
                    sock.close();
                } catch (Throwable ignored) {
                    // Closing the probe's own socket cannot change a verdict
                    // already printed.
                }
            }
        }
    }

    /**
     * A plain blocking wrap/unwrap pump. Deliberately not `SSLSocket` and not
     * a Selector: the point is to reach the engine implementation directly,
     * with nothing in between that could supply a chain of its own.
     */
    static void handshake(SSLEngine engine, Socket sock) throws Exception {
        SSLSession session = engine.getSession();
        ByteBuffer netOut = ByteBuffer.allocate(session.getPacketBufferSize() + 2048);
        ByteBuffer netIn = ByteBuffer.allocate(session.getPacketBufferSize() + 2048);
        ByteBuffer appIn = ByteBuffer.allocate(session.getApplicationBufferSize() + 2048);
        ByteBuffer empty = ByteBuffer.allocate(0);
        InputStream in = sock.getInputStream();
        OutputStream out = sock.getOutputStream();
        byte[] raw = new byte[16 * 1024];

        engine.beginHandshake();
        long deadline = System.currentTimeMillis() + 20000;
        while (System.currentTimeMillis() < deadline) {
            SSLEngineResult.HandshakeStatus st = engine.getHandshakeStatus();
            if (st == SSLEngineResult.HandshakeStatus.FINISHED
                    || st == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING) {
                return;
            }
            switch (st) {
                case NEED_TASK: {
                    Runnable t;
                    while ((t = engine.getDelegatedTask()) != null) {
                        t.run();
                    }
                    break;
                }
                case NEED_WRAP: {
                    netOut.clear();
                    SSLEngineResult r = engine.wrap(empty, netOut);
                    netOut.flip();
                    if (netOut.hasRemaining()) {
                        byte[] b = new byte[netOut.remaining()];
                        netOut.get(b);
                        out.write(b);
                        out.flush();
                    }
                    if (r.getStatus() == SSLEngineResult.Status.CLOSED) {
                        throw new IllegalStateException("engine closed during wrap");
                    }
                    if (r.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.FINISHED) {
                        return;
                    }
                    break;
                }
                case NEED_UNWRAP:
                case NEED_UNWRAP_AGAIN: {
                    // Only read more bytes when the buffered ones are spent —
                    // a TLS record can carry several handshake messages, and
                    // reading unconditionally would block after the last one.
                    if (netIn.position() == 0 || !unwrapOnce(engine, netIn, appIn)) {
                        int n = in.read(raw);
                        if (n < 0) {
                            throw new java.io.EOFException("peer closed during handshake");
                        }
                        netIn.put(raw, 0, n);
                        unwrapOnce(engine, netIn, appIn);
                    }
                    break;
                }
                default:
                    throw new IllegalStateException("unexpected handshake status " + st);
            }
        }
        throw new IllegalStateException("handshake did not complete within 20s");
    }

    /** @return true when the engine consumed something. */
    static boolean unwrapOnce(SSLEngine engine, ByteBuffer netIn, ByteBuffer appIn)
            throws Exception {
        netIn.flip();
        boolean consumed = false;
        try {
            while (netIn.hasRemaining()) {
                appIn.clear();
                SSLEngineResult r = engine.unwrap(netIn, appIn);
                if (r.getStatus() == SSLEngineResult.Status.BUFFER_UNDERFLOW) {
                    break;
                }
                if (r.getStatus() == SSLEngineResult.Status.CLOSED) {
                    throw new IllegalStateException("engine closed during unwrap");
                }
                if (r.bytesConsumed() == 0 && r.bytesProduced() == 0) {
                    break;
                }
                consumed = true;
                if (r.getHandshakeStatus() != SSLEngineResult.HandshakeStatus.NEED_UNWRAP
                        && r.getHandshakeStatus()
                                != SSLEngineResult.HandshakeStatus.NEED_UNWRAP_AGAIN) {
                    break;
                }
            }
        } finally {
            netIn.compact();
        }
        return consumed;
    }
}
