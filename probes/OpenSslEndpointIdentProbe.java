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
import io.netty.handler.ssl.IdentityCipherSuiteFilter;
import io.netty.handler.ssl.SslContext;
import io.netty.handler.ssl.SslContextBuilder;
import io.netty.handler.ssl.SslHandler;
import io.netty.handler.ssl.SslHandshakeCompletionEvent;
import io.netty.handler.ssl.SslProvider;
import io.netty.handler.ssl.util.InsecureTrustManagerFactory;

import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLParameters;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509ExtendedTrustManager;
import java.io.File;
import java.io.FileInputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.security.KeyStore;
import java.security.cert.CertificateException;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.util.Collection;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Why does `SSLEngineTest.testClientHostnameValidationFail` COMPLETE its
 * handshake on CratonVM with the OPENSSL provider (48/48 fail with
 * `IllegalStateException: handshake complete. expected failure`) while the same
 * test passes 12/12 with the JDK provider and 48/48 on HotSpot?
 *
 * Endpoint identification under netty's OpenSSL provider is not done by
 * BoringSSL. `ReferenceCountedOpenSslClientContext.setVerifyCallback` picks one
 * of two callbacks by `trustManager instanceof X509ExtendedTrustManager`:
 * the EXTENDED one calls `checkServerTrusted(chain, auth, engine)`, which is the
 * overload the JDK's `X509TrustManagerImpl` runs the "HTTPS" identity check in;
 * the plain one calls the two-argument `checkServerTrusted(chain, auth)`, which
 * validates the CHAIN and checks NO hostname at all. So a chain that verifies
 * against a cert issued for a different host is accepted, exactly as observed.
 *
 * This probe supplies its OWN trust manager and prints, per handshake, which
 * overload netty actually called and what the engine reported to it. It is a
 * measurement of the branch, not an argument about it.
 *
 * usage: OpenSslEndpointIdentProbe [openssl|jdk]
 */
public final class OpenSslEndpointIdentProbe {
    private OpenSslEndpointIdentProbe() {}

    private static final String RES = "io/netty/handler/ssl/";

    private static File res(String name) {
        java.net.URL u = OpenSslEndpointIdentProbe.class.getClassLoader().getResource(RES + name);
        if (u == null) {
            throw new IllegalStateException("resource not on the classpath: " + RES + name);
        }
        return new File(u.getFile().replaceFirst("^/([A-Za-z]:)", "$1"));
    }

    /** Records which `checkServerTrusted` overload netty reached, then delegates. */
    static final class Recording extends X509ExtendedTrustManager {
        private final X509ExtendedTrustManager delegate;

        Recording(X509ExtendedTrustManager delegate) {
            this.delegate = delegate;
        }

        @Override
        public void checkServerTrusted(X509Certificate[] chain, String authType)
                throws CertificateException {
            System.out.println("@@TM overload=2-arg(NO identity check) authType=" + authType);
            delegate.checkServerTrusted(chain, authType);
        }

        @Override
        public void checkServerTrusted(X509Certificate[] chain, String authType, Socket socket)
                throws CertificateException {
            System.out.println("@@TM overload=3-arg-Socket authType=" + authType);
            delegate.checkServerTrusted(chain, authType, socket);
        }

        @Override
        public void checkServerTrusted(X509Certificate[] chain, String authType, SSLEngine engine)
                throws CertificateException {
            String alg = null;
            String peerHost = null;
            boolean handshakeSession = false;
            try {
                alg = engine.getSSLParameters().getEndpointIdentificationAlgorithm();
                peerHost = engine.getPeerHost();
                handshakeSession = engine.getHandshakeSession() != null;
            } catch (RuntimeException e) {
                System.out.println("@@TM engine-query-threw " + e);
            }
            System.out.println("@@TM overload=3-arg-SSLEngine authType=" + authType
                    + " engine=" + engine.getClass().getName()
                    + " endpointIdentificationAlgorithm=" + alg
                    + " peerHost=" + peerHost
                    + " handshakeSession=" + handshakeSession
                    + " sessionClass=" + describeSession(engine)
                    + " peerHostViaSession=" + peerHostViaSession(engine)
                    + " subject=" + (chain.length > 0
                            ? chain[0].getSubjectX500Principal().getName() : "<empty chain>"));
            // The DECIDING line: did the JDK trust manager throw, or did it
            // accept a certificate issued for a different host? Those are two
            // different defects — one in the VM's `HostnameChecker` inputs, one
            // in netty/BoringSSL losing the verifier's rejection — and they need
            // opposite fixes.
            try {
                delegate.checkServerTrusted(chain, authType, engine);
                System.out.println("@@TM delegate=ACCEPTED (no CertificateException)");
            } catch (CertificateException ce) {
                System.out.println("@@TM delegate=REJECTED " + ce.getClass().getName()
                        + ": " + ce.getMessage());
                throw ce;
            } catch (RuntimeException re) {
                System.out.println("@@TM delegate=THREW " + re.getClass().getName()
                        + ": " + re.getMessage());
                throw re;
            }
        }

        private static String describeSession(SSLEngine engine) {
            try {
                javax.net.ssl.SSLSession s = engine.getHandshakeSession();
                return s == null ? "null" : s.getClass().getName()
                        + (s instanceof javax.net.ssl.ExtendedSSLSession ? "(extended)" : "(plain)");
            } catch (RuntimeException e) {
                return "<threw " + e + ">";
            }
        }

        private static String peerHostViaSession(SSLEngine engine) {
            try {
                javax.net.ssl.SSLSession s = engine.getHandshakeSession();
                return s == null ? "<no session>" : String.valueOf(s.getPeerHost());
            } catch (RuntimeException e) {
                return "<threw " + e + ">";
            }
        }

        @Override
        public void checkClientTrusted(X509Certificate[] chain, String authType)
                throws CertificateException {
            delegate.checkClientTrusted(chain, authType);
        }

        @Override
        public void checkClientTrusted(X509Certificate[] chain, String authType, Socket socket)
                throws CertificateException {
            delegate.checkClientTrusted(chain, authType, socket);
        }

        @Override
        public void checkClientTrusted(X509Certificate[] chain, String authType, SSLEngine engine)
                throws CertificateException {
            delegate.checkClientTrusted(chain, authType, engine);
        }

        @Override
        public X509Certificate[] getAcceptedIssuers() {
            return delegate.getAcceptedIssuers();
        }
    }

    private static X509ExtendedTrustManager trustManagerFor(File caFile) throws Exception {
        CertificateFactory cf = CertificateFactory.getInstance("X.509");
        Collection<? extends java.security.cert.Certificate> certs;
        try (FileInputStream in = new FileInputStream(caFile)) {
            certs = cf.generateCertificates(in);
        }
        KeyStore ks = KeyStore.getInstance(KeyStore.getDefaultType());
        ks.load(null, null);
        int i = 0;
        for (java.security.cert.Certificate c : certs) {
            ks.setCertificateEntry("ca" + i++, c);
        }
        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ks);
        for (TrustManager tm : tmf.getTrustManagers()) {
            if (tm instanceof X509ExtendedTrustManager) {
                return (X509ExtendedTrustManager) tm;
            }
        }
        throw new IllegalStateException("no X509ExtendedTrustManager from the default factory");
    }

    public static void main(String[] args) throws Exception {
        SslProvider provider = args.length > 0 && "jdk".equals(args[0])
                ? SslProvider.JDK : SslProvider.OPENSSL;
        final String expectedHost = "localhost";

        X509ExtendedTrustManager real = trustManagerFor(res("mutual_auth_ca.pem"));
        System.out.println("@@ENV provider=" + provider
                + " jdkTrustManager=" + real.getClass().getName()
                + " isExtended=" + (real instanceof X509ExtendedTrustManager));
        Recording recording = new Recording(real);
        System.out.println("@@ENV recordingIsExtended=" + (recording instanceof X509ExtendedTrustManager)
                + " class=" + recording.getClass().getName());

        // The server presents a cert for a host that is NOT `localhost`.
        final SslContext serverCtx = SslContextBuilder
                .forServer(res("notlocalhost_server.pem"), res("notlocalhost_server.key"), null)
                .sslProvider(provider)
                .trustManager(InsecureTrustManagerFactory.INSTANCE)
                .ciphers(null, IdentityCipherSuiteFilter.INSTANCE)
                .sessionCacheSize(0).sessionTimeout(0).build();
        final SslContext clientCtx = SslContextBuilder.forClient()
                .sslProvider(provider)
                .trustManager(recording)
                .ciphers(null, IdentityCipherSuiteFilter.INSTANCE)
                .sessionCacheSize(0).sessionTimeout(0).build();

        final CountDownLatch clientLatch = new CountDownLatch(1);
        final Throwable[] clientOutcome = new Throwable[1];
        final boolean[] clientSucceeded = new boolean[1];

        MultiThreadIoEventLoopGroup serverGroup =
                new MultiThreadIoEventLoopGroup(NioIoHandler.newFactory());
        MultiThreadIoEventLoopGroup clientGroup =
                new MultiThreadIoEventLoopGroup(NioIoHandler.newFactory());
        Channel serverChannel = null;
        Channel clientChannel = null;
        try {
            ServerBootstrap sb = new ServerBootstrap();
            sb.group(serverGroup);
            sb.channel(NioServerSocketChannel.class);
            sb.childHandler(new ChannelInitializer<Channel>() {
                @Override
                protected void initChannel(Channel ch) {
                    ch.pipeline().addLast(serverCtx.newHandler(ch.alloc()));
                }
            });

            Bootstrap cb = new Bootstrap();
            cb.group(clientGroup);
            cb.channel(NioSocketChannel.class);
            cb.handler(new ChannelInitializer<Channel>() {
                @Override
                protected void initChannel(Channel ch) {
                    SslHandler sslHandler = clientCtx.newHandler(ch.alloc(), expectedHost, 0);
                    SSLParameters parameters = sslHandler.engine().getSSLParameters();
                    parameters.setEndpointIdentificationAlgorithm("HTTPS");
                    sslHandler.engine().setSSLParameters(parameters);
                    System.out.println("@@CLIENT engine=" + sslHandler.engine().getClass().getName()
                            + " readBackAlgorithm="
                            + sslHandler.engine().getSSLParameters()
                                    .getEndpointIdentificationAlgorithm());
                    ch.pipeline().addLast(sslHandler);
                    ch.pipeline().addLast(new ChannelInboundHandlerAdapter() {
                        @Override
                        public void userEventTriggered(ChannelHandlerContext ctx, Object evt) {
                            if (evt instanceof SslHandshakeCompletionEvent) {
                                SslHandshakeCompletionEvent e = (SslHandshakeCompletionEvent) evt;
                                clientSucceeded[0] = e.isSuccess();
                                clientOutcome[0] = e.cause();
                                clientLatch.countDown();
                            }
                            ctx.fireUserEventTriggered(evt);
                        }

                        @Override
                        public void exceptionCaught(ChannelHandlerContext ctx, Throwable cause) {
                            clientOutcome[0] = cause;
                            clientLatch.countDown();
                        }
                    });
                }
            });

            serverChannel = sb.bind(new InetSocketAddress(expectedHost, 0)).sync().channel();
            int port = ((InetSocketAddress) serverChannel.localAddress()).getPort();
            ChannelFuture ccf = cb.connect(new InetSocketAddress(expectedHost, port));
            ccf.awaitUninterruptibly();
            System.out.println("@@CONNECT success=" + ccf.isSuccess() + " cause=" + ccf.cause());
            clientChannel = ccf.channel();

            boolean fired = clientLatch.await(20, TimeUnit.SECONDS);
            System.out.println("@@HANDSHAKE eventFired=" + fired
                    + " succeeded=" + clientSucceeded[0]
                    + " cause=" + (clientOutcome[0] == null ? "null"
                            : clientOutcome[0].getClass().getName() + ": "
                                    + clientOutcome[0].getMessage()));
            // HotSpot's answer: the handshake FAILS, because the cert names a
            // host that is not `localhost`.
            System.out.println("@@VERDICT " + (clientSucceeded[0]
                    ? "FAIL hostname-verification-not-enforced"
                    : "PASS hostname-verification-enforced"));
            if (clientSucceeded[0]) {
                System.exit(1);
            }
        } finally {
            if (clientChannel != null) {
                clientChannel.close().awaitUninterruptibly();
            }
            if (serverChannel != null) {
                serverChannel.close().awaitUninterruptibly();
            }
            serverGroup.shutdownGracefully();
            clientGroup.shutdownGracefully();
        }
    }

    @SuppressWarnings("unused")
    private static void unusedImportAnchor(SSLSocket s) { }
}
