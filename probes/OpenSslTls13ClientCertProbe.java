import io.netty.bootstrap.Bootstrap;
import io.netty.bootstrap.ServerBootstrap;
import io.netty.channel.Channel;
import io.netty.channel.ChannelFuture;
import io.netty.channel.ChannelHandlerContext;
import io.netty.channel.ChannelInboundHandlerAdapter;
import io.netty.channel.ChannelInitializer;
import io.netty.channel.MultiThreadIoEventLoopGroup;
import io.netty.channel.nio.NioIoHandler;
import io.netty.channel.socket.nio.NioServerSocketChannel;
import io.netty.channel.socket.nio.NioSocketChannel;
import io.netty.handler.ssl.ClientAuth;
import io.netty.handler.ssl.IdentityCipherSuiteFilter;
import io.netty.handler.ssl.OpenSslContext;
import io.netty.handler.ssl.SslContext;
import io.netty.handler.ssl.SslContextBuilder;
import io.netty.handler.ssl.SslHandshakeCompletionEvent;
import io.netty.handler.ssl.SslProvider;
import io.netty.handler.ssl.util.SelfSignedCertificate;

import javax.net.ssl.SSLEngine;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509ExtendedTrustManager;
import java.net.Socket;
import java.security.KeyStore;
import java.security.cert.CertificateException;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.io.FileInputStream;

import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.util.Arrays;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * The residual left in netty's SSL classes once the wildcard-bind and
 * endpoint-identification defects are closed: a mutual-TLS handshake in which
 * the CLIENT sends no certificate, so the server dies with
 * `PEER_DID_NOT_RETURN_A_CERTIFICATE`.
 *
 * In `OpenSslEngineTest` it is exactly twelve of 48 parameterisations in every
 * method that needs a client certificate — `TLSv1.3` AND `useTasks=false`, for
 * all three buffer types and both `delegate` / `useTickets` values. This probe
 * takes the 48-case matrix out of it and varies the three axes that look
 * relevant directly, so "which axis decides" is a table rather than an
 * inference:
 *
 *   protocol   TLSv1.3 | TLSv1.2
 *   useTasks   false   | true      (whether netty runs BoringSSL's callbacks
 *                                   INSIDE `SSL_do_handshake` or defers them)
 *   transport  ::1     | 127.0.0.1 (the axis that only became visible when the
 *                                   wildcard bind stopped being v4-only, since
 *                                   `testMutualAuthSameCertChain` connects to
 *                                   `serverChannel.localAddress()` verbatim)
 *
 * HotSpot passes all eight. Run it on both and diff the `@@ROW` lines.
 *
 * usage: OpenSslTls13ClientCertProbe [rounds]
 */
public final class OpenSslTls13ClientCertProbe {
    private OpenSslTls13ClientCertProbe() {}

    private static int failures;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        SelfSignedCertificate serverCert = new SelfSignedCertificate();
        SelfSignedCertificate clientCert = new SelfSignedCertificate();
        // A certificate that actually names `localhost`, for the arm that turns
        // endpoint identification ON. Without it the identification would fail
        // for a legitimate reason and the arm would measure nothing.
        SelfSignedCertificate localhostCert = new SelfSignedCertificate("localhost");

        for (int r = 0; r < rounds; r++) {
            for (String host : new String[] {"::1", "127.0.0.1"}) {
                for (String protocol : new String[] {"TLSv1.3", "TLSv1.2"}) {
                    for (boolean useTasks : new boolean[] {false, true}) {
                        run(r, serverCert, clientCert, host, protocol, useTasks, null);
                    }
                }
            }
            // The combination NO netty test in this suite covers, and the one a
            // fix that avoids `getSSLParameters()` only in the null case would
            // leave exposed: a client that presents a certificate AND asks for
            // endpoint identification, on the parameterisation that runs the
            // callback inside the native. Server and client both authenticate
            // with a cert naming `localhost`, and the client dials `localhost`,
            // so a failure here is the VM and not the certificate.
            run(r, localhostCert, localhostCert, "localhost", "TLSv1.3", false, "HTTPS");
            run(r, localhostCert, localhostCert, "localhost", "TLSv1.3", true, "HTTPS");
        }

        // THE REPRODUCER, run last and scored separately. A trust manager that
        // re-enters tcnative from inside BoringSSL's callback is the open
        // defect; on HotSpot these two rows PASS, on CratonVM the
        // `useTasks=false` one is expected to fail with
        // PEER_DID_NOT_RETURN_A_CERTIFICATE. It is reported, never counted into
        // `failures`, because this probe's exit status is about the VM's own
        // fixes and not about a defect it is only demonstrating.
        int before = failures;
        run(0, serverCert, clientCert, "127.0.0.1", "TLSv1.3", false, null, true);
        run(0, serverCert, clientCert, "127.0.0.1", "TLSv1.3", true, null, true);
        int reproduced = failures - before;
        failures = before;
        System.out.println("@@REPRO reentrant-trust-manager rows_failed=" + reproduced
                + " (HotSpot: 0; a VM with the open re-entrancy defect: 1, the useTasks=false row)");

        System.out.println("@@PROBE failures=" + failures);
        if (failures != 0) {
            System.exit(1);
        }
    }

    private static void run(int round, SelfSignedCertificate serverCert,
            SelfSignedCertificate clientCert, String host, String protocol, boolean useTasks,
            String identificationAlgorithm) throws Exception {
        run(round, serverCert, clientCert, host, protocol, useTasks, identificationAlgorithm, false);
    }

    private static void run(int round, SelfSignedCertificate serverCert,
            SelfSignedCertificate clientCert, String host, String protocol, boolean useTasks,
            String identificationAlgorithm, boolean reenterFromCallback) throws Exception {
        String cipher = "TLSv1.3".equals(protocol)
                ? "TLS_AES_128_GCM_SHA256" : "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256";

        SslContext serverCtx = SslContextBuilder
                .forServer(serverCert.certificate(), serverCert.privateKey())
                .trustManager(clientCert.cert())
                .clientAuth(ClientAuth.REQUIRE)
                .sslProvider(SslProvider.OPENSSL)
                .protocols(protocol)
                .ciphers(Arrays.asList(cipher), IdentityCipherSuiteFilter.INSTANCE)
                .build();
        SslContext clientCtx = SslContextBuilder.forClient()
                .keyManager(clientCert.certificate(), clientCert.privateKey())
                .trustManager(reenterFromCallback
                        ? new ReentrantTrustManager(trustManagerFor(serverCert.certificate()))
                        : trustManagerFor(serverCert.certificate()))
                .sslProvider(SslProvider.OPENSSL)
                .protocols(protocol)
                .ciphers(Arrays.asList(cipher), IdentityCipherSuiteFilter.INSTANCE)
                .endpointIdentificationAlgorithm(identificationAlgorithm)
                .build();
        setUseTasks(serverCtx, useTasks);
        setUseTasks(clientCtx, useTasks);

        final CountDownLatch serverDone = new CountDownLatch(1);
        final CountDownLatch clientDone = new CountDownLatch(1);
        final Throwable[] serverCause = new Throwable[1];
        final Throwable[] clientCause = new Throwable[1];
        final boolean[] serverOk = new boolean[1];
        final boolean[] clientOk = new boolean[1];

        MultiThreadIoEventLoopGroup sg = new MultiThreadIoEventLoopGroup(NioIoHandler.newFactory());
        MultiThreadIoEventLoopGroup cg = new MultiThreadIoEventLoopGroup(NioIoHandler.newFactory());
        Channel server = null;
        Channel client = null;
        try {
            ServerBootstrap sb = new ServerBootstrap();
            sb.group(sg).channel(NioServerSocketChannel.class);
            final SslContext sctx = serverCtx;
            sb.childHandler(new ChannelInitializer<Channel>() {
                @Override
                protected void initChannel(Channel ch) {
                    ch.pipeline().addLast(sctx.newHandler(ch.alloc()));
                    ch.pipeline().addLast(new Recorder(serverDone, serverOk, serverCause));
                }
            });
            // Bind the loopback address EXPLICITLY, so the transport axis is the
            // probe's choice and not a consequence of what a wildcard bind
            // resolves to.
            server = sb.bind(new InetSocketAddress(InetAddress.getByName(host), 0))
                    .sync().channel();
            final int port = ((InetSocketAddress) server.localAddress()).getPort();

            Bootstrap cb = new Bootstrap();
            cb.group(cg).channel(NioSocketChannel.class);
            final SslContext cctx = clientCtx;
            final String peerHost = identificationAlgorithm == null ? null : host;
            cb.handler(new ChannelInitializer<Channel>() {
                @Override
                protected void initChannel(Channel ch) {
                    // `newHandler(alloc, host, port)` is what gives the client
                    // engine a peer host; without one, identification has
                    // nothing to match and the arm would measure nothing.
                    ch.pipeline().addLast(peerHost == null
                            ? cctx.newHandler(ch.alloc())
                            : cctx.newHandler(ch.alloc(), peerHost, port));
                    ch.pipeline().addLast(new Recorder(clientDone, clientOk, clientCause));
                }
            });
            ChannelFuture ccf = identificationAlgorithm == null
                    ? cb.connect(new InetSocketAddress(InetAddress.getByName(host), port))
                    // By NAME, so the engine has a peer host to identify against.
                    : cb.connect(new InetSocketAddress(host, port));
            ccf.awaitUninterruptibly();
            client = ccf.channel();

            boolean fired = clientDone.await(20, TimeUnit.SECONDS)
                    & serverDone.await(20, TimeUnit.SECONDS);
            boolean ok = ccf.isSuccess() && fired && clientOk[0] && serverOk[0];
            if (!ok) {
                failures++;
            }
            System.out.println("@@ROW " + (ok ? "PASS" : "FAIL")
                    + " round=" + round
                    + " transport=" + host
                    + " protocol=" + protocol
                    + " useTasks=" + useTasks
                    + " identify=" + identificationAlgorithm
                    + " reenter=" + reenterFromCallback
                    + " connect=" + ccf.isSuccess()
                    + " clientHandshake=" + clientOk[0]
                    + " serverHandshake=" + serverOk[0]
                    + " clientCause=" + brief(clientCause[0])
                    + " serverCause=" + brief(serverCause[0]));
            System.out.flush();
        } finally {
            if (client != null) {
                client.close().awaitUninterruptibly();
            }
            if (server != null) {
                server.close().awaitUninterruptibly();
            }
            sg.shutdownGracefully();
            cg.shutdownGracefully();
        }
    }

    /**
     * A trust manager that makes the offending call ITSELF.
     *
     * This is the reproducer for
     * `known-issues/netty/java-reentry-from-boringssl-verify-callback-loses-the-tls13-client-cert-20260826.md`,
     * and it lives here rather than behind a VM flag on purpose. The defect is
     * "Java re-enters tcnative from inside BoringSSL's certificate callback",
     * so a trust manager calling `engine.getSSLParameters()` from that callback
     * IS the defect — no switch in the shipped VM is needed to arm it, and a
     * shipped VM should not carry one that can weaken or break TLS.
     *
     * `getSSLParameters()` on netty's `ReferenceCountedOpenSslEngine` is
     * `synchronized` and re-enters tcnative for `SSL.getOptions` and, through
     * `super.getSSLParameters()` -> `getEnabledCipherSuites()`, `SSL.getCiphers`
     * — on the very `SSL*` BoringSSL is inside. The result is read and thrown
     * away: what matters is that the call was made, not what it answered.
     */
    static final class ReentrantTrustManager extends X509ExtendedTrustManager {
        private final X509ExtendedTrustManager delegate;

        ReentrantTrustManager(X509ExtendedTrustManager delegate) {
            this.delegate = delegate;
        }

        private static void reenter(SSLEngine engine) {
            try {
                engine.getSSLParameters().getEndpointIdentificationAlgorithm();
            } catch (RuntimeException ignored) {
                // The call being MADE is the experiment; its answer is not.
            }
        }

        @Override
        public void checkServerTrusted(X509Certificate[] c, String a, SSLEngine e)
                throws CertificateException {
            reenter(e);
            delegate.checkServerTrusted(c, a, e);
        }

        @Override
        public void checkClientTrusted(X509Certificate[] c, String a, SSLEngine e)
                throws CertificateException {
            reenter(e);
            delegate.checkClientTrusted(c, a, e);
        }

        @Override
        public void checkServerTrusted(X509Certificate[] c, String a) throws CertificateException {
            delegate.checkServerTrusted(c, a);
        }

        @Override
        public void checkClientTrusted(X509Certificate[] c, String a) throws CertificateException {
            delegate.checkClientTrusted(c, a);
        }

        @Override
        public void checkServerTrusted(X509Certificate[] c, String a, Socket s)
                throws CertificateException {
            delegate.checkServerTrusted(c, a, s);
        }

        @Override
        public void checkClientTrusted(X509Certificate[] c, String a, Socket s)
                throws CertificateException {
            delegate.checkClientTrusted(c, a, s);
        }

        @Override
        public X509Certificate[] getAcceptedIssuers() {
            return delegate.getAcceptedIssuers();
        }
    }

    /** The default JDK trust manager over a single trusted certificate. */
    private static X509ExtendedTrustManager trustManagerFor(java.io.File certFile) throws Exception {
        CertificateFactory cf = CertificateFactory.getInstance("X.509");
        KeyStore ks = KeyStore.getInstance(KeyStore.getDefaultType());
        ks.load(null, null);
        try (FileInputStream in = new FileInputStream(certFile)) {
            int i = 0;
            for (java.security.cert.Certificate c : cf.generateCertificates(in)) {
                ks.setCertificateEntry("ca" + i++, c);
            }
        }
        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ks);
        for (TrustManager tm : tmf.getTrustManagers()) {
            if (tm instanceof X509ExtendedTrustManager) {
                return (X509ExtendedTrustManager) tm;
            }
        }
        throw new IllegalStateException("no X509ExtendedTrustManager");
    }

    private static void setUseTasks(SslContext ctx, boolean useTasks) {
        if (ctx instanceof OpenSslContext) {
            ((OpenSslContext) ctx).setUseTasks(useTasks);
        }
    }

    private static String brief(Throwable t) {
        if (t == null) {
            return "none";
        }
        Throwable root = t;
        while (root.getCause() != null && root.getCause() != root) {
            root = root.getCause();
        }
        String m = root.getMessage();
        return root.getClass().getSimpleName() + "(" + (m == null ? "" : m) + ")";
    }

    private static final class Recorder extends ChannelInboundHandlerAdapter {
        private final CountDownLatch latch;
        private final boolean[] ok;
        private final Throwable[] cause;

        Recorder(CountDownLatch latch, boolean[] ok, Throwable[] cause) {
            this.latch = latch;
            this.ok = ok;
            this.cause = cause;
        }

        @Override
        public void userEventTriggered(ChannelHandlerContext ctx, Object evt) {
            if (evt instanceof SslHandshakeCompletionEvent) {
                SslHandshakeCompletionEvent e = (SslHandshakeCompletionEvent) evt;
                ok[0] = e.isSuccess();
                if (!e.isSuccess()) {
                    cause[0] = e.cause();
                }
                latch.countDown();
            }
            ctx.fireUserEventTriggered(evt);
        }

        @Override
        public void exceptionCaught(ChannelHandlerContext ctx, Throwable t) {
            if (cause[0] == null) {
                cause[0] = t;
            }
            latch.countDown();
        }
    }
}
