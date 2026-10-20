import java.io.*;
import java.net.*;
import java.nio.*;
import java.security.KeyStore;
import java.util.*;
import java.util.concurrent.*;
import javax.net.ssl.*;

/** Fresh-process HotSpot differential gate for retained SSL socket options and engine SNI. */
public class SslSocketSurface {
    static SSLContext context() throws Exception { return context("TLS"); }
    static SSLContext context(String algorithm) throws Exception { return context(algorithm, true); }
    static SSLContext context(String algorithm, boolean identity) throws Exception {
        KeyStore k = KeyStore.getInstance("PKCS12");
        try (InputStream in = new FileInputStream(System.getProperty("nio19.ks"))) {
            k.load(in, "changeit".toCharArray());
        }
        KeyManagerFactory km = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        km.init(k, "changeit".toCharArray());
        TrustManagerFactory tm = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tm.init(k);
        SSLContext ctx = SSLContext.getInstance(algorithm);
        ctx.init(identity ? km.getKeyManagers() : null, tm.getTrustManagers(), null);
        return ctx;
    }
    interface Call { Object run() throws Exception; }
    static String result(Call c) {
        try { return String.valueOf(c.run()); }
        catch (Exception e) { return e.getClass().getName() + ":" + e.getMessage(); }
    }
    static void options(SSLContext ctx, String proto, boolean before) throws Exception {
        try (SSLServerSocket ss = (SSLServerSocket) ctx.getServerSocketFactory().createServerSocket(0);
             SSLSocket c = (SSLSocket) ctx.getSocketFactory().createSocket()) {
            ss.setEnabledProtocols(new String[]{proto});
            ExecutorService ex = Executors.newSingleThreadExecutor();
            try {
                Future<String> peer = ex.submit(() -> {
                    try (SSLSocket s = (SSLSocket) ss.accept()) {
                        s.setSoTimeout(5000);
                        s.startHandshake();
                        int first = s.getInputStream().read();
                        s.getOutputStream().write(first);
                        s.getOutputStream().flush();
                        int eof = s.getInputStream().read();
                        return "enabled=" + Arrays.toString(s.getEnabledProtocols())
                                + "/local=" + (s.getLocalPort() == ss.getLocalPort())
                                + "/remote=" + (s.getPort() > 0) + "/mode=" + s.getUseClientMode()
                                + "/" + s.getSession().getProtocol() + ":" + first + ":" + eof;
                    }
                });
                String tag = proto + "/" + (before ? "before" : "after");
                if (!before) c.connect(new InetSocketAddress("localhost", ss.getLocalPort()));
                if (before) c.setEnabledProtocols(new String[]{proto});
                else { SSLParameters params = c.getSSLParameters(); params.setProtocols(new String[]{proto}); c.setSSLParameters(params); }
                c.setSoTimeout(1234);
                c.setTcpNoDelay(true);
                c.setKeepAlive(true);
                System.out.println(tag + " configured=" + Arrays.toString(c.getEnabledProtocols())
                        + "/" + Arrays.toString(c.getSSLParameters().getProtocols())
                        + "/" + c.getSoTimeout() + "/" + c.getTcpNoDelay() + "/" + c.getKeepAlive());
                if (before) c.connect(new InetSocketAddress("localhost", ss.getLocalPort()));
                System.out.println(tag + " connected=" + c.isConnected() + "/" + c.isBound()
                        + "/" + (c.getLocalPort() > 0) + "/" + (c.getPort() == ss.getLocalPort())
                        + "/" + c.getSoTimeout() + "/" + c.getTcpNoDelay() + "/" + c.getKeepAlive());
                System.out.println(tag + " local-address=" + c.getLocalAddress().isLoopbackAddress()
                        + "/" + (((InetSocketAddress)c.getLocalSocketAddress()).getPort() == c.getLocalPort()));
                c.startHandshake();
                c.getOutputStream().write(7);
                c.getOutputStream().flush();
                System.out.println(tag + " echo=" + c.getInputStream().read());
                c.setSoTimeout(100);
                System.out.println(tag + " read-timeout=" + result(() -> c.getInputStream().read()));
                c.setTcpNoDelay(false); c.setKeepAlive(false); c.setSoTimeout(0);
                System.out.println(tag + " reset=" + c.getSoTimeout() + "/" + c.getTcpNoDelay() + "/" + c.getKeepAlive());
                c.close(); c.close();
                System.out.println(tag + " peer=" + peer.get(10, TimeUnit.SECONDS));
                System.out.println(tag + " closed-options=" + result(() -> c.getSoTimeout())
                        + "/" + result(() -> c.getTcpNoDelay()) + "/" + result(() -> c.getKeepAlive()));
            } finally { ex.shutdownNow(); }
        }
    }
    static void closeWhileReading(SSLContext ctx) throws Exception {
        try (SSLServerSocket ss = (SSLServerSocket)ctx.getServerSocketFactory().createServerSocket(0)) {
            ExecutorService ex = Executors.newFixedThreadPool(2);
            try {
                Future<Integer> peer = ex.submit(() -> {
                    try (SSLSocket s = (SSLSocket)ss.accept()) {
                        s.setSoTimeout(5000); s.startHandshake();
                        return s.getInputStream().read();
                    }
                });
                SSLSocket c = (SSLSocket)ctx.getSocketFactory().createSocket("localhost", ss.getLocalPort());
                c.startHandshake();
                InputStream input = c.getInputStream();
                CountDownLatch started = new CountDownLatch(1);
                Future<String> reader = ex.submit(() -> {
                    started.countDown();
                    try { return "read:" + input.read(); }
                    catch (IOException e) { return e.getClass().getName(); }
                });
                started.await(); Thread.sleep(150);
                c.close();
                String terminal = reader.get(10, TimeUnit.SECONDS);
                // A concurrently closed local read may observe EOF or SocketException,
                // depending on whether JSSE drains the peer's response before TCP closes.
                if (!terminal.equals("read:-1") && !terminal.equals("java.net.SocketException")) throw new AssertionError(terminal);
                System.err.println("close-reading-terminal=" + terminal);
                System.out.println("close-reading=closed/" + peer.get(10, TimeUnit.SECONDS));
            } finally { ex.shutdownNow(); }
        }
    }
    static void layeredOptions(SSLContext ctx) throws Exception {
        try (SSLServerSocket ss = (SSLServerSocket)ctx.getServerSocketFactory().createServerSocket(0)) {
            ExecutorService ex = Executors.newSingleThreadExecutor();
            try {
                Future<Integer> peer = ex.submit(() -> {
                    try (SSLSocket s = (SSLSocket)ss.accept()) {s.setSoTimeout(5000);s.startHandshake();return s.getInputStream().read();}
                });
                try (Socket raw = new Socket("localhost", ss.getLocalPort())) {
                    raw.setSoTimeout(120); raw.setTcpNoDelay(true); raw.setKeepAlive(true);
                    try (SSLSocket c = (SSLSocket)ctx.getSocketFactory().createSocket(raw, "localhost", ss.getLocalPort(), true)) {
                        System.out.println("layered.before=" + c.getSoTimeout() + "/" + c.getTcpNoDelay() + "/" + c.getKeepAlive() + "/" + (c.getLocalPort() > 0));
                        c.startHandshake();
                        System.out.println("layered.after=" + c.getSoTimeout() + "/" + c.getTcpNoDelay() + "/" + c.getKeepAlive() + "/" + (c.getLocalPort() > 0)
                                + "/" + result(() -> c.getInputStream().read()));
                        c.setSoTimeout(0);
                    }
                }
                System.out.println("layered.peer=" + peer.get(10, TimeUnit.SECONDS));
            } finally { ex.shutdownNow(); }
        }
    }
    static void defaultTimeout() throws Exception {
        SSLContext serverContext = context(), clientContext = context("TLS", false);
        try (SSLServerSocket ss = (SSLServerSocket)serverContext.getServerSocketFactory().createServerSocket(0);
             SSLSocket c = (SSLSocket)clientContext.getSocketFactory().createSocket()) {
            ExecutorService ex = Executors.newSingleThreadExecutor();
            try {
                Future<Integer> peer = ex.submit(() -> {
                    try (SSLSocket s = (SSLSocket)ss.accept()) {s.setSoTimeout(5000);s.startHandshake();return s.getInputStream().read();}
                });
                c.setSoTimeout(120); c.connect(new InetSocketAddress("localhost", ss.getLocalPort())); c.startHandshake();
                System.out.println("default-timeout=" + c.getSoTimeout() + "/" + result(() -> c.getInputStream().read()));
                c.setSoTimeout(0); c.close();
                System.out.println("default-timeout.peer=" + peer.get(10, TimeUnit.SECONDS));
            } finally { ex.shutdownNow(); }
        }
    }
    static void closeStream(SSLContext ctx, boolean output) throws Exception {
        try (SSLServerSocket ss = (SSLServerSocket)ctx.getServerSocketFactory().createServerSocket(0)) {
            ExecutorService ex = Executors.newSingleThreadExecutor();
            try {
                Future<Integer> peer = ex.submit(() -> {
                    try (SSLSocket s = (SSLSocket)ss.accept()) { s.setSoTimeout(5000); s.startHandshake(); return s.getInputStream().read(); }
                });
                SSLSocket c = (SSLSocket)ctx.getSocketFactory().createSocket("localhost", ss.getLocalPort());
                c.startHandshake(); InputStream in = c.getInputStream(); OutputStream out = c.getOutputStream();
                if (output) out.close(); else in.close();
                System.out.println("stream-close " + output + "=" + c.isClosed() + "/" + c.isConnected() + "/" + c.isBound()
                        + "/" + peer.get(10, TimeUnit.SECONDS) + "/" + result(() -> in.read())
                        + "/" + result(() -> {out.write(1); return "accepted";})
                        + "/" + result(() -> in.read(new byte[1])) + "/" + result(() -> in.read(new byte[0]))
                        + "/" + result(() -> {out.write(new byte[]{1}); return "accepted";})
                        + "/" + result(() -> {out.write(new byte[0]); return "accepted";}));
                c.close();
            } finally { ex.shutdownNow(); }
        }
    }
    static void sealedIdentity() throws Exception {
        SSLContext a = context(), b = context("TLS", false);
        SSLSocketFactory factory = a.getSocketFactory();
        try (SSLServerSocket ss = (SSLServerSocket)a.getServerSocketFactory().createServerSocket(0);
             SSLSocket c = (SSLSocket)factory.createSocket()) {
            ss.setNeedClientAuth(true); ss.setEnabledProtocols(new String[]{"TLSv1.3"});
            b.getSocketFactory(); // A deferred socket must retain A's identity after B becomes current.
            ExecutorService ex = Executors.newSingleThreadExecutor();
            try {
                Future<String> peer = ex.submit(() -> {
                    try (SSLSocket s = (SSLSocket)ss.accept()) {
                        s.setSoTimeout(5000); s.startHandshake();
                        int count = s.getSession().getPeerCertificates().length;
                        int data = s.getInputStream().read(), eof = s.getInputStream().read();
                        return count + "/" + data + "/" + eof;
                    }
                });
                c.setEnabledProtocols(new String[]{"TLSv1.3"}); c.setSoTimeout(5000);
                c.connect(new InetSocketAddress("localhost", ss.getLocalPort())); c.startHandshake();
                System.out.println("sealed-identity.local=" + c.getSession().getLocalCertificates().length);
                c.getOutputStream().write(11); c.close();
                System.out.println("sealed-identity.peer=" + peer.get(10, TimeUnit.SECONDS));
            } finally { ex.shutdownNow(); }
        }
    }
    static void sni(SSLContext ctx, String host, String explicit) throws Exception {
        ctx = context();
        SSLEngine c = ctx.createSSLEngine(host, 4433), s = ctx.createSSLEngine();
        c.setUseClientMode(true); s.setUseClientMode(false);
        if (explicit != null) {
            SSLParameters p = c.getSSLParameters();
            p.setServerNames(explicit.isEmpty() ? Collections.emptyList() : List.of(new SNIHostName(explicit)));
            c.setSSLParameters(p);
        }
        List<SNIServerName> configured = c.getSSLParameters().getServerNames();
        List<String> configNames = configured == null ? null : new ArrayList<>();
        if (configured != null) for (SNIServerName n : configured) configNames.add(((SNIHostName)n).getAsciiName());
        System.out.println("sni-config " + host + "/" + explicit + " names=" + configNames);
        ByteBuffer c2s = ByteBuffer.allocate(65536), s2c = ByteBuffer.allocate(65536);
        ByteBuffer empty = ByteBuffer.allocate(0), sink = ByteBuffer.allocate(65536);
        c.beginHandshake(); s.beginHandshake();
        for (int i = 0; i < 500; i++) {
            for (SSLEngine e : new SSLEngine[]{c, s}) {
                var hs = e.getHandshakeStatus();
                if (hs == SSLEngineResult.HandshakeStatus.NEED_TASK) {
                    Runnable r; while ((r = e.getDelegatedTask()) != null) r.run();
                } else if (hs == SSLEngineResult.HandshakeStatus.NEED_WRAP) {
                    e.wrap(empty, e == c ? c2s : s2c);
                } else if (hs == SSLEngineResult.HandshakeStatus.NEED_UNWRAP) {
                    ByteBuffer in = e == c ? s2c : c2s;
                    in.flip(); if (in.hasRemaining()) e.unwrap(in, sink); in.compact(); sink.clear();
                }
            }
            if (c.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING
                    && s.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING) break;
        }
        if (!c.getSession().isValid() || !s.getSession().isValid()) throw new AssertionError("handshake incomplete");
        List<String> names = new ArrayList<>();
        for (SNIServerName n : ((ExtendedSSLSession)s.getSession()).getRequestedServerNames())
            names.add(((SNIHostName)n).getAsciiName());
        System.out.println("sni " + host + "/" + explicit + " names=" + names
                + " peer=" + c.getPeerHost() + "/" + c.getSession().getPeerHost());
        c.closeOutbound(); s.closeOutbound();
    }
    public static void main(String[] args) throws Exception {
        SSLContext ctx = context();
        try (SSLSocket u = (SSLSocket)ctx.getSocketFactory().createSocket()) {
            System.out.println("fresh=" + u.getTcpNoDelay() + "/" + u.getKeepAlive() + "/" + u.getSoTimeout()
                    + "/" + u.getLocalPort() + "/" + u.getLocalAddress().isAnyLocalAddress() + "/" + u.getLocalSocketAddress());
            System.out.println("negative-timeout=" + result(() -> {u.setSoTimeout(-1); return "accepted";}));
            System.out.println("null-protocols=" + result(() -> {u.setEnabledProtocols(null); return "accepted";}));
            System.out.println("bad-protocol=" + result(() -> {u.setEnabledProtocols(new String[]{"bad"}); return "accepted";}));
        }
        try (SSLSocket pinned = (SSLSocket)context("TLSv1.2").getSocketFactory().createSocket()) {
            System.out.println("pinned-unconnected=" + Arrays.toString(pinned.getEnabledProtocols())
                    + "/" + Arrays.toString(pinned.getSSLParameters().getProtocols()));
        }
        try (ServerSocket plain = new ServerSocket(0);
             SSLSocket empty = (SSLSocket)ctx.getSocketFactory().createSocket()) {
            String[] protocols = {"TLSv1.3"}; empty.setEnabledProtocols(protocols); protocols[0] = "TLSv1.2";
            System.out.println("protocol-copy=" + Arrays.toString(empty.getEnabledProtocols()));
            empty.setEnabledProtocols(new String[0]);
            System.out.println("empty-protocols=" + Arrays.toString(empty.getEnabledProtocols()));
            empty.connect(new InetSocketAddress("localhost", plain.getLocalPort()));
            System.out.println("empty-handshake=" + result(() -> {empty.startHandshake(); return "accepted";}));
        }
        for (String p : new String[]{"TLSv1.3", "TLSv1.2"}) for (boolean before : new boolean[]{true, false}) options(ctx, p, before);
        layeredOptions(ctx);
        defaultTimeout();
        closeStream(ctx, false); closeStream(ctx, true);
        sealedIdentity();
        closeWhileReading(ctx);
        sni(ctx, "localhost", null);
        sni(ctx, "localhost", "localhost");
        sni(ctx, "www.example.com", null);
        sni(ctx, "www.example.com", "");
        sni(ctx, "www.example.com", "other.example.com");
        sni(ctx, "bad_host.example.com", null);
        sni(ctx, "www.example.com.", null);
        sni(ctx, "b\u00fccher.example", null);
        sni(ctx, "127.0.0.1", null);
        sni(ctx, "127.0.0.1", "localhost");
        System.out.println("SSL_SOCKET_SURFACE_OK");
    }
}
