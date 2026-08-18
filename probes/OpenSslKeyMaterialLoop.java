import io.netty.buffer.ByteBufAllocator;
import io.netty.buffer.UnpooledByteBufAllocator;
import io.netty.handler.ssl.OpenSsl;
import io.netty.handler.ssl.SslContext;
import io.netty.handler.ssl.SslContextBuilder;
import io.netty.handler.ssl.SslProvider;
import io.netty.handler.ssl.util.InsecureTrustManagerFactory;
import io.netty.handler.ssl.util.SelfSignedCertificate;

import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLEngineResult;
import java.nio.ByteBuffer;

/**
 * The `ParameterizedSslHandlerTest.reentryOnHandshakeCompleteNioChannel`
 * residual of `openssl-key-material-and-engine-residuals-20260813.md` §D,
 * reduced to the part that fails.
 *
 * That test does its work over real NIO sockets and an event loop, so a
 * failure there is three layers away from the thing that broke. What it
 * actually repeats is: build an OPENSSL server context from a
 * SelfSignedCertificate's PEM FILES, build a JDK client context, and run one
 * handshake. The three errors the page records —
 *
 *   OpenSslHandshakeException: error:100000ae:…:NO_CERTIFICATE_SET
 *   SSLHandshakeException:     Unable to find key material for auth method(s)
 *   SSLException:              PrivateKey type not supported PKCS#8
 *
 * are all raised inside netty's `OpenSslKeyMaterialProvider`, i.e. before a
 * single byte reaches a socket. So this drives the same path with no sockets
 * and no event loop, `REPS` times, with allocation between iterations so a
 * young collection lands inside the key-material work rather than between
 * runs.
 *
 * `--gc` and the heap size are the caller's to choose: the page's hypothesis
 * is a native local held live across an allocation, which only a MOVING
 * collector can expose, so a green run under one collector proves nothing
 * about another.
 *
 * Prints one line per failure with the exception CHAIN, not just the top
 * frame: `PrivateKey type not supported %s` is raised by a `catch` whose
 * message prints `key.getFormat()`, so the top frame names the format and the
 * CAUSE names what actually refused the bytes.
 */
public final class OpenSslKeyMaterialLoop {

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        boolean churn = args.length <= 1 || !"nochurn".equals(args[1]);

        System.out.println("OpenSsl.isAvailable=" + OpenSsl.isAvailable());
        if (!OpenSsl.isAvailable()) {
            Throwable cause = OpenSsl.unavailabilityCause();
            System.out.println("UNAVAILABLE cause=" + cause);
            System.out.println("RESULT reps=0 ok=0 failed=0 (tcnative missing — this run proves nothing)");
            return;
        }

        SelfSignedCertificate ssc = new SelfSignedCertificate();
        ByteBufAllocator alloc = UnpooledByteBufAllocator.DEFAULT;

        int ok = 0;
        int failed = 0;
        Object[] garbage = new Object[128];

        for (int i = 0; i < reps; i++) {
            if (churn) {
                // Young-generation pressure, so a collection can land inside
                // the key-material work below rather than between iterations.
                for (int g = 0; g < garbage.length; g++) {
                    garbage[g] = new byte[8192];
                }
            }
            try {
                SslContext server = SslContextBuilder
                        .forServer(ssc.certificate(), ssc.privateKey())
                        .sslProvider(SslProvider.OPENSSL)
                        .build();
                SslContext client = SslContextBuilder.forClient()
                        .trustManager(InsecureTrustManagerFactory.INSTANCE)
                        .sslProvider(SslProvider.JDK)
                        .build();
                SSLEngine se = server.newEngine(alloc);
                SSLEngine ce = client.newEngine(alloc);
                se.setUseClientMode(false);
                ce.setUseClientMode(true);
                handshake(ce, se);
                ok++;
            } catch (Throwable t) {
                failed++;
                StringBuilder chain = new StringBuilder();
                for (Throwable c = t; c != null && chain.length() < 600; c = c.getCause()) {
                    if (chain.length() > 0) {
                        chain.append("  <- ");
                    }
                    chain.append(c.getClass().getName()).append(": ").append(c.getMessage());
                    if (c.getCause() == c) {
                        break;
                    }
                }
                System.out.println("FAIL iteration=" + i + " " + chain);
            }
        }
        ssc.delete();
        System.out.println("RESULT reps=" + reps + " ok=" + ok + " failed=" + failed);
    }

    /**
     * Drive both engines to a completed handshake through host byte buffers.
     *
     * Bounded by a step count rather than a clock: a stalled handshake must
     * fail this probe rather than hang it, since the page it serves is about
     * an intermittent stall as well as an exception.
     */
    private static void handshake(SSLEngine client, SSLEngine server) throws Exception {
        client.beginHandshake();
        server.beginHandshake();

        int packet = Math.max(client.getSession().getPacketBufferSize(),
                              server.getSession().getPacketBufferSize());
        int app = Math.max(client.getSession().getApplicationBufferSize(),
                           server.getSession().getApplicationBufferSize());

        ByteBuffer cToS = ByteBuffer.allocate(packet * 2);
        ByteBuffer sToC = ByteBuffer.allocate(packet * 2);
        ByteBuffer appBuf = ByteBuffer.allocate(app * 2);
        ByteBuffer empty = ByteBuffer.allocate(0);

        for (int step = 0; step < 200; step++) {
            boolean progressed = false;
            progressed |= pump(client, empty, cToS, appBuf, sToC);
            progressed |= pump(server, empty, sToC, appBuf, cToS);
            if (done(client) && done(server)) {
                return;
            }
            if (!progressed) {
                throw new IllegalStateException("handshake stalled: client="
                        + client.getHandshakeStatus() + " server=" + server.getHandshakeStatus());
            }
        }
        throw new IllegalStateException("handshake did not finish in 200 steps");
    }

    private static boolean done(SSLEngine e) {
        return e.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING
                || e.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.FINISHED;
    }

    /** One wrap/unwrap/task turn for one engine. Returns whether it moved. */
    private static boolean pump(SSLEngine e, ByteBuffer emptyApp, ByteBuffer out,
                                ByteBuffer appBuf, ByteBuffer in) throws Exception {
        boolean progressed = false;
        Runnable task;
        while ((task = e.getDelegatedTask()) != null) {
            task.run();
            progressed = true;
        }
        if (e.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NEED_WRAP) {
            SSLEngineResult r = e.wrap(emptyApp, out);
            progressed |= r.bytesProduced() > 0;
        }
        if (e.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NEED_UNWRAP && in.position() > 0) {
            in.flip();
            appBuf.clear();
            SSLEngineResult r = e.unwrap(in, appBuf);
            in.compact();
            progressed |= r.bytesConsumed() > 0;
        }
        return progressed;
    }
}
