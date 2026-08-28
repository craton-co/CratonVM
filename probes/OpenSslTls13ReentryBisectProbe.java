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
import javax.net.ssl.X509ExtendedKeyManager;
import javax.net.ssl.KeyManager;
import javax.net.ssl.KeyManagerFactory;
import java.io.FileInputStream;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.security.KeyStore;
import java.security.Principal;
import java.security.PrivateKey;
import java.security.cert.CertificateException;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.util.Arrays;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Which re-entrant call from inside BoringSSL's verify callback loses the
 * TLSv1.3 client certificate?
 *
 * `known-issues/netty/java-reentry-from-boringssl-verify-callback-loses-the-tls13-client-cert-20260826.md`
 * establishes THAT `engine.getSSLParameters()` from inside `checkServerTrusted`
 * loses the client `Certificate` flight when `useTasks=false`, and lists as its
 * first unanswered question WHICH part of that call does it — because
 * `getSSLParameters()` is at least four different hazards at once:
 *
 *   - it takes the engine's monitor (`synchronized`);
 *   - it calls `SSL.getOptions(ssl)` — a tcnative native on the live `SSL*`;
 *   - it calls `SSL.getCiphers(ssl)` through `getEnabledCipherSuites()`;
 *   - it runs ordinary Java that allocates, so it can provoke a collection.
 *
 * A lock-ordering problem, a BoringSSL state-machine problem and a
 * GC-moved-something-the-outer-native-holds problem need three different fixes,
 * so the page is explicit that guessing between them is not allowed.
 *
 * This probe runs ONE handshake per (action, useTasks) pair and varies only what
 * the trust manager does on the callback. Each action is a strict subset or
 * superset of the others, so the table reads as a bisection rather than a list:
 *
 *   none                 control -- the delegate only, nothing re-entrant
 *   sync                 `synchronized (engine) {}` -- the monitor, no native
 *   javaOnly             a Java call on the engine that reaches no native
 *   alloc                allocate ~64 MiB of garbage -- provokes a collection,
 *                        takes no lock, calls no native
 *   gc                   `System.gc()` -- the same hypothesis, harder
 *   sslGetOptions        `SSL.getOptions(ssl)` alone, reflectively
 *   sslGetCiphers        `SSL.getCiphers(ssl)` alone, reflectively
 *   sslGetVersion        a third read-only native on the same `SSL*`
 *   sslGetLastErrorNumber a tcnative native that touches NO `SSL*`
 *   sslNewMemBIO         a tcnative native that runs BoringSSL but no `SSL*`
 *   otherSslGetOptions   `SSL.getOptions` on a DIFFERENT, idle `SSL*`
 *   enabledCipherSuites  netty Java around `SSL.getCiphers`, no monitor
 *   getSSLParameters     the known reproducer, all four hazards at once
 *
 * A COUNTING KEY MANAGER on the client answers the page's second question at
 * the same time, and for free: `chooseClientAlias`/`getCertificateChain` counts
 * say whether the client was ever ASKED for its certificate. Zero asks means
 * the loss is on the decide-to-send side (BoringSSL never ran the cert
 * callback); a normal ask with a lost flight means it is on the emit side.
 *
 * Read the output as a table:
 *   @@BISECT action=<name> useTasks=<b> result=<PASS|FAIL> keyAsks=<n> chainAsks=<n> ...
 *
 * HotSpot is the oracle: every row must PASS there. Any row that FAILs on
 * HotSpot is a bug in this probe, not in the VM.
 *
 * usage: OpenSslTls13ReentryBisectProbe [rounds]
 */
public final class OpenSslTls13ReentryBisectProbe {
    private OpenSslTls13ReentryBisectProbe() {}

    /** What the trust manager does before delegating, on the callback. */
    enum Action {
        NONE,
        SYNC,
        JAVA_ONLY,
        ALLOC,
        GC,
        SSL_GET_OPTIONS,
        SSL_GET_CIPHERS,
        SSL_GET_VERSION,
        SSL_GET_LAST_ERROR_NUMBER,
        SSL_NEW_MEM_BIO,
        OTHER_SSL_GET_OPTIONS,
        ENABLED_CIPHER_SUITES,
        GET_SSL_PARAMETERS,
    }

    /** Kept alive across the callback so `ALLOC` cannot be optimised away. */
    private static volatile Object allocSink;

    /**
     * An idle `SSL*` from an engine that is NOT in a handshake, and the objects
     * that keep it alive.
     *
     * `OTHER_SSL_GET_OPTIONS` calls the same tcnative native on this pointer
     * from inside the callback. Same nesting, same library, same function — only
     * the `SSL*` differs, which is the one axis that separates "BoringSSL state
     * on the SSL BoringSSL is currently inside" from "any nested call into
     * tcnative at all".
     */
    private static long otherSsl;
    private static SslContext otherCtx;
    private static SSLEngine otherEngine;

    private static int rowsFailed;
    private static int rowsRun;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        SelfSignedCertificate serverCert = new SelfSignedCertificate();
        SelfSignedCertificate clientCert = new SelfSignedCertificate();

        // Built once, before any handshake, and never used for one. Both
        // references are held for the life of the process so the engine is not
        // finalized and the pointer stays valid.
        otherCtx = SslContextBuilder.forClient()
                .trustManager(serverCert.cert())
                .sslProvider(SslProvider.OPENSSL)
                .protocols("TLSv1.3")
                .build();
        otherEngine = otherCtx.newEngine(io.netty.buffer.UnpooledByteBufAllocator.DEFAULT);
        otherSsl = sslPointer(otherEngine);
        System.out.println("@@BISECT-SETUP idleSsl=" + (otherSsl != 0));

        for (int r = 0; r < rounds; r++) {
            for (Action action : Action.values()) {
                // `useTasks=true` is the control on every row: netty defers the
                // callback out of `SSL_do_handshake`, so the same Java runs with
                // no native on the stack beneath it. A row that fails in BOTH
                // arms is not this page's defect.
                for (boolean useTasks : new boolean[] {false, true}) {
                    run(r, serverCert, clientCert, action, useTasks);
                }
            }
        }
        System.out.println("@@BISECT-SUMMARY rows=" + rowsRun + " failed=" + rowsFailed
                + " (HotSpot: failed=0)");
        System.out.flush();
    }

    private static void run(int round, SelfSignedCertificate serverCert,
            SelfSignedCertificate clientCert, Action action, boolean useTasks) throws Exception {
        final String host = "127.0.0.1";
        final String cipher = "TLS_AES_128_GCM_SHA256";

        SslContext serverCtx = SslContextBuilder
                .forServer(serverCert.certificate(), serverCert.privateKey())
                .trustManager(clientCert.cert())
                .clientAuth(ClientAuth.REQUIRE)
                .sslProvider(SslProvider.OPENSSL)
                .protocols("TLSv1.3")
                .ciphers(Arrays.asList(cipher), IdentityCipherSuiteFilter.INSTANCE)
                .build();

        CountingKeyManager km = new CountingKeyManager(
                keyManagerFor(clientCert.certificate(), clientCert.privateKey()));
        SslContext clientCtx = SslContextBuilder.forClient()
                .keyManager(km)
                .trustManager(new BisectTrustManager(trustManagerFor(serverCert.certificate()), action))
                .sslProvider(SslProvider.OPENSSL)
                .protocols("TLSv1.3")
                .ciphers(Arrays.asList(cipher), IdentityCipherSuiteFilter.INSTANCE)
                .endpointIdentificationAlgorithm(null)
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
            server = sb.bind(new InetSocketAddress(InetAddress.getByName(host), 0)).sync().channel();
            final int port = ((InetSocketAddress) server.localAddress()).getPort();

            Bootstrap cb = new Bootstrap();
            cb.group(cg).channel(NioSocketChannel.class);
            final SslContext cctx = clientCtx;
            cb.handler(new ChannelInitializer<Channel>() {
                @Override
                protected void initChannel(Channel ch) {
                    ch.pipeline().addLast(cctx.newHandler(ch.alloc()));
                    ch.pipeline().addLast(new Recorder(clientDone, clientOk, clientCause));
                }
            });
            ChannelFuture ccf = cb.connect(new InetSocketAddress(InetAddress.getByName(host), port));
            ccf.awaitUninterruptibly();
            client = ccf.channel();

            boolean fired = clientDone.await(20, TimeUnit.SECONDS)
                    & serverDone.await(20, TimeUnit.SECONDS);
            boolean ok = ccf.isSuccess() && fired && clientOk[0] && serverOk[0];
            rowsRun++;
            if (!ok) {
                rowsFailed++;
            }
            System.out.println("@@BISECT " + (ok ? "PASS" : "FAIL")
                    + " action=" + action
                    + " useTasks=" + useTasks
                    + " round=" + round
                    + " reenterCalls=" + BisectTrustManager.calls.get()
                    + " reenterThrew=" + BisectTrustManager.threw.get()
                    + " keyAsks=" + km.aliasAsks.get()
                    + " chainAsks=" + km.chainAsks.get()
                    + " clientHandshake=" + clientOk[0]
                    + " serverHandshake=" + serverOk[0]
                    + " clientCause=" + brief(clientCause[0])
                    + " serverCause=" + brief(serverCause[0]));
            System.out.flush();
        } finally {
            BisectTrustManager.calls.set(0);
            BisectTrustManager.threw.set(0);
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

    // ---- the trust manager under test --------------------------------------

    static final class BisectTrustManager extends X509ExtendedTrustManager {
        static final AtomicInteger calls = new AtomicInteger();
        static final AtomicInteger threw = new AtomicInteger();

        private final X509ExtendedTrustManager delegate;
        private final Action action;

        BisectTrustManager(X509ExtendedTrustManager delegate, Action action) {
            this.delegate = delegate;
            this.action = action;
        }

        /**
         * The experiment. Every arm's RESULT is discarded — what is varied is
         * only what runs, on which stack.
         */
        private void reenter(SSLEngine engine) {
            if (action == Action.NONE) {
                return;
            }
            calls.incrementAndGet();
            try {
                switch (action) {
                    case SYNC:
                        // The monitor, and nothing else. `getSSLParameters()` is
                        // `synchronized` on this same object, so if the defect is
                        // lock ordering rather than native re-entry, this row is
                        // the one that fails.
                        synchronized (engine) {
                            allocSink = engine;
                        }
                        break;
                    case JAVA_ONLY:
                        // Java on the engine that reaches no native and takes no
                        // monitor: `getUseClientMode` is a plain field read.
                        allocSink = Boolean.valueOf(engine.getUseClientMode());
                        break;
                    case ALLOC:
                        // No lock, no native — only allocation. If THIS row
                        // fails, the mechanism is a collection moving something
                        // the outer tcnative frame still points at, and every
                        // native-re-entry theory is wrong.
                        allocSink = churn();
                        break;
                    case GC:
                        System.gc();
                        break;
                    case SSL_GET_OPTIONS:
                        allocSink = Integer.valueOf(sslGetOptions(engine));
                        break;
                    case SSL_GET_CIPHERS:
                        allocSink = sslGetCiphers(engine);
                        break;
                    case SSL_GET_VERSION:
                        // A third read-only native on the SAME `SSL*`. Two is a
                        // pair; three is a rule.
                        allocSink = sslStatic("getVersion", long.class)
                                .invoke(null, Long.valueOf(sslPointer(engine)));
                        break;
                    case SSL_GET_LAST_ERROR_NUMBER:
                        // A tcnative native that touches NO `SSL*` at all --
                        // it reads OpenSSL's thread-local error queue. If this
                        // row fails, the hazard is "any nested call into the
                        // tcnative library"; if it passes, the hazard needs the
                        // live `SSL*`.
                        allocSink = sslStatic("getLastErrorNumber").invoke(null);
                        break;
                    case SSL_NEW_MEM_BIO:
                        // A tcnative native that ALLOCATES inside BoringSSL but
                        // touches no `SSL*`: it separates "reads the SSL" from
                        // "runs BoringSSL code at all".
                        allocSink = sslStatic("newMemBIO").invoke(null);
                        break;
                    case OTHER_SSL_GET_OPTIONS:
                        // The same native, on a DIFFERENT, idle `SSL*` created
                        // before the handshake started. Same nesting, same
                        // library, same function -- only the pointer differs.
                        // This is what separates "BoringSSL state on the SSL
                        // BoringSSL is inside" from "any nested tcnative call".
                        allocSink = Integer.valueOf((Integer) sslStatic("getOptions", long.class)
                                .invoke(null, Long.valueOf(otherSsl)));
                        break;
                    case ENABLED_CIPHER_SUITES:
                        allocSink = engine.getEnabledCipherSuites();
                        break;
                    case GET_SSL_PARAMETERS:
                        allocSink = engine.getSSLParameters().getEndpointIdentificationAlgorithm();
                        break;
                    default:
                        break;
                }
            } catch (Throwable ignored) {
                // The call being MADE is the experiment; whether it answered is
                // not. Counted so a row that silently did nothing is visible.
                threw.incrementAndGet();
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

    /** ~64 MiB of short-lived garbage, enough to force at least one collection. */
    private static Object churn() {
        Object last = null;
        for (int i = 0; i < 1024; i++) {
            last = new byte[64 * 1024];
        }
        return last;
    }

    // ---- reflective access to the live `SSL*` -------------------------------
    //
    // `ReferenceCountedOpenSslEngine.ssl` is a private long. Reading it and
    // calling `SSL.getOptions`/`SSL.getCiphers` directly is what separates
    // "a tcnative native on this SSL*" from "netty's Java around it".

    private static long sslPointer(SSLEngine engine) throws Exception {
        Class<?> c = engine.getClass();
        while (c != null) {
            try {
                Field f = c.getDeclaredField("ssl");
                f.setAccessible(true);
                return f.getLong(engine);
            } catch (NoSuchFieldException e) {
                c = c.getSuperclass();
            }
        }
        throw new NoSuchFieldException("ssl");
    }

    private static Method sslStatic(String name, Class<?>... params) throws Exception {
        Class<?> ssl = Class.forName("io.netty.internal.tcnative.SSL");
        Method m = ssl.getDeclaredMethod(name, params);
        m.setAccessible(true);
        return m;
    }

    private static int sslGetOptions(SSLEngine engine) throws Exception {
        long p = sslPointer(engine);
        return (Integer) sslStatic("getOptions", long.class).invoke(null, p);
    }

    private static Object sslGetCiphers(SSLEngine engine) throws Exception {
        long p = sslPointer(engine);
        return sslStatic("getCiphers", long.class).invoke(null, p);
    }

    // ---- a key manager that counts what it was asked for --------------------

    /**
     * Answers the page's "send side or receive side?" question without a packet
     * capture: if the client is never asked for an alias or a chain, BoringSSL
     * never ran the certificate callback and the flight was lost BEFORE any
     * emission; if it is asked normally and the server still sees nothing, the
     * loss is downstream of the decision.
     */
    static final class CountingKeyManager extends X509ExtendedKeyManager {
        final AtomicInteger aliasAsks = new AtomicInteger();
        final AtomicInteger chainAsks = new AtomicInteger();
        private final X509ExtendedKeyManager delegate;

        CountingKeyManager(X509ExtendedKeyManager delegate) {
            this.delegate = delegate;
        }

        @Override
        public String chooseEngineClientAlias(String[] keyType, Principal[] issuers, SSLEngine e) {
            aliasAsks.incrementAndGet();
            return delegate.chooseEngineClientAlias(keyType, issuers, e);
        }

        @Override
        public String chooseEngineServerAlias(String keyType, Principal[] issuers, SSLEngine e) {
            return delegate.chooseEngineServerAlias(keyType, issuers, e);
        }

        @Override
        public String chooseClientAlias(String[] keyType, Principal[] issuers, Socket s) {
            aliasAsks.incrementAndGet();
            return delegate.chooseClientAlias(keyType, issuers, s);
        }

        @Override
        public String chooseServerAlias(String keyType, Principal[] issuers, Socket s) {
            return delegate.chooseServerAlias(keyType, issuers, s);
        }

        @Override
        public X509Certificate[] getCertificateChain(String alias) {
            chainAsks.incrementAndGet();
            return delegate.getCertificateChain(alias);
        }

        @Override
        public String[] getClientAliases(String keyType, Principal[] issuers) {
            return delegate.getClientAliases(keyType, issuers);
        }

        @Override
        public String[] getServerAliases(String keyType, Principal[] issuers) {
            return delegate.getServerAliases(keyType, issuers);
        }

        @Override
        public PrivateKey getPrivateKey(String alias) {
            return delegate.getPrivateKey(alias);
        }
    }

    private static X509ExtendedKeyManager keyManagerFor(java.io.File certFile, java.io.File keyFile)
            throws Exception {
        // Build a keystore the same way `SslContextBuilder.keyManager(File,File)`
        // would, so the delegate is the ordinary SunJSSE key manager.
        CertificateFactory cf = CertificateFactory.getInstance("X.509");
        java.util.List<java.security.cert.Certificate> chain = new java.util.ArrayList<>();
        try (FileInputStream in = new FileInputStream(certFile)) {
            chain.addAll(cf.generateCertificates(in));
        }
        PrivateKey key = readPkcs8(keyFile);
        KeyStore ks = KeyStore.getInstance(KeyStore.getDefaultType());
        ks.load(null, null);
        ks.setKeyEntry("client", key, new char[0],
                chain.toArray(new java.security.cert.Certificate[0]));
        KeyManagerFactory kmf =
                KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, new char[0]);
        for (KeyManager m : kmf.getKeyManagers()) {
            if (m instanceof X509ExtendedKeyManager) {
                return (X509ExtendedKeyManager) m;
            }
        }
        throw new IllegalStateException("no X509ExtendedKeyManager");
    }

    private static PrivateKey readPkcs8(java.io.File keyFile) throws Exception {
        byte[] all = java.nio.file.Files.readAllBytes(keyFile.toPath());
        String pem = new String(all, java.nio.charset.StandardCharsets.US_ASCII)
                .replace("-----BEGIN PRIVATE KEY-----", "")
                .replace("-----END PRIVATE KEY-----", "")
                .replaceAll("\\s", "");
        byte[] der = java.util.Base64.getDecoder().decode(pem);
        return java.security.KeyFactory.getInstance("RSA")
                .generatePrivate(new java.security.spec.PKCS8EncodedKeySpec(der));
    }

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
