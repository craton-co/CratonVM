import io.netty.bootstrap.Bootstrap;
import io.netty.bootstrap.ServerBootstrap;
import io.netty.channel.Channel;
import io.netty.channel.ChannelFuture;
import io.netty.channel.ChannelInitializer;
import io.netty.channel.ChannelPipeline;
import io.netty.channel.MultiThreadIoEventLoopGroup;
import io.netty.channel.nio.NioIoHandler;
import io.netty.channel.socket.nio.NioServerSocketChannel;
import io.netty.channel.socket.nio.NioSocketChannel;
import io.netty.handler.ssl.IdentityCipherSuiteFilter;
import io.netty.handler.ssl.SslContext;
import io.netty.handler.ssl.SslContextBuilder;
import io.netty.handler.ssl.SslHandler;
import io.netty.handler.ssl.SslProvider;
import io.netty.util.NetUtil;

import javax.net.ssl.SSLEngine;
import java.io.File;
import java.net.InetSocketAddress;

/**
 * `SSLEngineTest.mySetupMutualAuth` reduced to the one assertion that fails on
 * CratonVM and passes on HotSpot:
 *
 *     ChannelFuture ccf = cb.connect(new InetSocketAddress(NetUtil.LOCALHOST, port));
 *     assertTrue(ccf.awaitUninterruptibly().isSuccess());          // SSLEngineTest.java:1302
 *
 * `assertTrue` throws the future's cause away, so the netty suite reports this
 * as `expected: <true> but was: <false>` and nothing else. This probe prints
 * `ccf.cause()`, and runs the three trust/key configurations that
 * `testMutualAuthDiffCerts`, `testMutualAuthDiffCertsServerFailure` and
 * `testMutualAuthDiffCertsClientFailure` set up, so "which configuration breaks
 * the connect" is a measurement rather than a guess.
 *
 * Needs netty's handler test-classes on the classpath (for the `test*.crt` /
 * `test*.pem` resources), and, for the `openssl` arm, a classpath on which
 * `OpenSsl.isAvailable()` is true — see `apps/netty-suite-runner/gen-openssl-args.sh`.
 *
 * usage: SslMutualAuthConnectProbe [jdk|openssl] [rounds]
 */
public final class SslMutualAuthConnectProbe {
    private SslMutualAuthConnectProbe() {}

    private static final String RES = "io/netty/handler/ssl/";

    private static File res(String name) {
        java.net.URL u = SslMutualAuthConnectProbe.class.getClassLoader().getResource(RES + name);
        if (u == null) {
            throw new IllegalStateException("resource not on the classpath: " + RES + name);
        }
        return new File(u.getFile().replaceFirst("^/([A-Za-z]:)", "$1"));
    }

    public static void main(String[] args) throws Exception {
        SslProvider provider = args.length > 0 && "jdk".equals(args[0])
                ? SslProvider.JDK : SslProvider.OPENSSL;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 1;

        System.out.println("@@NETUTIL LOCALHOST=" + NetUtil.LOCALHOST
                + " class=" + NetUtil.LOCALHOST.getClass().getName()
                + " LOCALHOST4=" + NetUtil.LOCALHOST4
                + " LOCALHOST6=" + NetUtil.LOCALHOST6
                + " isIpV4StackPreferred=" + NetUtil.isIpV4StackPreferred()
                + " isIpV6AddressesPreferred=" + NetUtil.isIpV6AddressesPreferred());
        if (NetUtil.LOCALHOST instanceof java.net.Inet6Address) {
            java.net.Inet6Address a6 = (java.net.Inet6Address) NetUtil.LOCALHOST;
            System.out.println("@@NETUTIL LOCALHOST6-detail scopeId=" + a6.getScopeId()
                    + " scopedIface=" + a6.getScopedInterface()
                    + " isLoopback=" + a6.isLoopbackAddress());
        }
        for (java.util.Enumeration<java.net.NetworkInterface> e =
                java.net.NetworkInterface.getNetworkInterfaces(); e.hasMoreElements();) {
            java.net.NetworkInterface ni = e.nextElement();
            System.out.println("@@IFACE name=" + ni.getName() + " idx=" + ni.getIndex()
                    + " loopback=" + ni.isLoopback() + " up=" + ni.isUp()
                    + " display=" + ni.getDisplayName());
            for (java.util.Enumeration<java.net.InetAddress> a = ni.getInetAddresses();
                    a.hasMoreElements();) {
                System.out.println("@@IFACE-ADDR   " + a.nextElement());
            }
        }

        File testCrt = res("test.crt");
        File testEnc = res("test_encrypted.pem");
        File testUnenc = res("test_unencrypted.pem");
        File test2Crt = res("test2.crt");
        File test2Enc = res("test2_encrypted.pem");
        File test2Unenc = res("test2_unencrypted.pem");

        for (int i = 0; i < rounds; i++) {
            // testMutualAuthDiffCerts: server trusts the CLIENT cert, encrypted keys.
            run(provider, i, "diffCerts", test2Crt, testEnc, testCrt, "12345",
                    testCrt, test2Enc, test2Crt, "12345");
            // testMutualAuthDiffCertsServerFailure: server trusts ITSELF, encrypted keys.
            run(provider, i, "serverFailure", testCrt, testEnc, testCrt, "12345",
                    testCrt, test2Enc, test2Crt, "12345");
            // testMutualAuthDiffCertsClientFailure: client trusts ITSELF, unencrypted keys.
            run(provider, i, "clientFailure", test2Crt, testUnenc, testCrt, null,
                    test2Crt, test2Unenc, test2Crt, null);
        }
    }

    private static void run(SslProvider provider, int round, String label,
            File serverTrustCrt, File serverKey, File serverCrt, String serverKeyPassword,
            File clientTrustCrt, File clientKey, File clientCrt, String clientKeyPassword)
            throws Exception {
        final SslContext serverCtx = SslContextBuilder.forServer(serverCrt, serverKey, serverKeyPassword)
                .sslProvider(provider)
                .trustManager(serverTrustCrt)
                .ciphers(null, IdentityCipherSuiteFilter.INSTANCE)
                .sessionCacheSize(0)
                .sessionTimeout(0).build();
        final SslContext clientCtx = SslContextBuilder.forClient()
                .sslProvider(provider)
                .trustManager(clientTrustCrt)
                .keyManager(clientCrt, clientKey, clientKeyPassword)
                .ciphers(null, IdentityCipherSuiteFilter.INSTANCE)
                .sessionCacheSize(0)
                .endpointIdentificationAlgorithm(null)
                .sessionTimeout(0).build();

        MultiThreadIoEventLoopGroup serverGroup = new MultiThreadIoEventLoopGroup(NioIoHandler.newFactory());
        MultiThreadIoEventLoopGroup clientGroup = new MultiThreadIoEventLoopGroup(NioIoHandler.newFactory());
        Channel serverChannel = null;
        Channel clientChannel = null;
        try {
            ServerBootstrap sb = new ServerBootstrap();
            sb.group(serverGroup);
            sb.channel(NioServerSocketChannel.class);
            sb.childHandler(new ChannelInitializer<Channel>() {
                @Override
                protected void initChannel(Channel ch) {
                    ChannelPipeline p = ch.pipeline();
                    SSLEngine engine = serverCtx.newEngine(ch.alloc());
                    engine.setUseClientMode(false);
                    engine.setNeedClientAuth(true);
                    p.addLast(new SslHandler(engine));
                }
            });

            Bootstrap cb = new Bootstrap();
            cb.group(clientGroup);
            cb.channel(NioSocketChannel.class);
            cb.handler(new ChannelInitializer<Channel>() {
                @Override
                protected void initChannel(Channel ch) {
                    SslHandler handler = clientCtx.newHandler(ch.alloc());
                    handler.engine().setNeedClientAuth(true);
                    ch.pipeline().addLast(handler);
                }
            });

            serverChannel = sb.bind(new InetSocketAddress(0)).sync().channel();
            int port = ((InetSocketAddress) serverChannel.localAddress()).getPort();

            ChannelFuture ccf = cb.connect(new InetSocketAddress(NetUtil.LOCALHOST, port));
            ccf.awaitUninterruptibly();
            clientChannel = ccf.channel();
            System.out.println("@@CONNECT round=" + round + " provider=" + provider
                    + " case=" + label + " success=" + ccf.isSuccess()
                    + " serverLocal=" + serverChannel.localAddress()
                    + " serverClass=" + ((InetSocketAddress) serverChannel.localAddress())
                            .getAddress().getClass().getSimpleName()
                    + " remote=" + NetUtil.LOCALHOST + ":" + port);
            if (!ccf.isSuccess()) {
                Throwable cause = ccf.cause();
                System.out.println("@@CONNECT-CAUSE " + label + " "
                        + (cause == null ? "<null cause>" : cause.getClass().getName()
                                + ": " + cause.getMessage()));
                if (cause != null) {
                    cause.printStackTrace(System.out);
                }
            }
            System.out.flush();
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
}
