import io.netty.buffer.UnpooledByteBufAllocator;
import io.netty.handler.ssl.SslContext;
import io.netty.handler.ssl.SslContextBuilder;
import io.netty.handler.ssl.SslProvider;
import io.netty.handler.ssl.ClientAuth;
import io.netty.handler.ssl.util.SelfSignedCertificate;
import io.netty.handler.ssl.util.SimpleTrustManagerFactory;

import javax.net.ssl.ManagerFactoryParameters;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLEngineResult;
import javax.net.ssl.TrustManager;
import javax.net.ssl.X509TrustManager;
import java.nio.ByteBuffer;
import java.security.KeyStore;
import java.security.cert.X509Certificate;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Hammer the ONE native that was caught holding a stale pin.
 *
 * `hunt8` run 5 named `engine_consult_trust_managers`
 * (`native-builtins/src/t27_tls.rs`) as the holder of a receiver the collector
 * had moved: it pins each trust manager, re-derives it from the pin, and the
 * very next read — `tm_is_extended`'s `class_id_of_object` — faulted on a
 * vacated address. Reaching that native through the full netty class costs a
 * whole-class run and hits maybe 1 time in 20 to 1 in 230, which is not a loop
 * a fix can be iterated in.
 *
 * This drives the same native directly: in-memory `SSLEngine` handshake pairs,
 * a custom `X509TrustManager` on the server so `engine_consult_trust_managers`
 * actually has managers to consult, and peer threads allocating hard so a
 * collection can land inside the window between the pin and the read.
 *
 * No sockets and no event loops — a handshake is a few hundred microseconds,
 * so this does in a minute what the class does in a day.
 *
 * A `NoSuchMethodError` naming `java.lang.Object` is the catch. Anything else,
 * including a clean finish, is a result about THIS PROBE and not about the
 * defect — the previous attempt (`MhStaleReceiverProbe`) reproduced nothing
 * and is recorded as a negative for exactly that reason.
 */
public final class TmChurnProbe {

    /** Same shape as the netty test's: a manager that rejects, so the engine consults it. */
    static final class RejectingTmf extends SimpleTrustManagerFactory {
        @Override protected void engineInit(KeyStore keyStore) { }
        @Override protected void engineInit(ManagerFactoryParameters p) { }
        @Override protected TrustManager[] engineGetTrustManagers() {
            return new TrustManager[] { new X509TrustManager() {
                @Override public void checkClientTrusted(X509Certificate[] c, String s) { }
                @Override public void checkServerTrusted(X509Certificate[] c, String s) { }
                @Override public X509Certificate[] getAcceptedIssuers() {
                    return new X509Certificate[0];
                }
            } };
        }
    }

    private static final AtomicLong HANDSHAKES = new AtomicLong();
    private static final AtomicLong CAUGHT = new AtomicLong();
    private static volatile Object sink;

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 120;
        int workers = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        int pressure = args.length > 2 ? Integer.parseInt(args[2]) : 2;

        SelfSignedCertificate ssc = new SelfSignedCertificate();
        final SslContext serverCtx = SslContextBuilder
                .forServer(ssc.certificate(), ssc.privateKey())
                .sslProvider(SslProvider.JDK)
                .trustManager(new RejectingTmf())
                .clientAuth(ClientAuth.REQUIRE)
                .build();
        final SslContext clientCtx = SslContextBuilder.forClient()
                .sslProvider(SslProvider.JDK)
                .trustManager(new RejectingTmf())
                .keyManager(ssc.certificate(), ssc.privateKey())
                .build();

        // Peer threads whose only job is to make a collection land inside the
        // pin-to-read window on a DIFFERENT thread — the shape a single-threaded
        // probe cannot produce, and the one the last probe was missing.
        for (int i = 0; i < pressure; i++) {
            Thread t = new Thread(() -> {
                Object[] ring = new Object[256];
                int k = 0;
                while (!Thread.currentThread().isInterrupted()) {
                    ring[k++ & 255] = new byte[8192];
                }
            }, "gc-pressure-" + i);
            t.setDaemon(true);
            t.start();
        }

        final long deadline = System.nanoTime() + seconds * 1_000_000_000L;
        Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            ts[w] = new Thread(() -> {
                while (System.nanoTime() < deadline && CAUGHT.get() < 5) {
                    try {
                        handshake(serverCtx, clientCtx);
                        HANDSHAKES.incrementAndGet();
                    } catch (NoSuchMethodError e) {
                        CAUGHT.incrementAndGet();
                        System.out.println("PROBE CAUGHT " + e.getMessage());
                        System.out.flush();
                    } catch (Throwable ignored) {
                        // A rejected handshake is the NORMAL outcome here — the
                        // trust managers are consulted either way, which is the
                        // whole point. Only NoSuchMethodError is the catch.
                    }
                }
            }, "tm-churn-" + w);
            ts[w].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("PROBE done handshakes=" + HANDSHAKES.get()
                + " caught=" + CAUGHT.get() + " workers=" + workers);
        System.out.flush();
        System.exit(CAUGHT.get() > 0 ? 3 : 0);
    }

    /** One in-memory handshake, driven to completion or failure. */
    private static void handshake(SslContext serverCtx, SslContext clientCtx) throws Exception {
        UnpooledByteBufAllocator alloc = UnpooledByteBufAllocator.DEFAULT;
        SSLEngine server = serverCtx.newEngine(alloc);
        SSLEngine client = clientCtx.newEngine(alloc);
        server.setUseClientMode(false);
        server.setNeedClientAuth(true);
        client.setUseClientMode(true);
        server.beginHandshake();
        client.beginHandshake();

        int cap = Math.max(server.getSession().getPacketBufferSize(),
                           client.getSession().getPacketBufferSize());
        int app = Math.max(server.getSession().getApplicationBufferSize(),
                           client.getSession().getApplicationBufferSize());
        ByteBuffer c2s = ByteBuffer.allocate(cap);
        ByteBuffer s2c = ByteBuffer.allocate(cap);
        ByteBuffer appBuf = ByteBuffer.allocate(app);
        ByteBuffer empty = ByteBuffer.allocate(0);

        for (int step = 0; step < 64; step++) {
            boolean progressed = false;
            progressed |= pump(client, empty, c2s, appBuf);
            progressed |= pump(server, empty, s2c, appBuf);
            c2s.flip();
            if (c2s.hasRemaining()) {
                server.unwrap(c2s, appBuf);
                appBuf.clear();
                progressed = true;
            }
            c2s.compact();
            s2c.flip();
            if (s2c.hasRemaining()) {
                client.unwrap(s2c, appBuf);
                appBuf.clear();
                progressed = true;
            }
            s2c.compact();
            // Keep something allocating on THIS thread too, so the window is
            // not only closed by the peers.
            sink = new byte[512];
            if (!progressed) {
                break;
            }
        }
    }

    private static boolean pump(SSLEngine e, ByteBuffer src, ByteBuffer dst, ByteBuffer appBuf)
            throws Exception {
        boolean did = false;
        for (int i = 0; i < 8; i++) {
            SSLEngineResult.HandshakeStatus hs = e.getHandshakeStatus();
            if (hs == SSLEngineResult.HandshakeStatus.NEED_TASK) {
                Runnable r;
                while ((r = e.getDelegatedTask()) != null) {
                    r.run();
                }
                did = true;
            } else if (hs == SSLEngineResult.HandshakeStatus.NEED_WRAP) {
                e.wrap(src, dst);
                did = true;
            } else {
                break;
            }
        }
        return did;
    }
}
