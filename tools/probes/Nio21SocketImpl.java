import java.io.*;
import java.lang.reflect.Modifier;
import java.net.*;
import java.security.KeyStore;
import java.util.*;
import java.util.concurrent.*;
import javax.net.ssl.*;

/**
 * The JSSE socket and factory objects a program is handed, probed the way a program probes them: class and
 * ancestry, the casts to the JDK's own implementation classes, reflection over the declared fields, the
 * accessors on a client and a server socket after a real loopback handshake, and a TLS echo through them.
 * HotSpot hands out sun.security.ssl.SSLSocketFactoryImpl / SSLServerSocketFactoryImpl / SSLSocketImpl /
 * SSLServerSocketImpl. Run: -Dnio19.ks=pkcs12 (password "changeit", the key's own cert is trusted). Page:
 * compatible-tls-and-http-carriers-are-instances-of-abstract-classes-FIXED-20261010.md.
 */
public class Nio21SocketImpl {
    interface T { Object get() throws Throwable; }

    static String f(T t) {
        try {
            return String.valueOf(t.get());
        } catch (Throwable x) {
            return "!" + x.getClass().getName();
        }
    }

    static void shape(String tag, Object o, String impl, Class<?> pub) {
        Class<?> c = o.getClass();
        System.out.println(tag + " class=" + c.getName());
        System.out.println(tag + " super=" + c.getSuperclass().getName() + " final=" + Modifier.isFinal(c.getModifiers())
                + " abstract=" + Modifier.isAbstract(c.getModifiers()));
        System.out.println(tag + " instanceof public=" + pub.isInstance(o));
        System.out.println(tag + " Impl.isInstance=" + f(() -> Class.forName(impl).isInstance(o)));
        System.out.println(tag + " hasDeclaredFields=" + (c.getDeclaredFields().length > 0));
        System.out.println(tag + " fieldNames=" + f(() -> {
            List<String> n = new ArrayList<>();
            for (var fl : c.getDeclaredFields()) n.add(fl.getName());
            Collections.sort(n);
            return n;
        }));
    }

    static void sockProbe(String tag, SSLSocket s) {
        shape(tag, s, "sun.security.ssl.SSLSocketImpl", SSLSocket.class);
        System.out.println(tag + " instanceof Socket=" + (s instanceof Socket));
        System.out.println(tag + " connected=" + s.isConnected() + " closed=" + s.isClosed() + " bound=" + s.isBound());
        System.out.println(tag + " port>0=" + (s.getPort() > 0) + " localPort>0=" + (s.getLocalPort() > 0));
        System.out.println(tag + " inet=" + f(() -> s.getInetAddress() != null));
        System.out.println(tag + " useClientMode=" + s.getUseClientMode());
        System.out.println(tag + " needAuth=" + s.getNeedClientAuth() + " wantAuth=" + s.getWantClientAuth());
        System.out.println(tag + " enableSessionCreation=" + s.getEnableSessionCreation());
        System.out.println(tag + " protocols=" + Arrays.asList(s.getEnabledProtocols()));
        System.out.println(tag + " cipherCount>0=" + (s.getEnabledCipherSuites().length > 0));
        System.out.println(tag + " params.protocols=" + Arrays.asList(s.getSSLParameters().getProtocols()));
        System.out.println(tag + " soTimeout=" + f(() -> s.getSoTimeout()));
        System.out.println(tag + " tcpNoDelay=" + f(() -> s.getTcpNoDelay()));
        System.out.println(tag + " keepAlive=" + f(() -> s.getKeepAlive()));
        System.out.println(tag + " session=" + s.getSession().getProtocol() + "/" + s.getSession().getCipherSuite());
        System.out.println(tag + " session.class=" + s.getSession().getClass().getName());
        System.out.println(tag + " handshakeSession=" + f(() -> s.getHandshakeSession()));
        System.out.println(tag + " appProto=" + f(() -> s.getApplicationProtocol()));
        System.out.println(tag + " toStringKind=" + s.toString().replaceAll("[0-9A-Fa-f]{6,}", "H").replaceAll("[0-9]{4,}", "N").substring(0, 9));
    }

    public static void main(String[] a) throws Exception {
        String ks = System.getProperty("nio19.ks");
        KeyStore k = KeyStore.getInstance("PKCS12");
        try (InputStream in = new FileInputStream(ks)) { k.load(in, "changeit".toCharArray()); }
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(k, "changeit".toCharArray());
        TrustManagerFactory tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(k);
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(kmf.getKeyManagers(), tmf.getTrustManagers(), null);

        // 1. the factories
        SSLSocketFactory sf = ctx.getSocketFactory();
        SSLServerSocketFactory ssf = ctx.getServerSocketFactory();
        shape("ctx-socket-factory", sf, "sun.security.ssl.SSLSocketFactoryImpl", SSLSocketFactory.class);
        shape("ctx-server-factory", ssf, "sun.security.ssl.SSLServerSocketFactoryImpl", SSLServerSocketFactory.class);
        shape("default-socket-factory", SSLSocketFactory.getDefault(), "sun.security.ssl.SSLSocketFactoryImpl", SSLSocketFactory.class);
        shape("default-server-factory", SSLServerSocketFactory.getDefault(), "sun.security.ssl.SSLServerSocketFactoryImpl", SSLServerSocketFactory.class);
        System.out.println("factory defaultCiphers>0=" + (sf.getDefaultCipherSuites().length > 0)
                + " supported>=default=" + (sf.getSupportedCipherSuites().length >= sf.getDefaultCipherSuites().length));
        System.out.println("server factory defaultCiphers>0=" + (ssf.getDefaultCipherSuites().length > 0));

        // 2. unconnected sockets
        try (SSLSocket u = (SSLSocket) sf.createSocket()) {
            shape("unconnected-socket", u, "sun.security.ssl.SSLSocketImpl", SSLSocket.class);
            System.out.println("unconnected connected=" + u.isConnected() + " bound=" + u.isBound() + " closed=" + u.isClosed());
            System.out.println("unconnected clientMode=" + u.getUseClientMode() + " protocols=" + Arrays.asList(u.getEnabledProtocols()));
            u.setEnabledProtocols(new String[] {"TLSv1.3"});
            System.out.println("unconnected after setEnabledProtocols=" + Arrays.asList(u.getEnabledProtocols()));
            u.setSoTimeout(1234);
            System.out.println("unconnected soTimeout=" + u.getSoTimeout());
        }
        try (SSLServerSocket u = (SSLServerSocket) ssf.createServerSocket()) {
            shape("unbound-server-socket", u, "sun.security.ssl.SSLServerSocketImpl", SSLServerSocket.class);
            System.out.println("unbound bound=" + u.isBound() + " closed=" + u.isClosed() + " port=" + u.getLocalPort());
            u.setNeedClientAuth(true);
            System.out.println("unbound needAuth=" + u.getNeedClientAuth() + " wantAuth=" + u.getWantClientAuth());
            u.setEnabledProtocols(new String[] {"TLSv1.3"});
            System.out.println("unbound protocols=" + Arrays.asList(u.getEnabledProtocols()));
            System.out.println("unbound toString=" + u.toString());
        }

        // 3. a loopback echo: server socket, accepted socket, client socket
        try (SSLServerSocket ss = (SSLServerSocket) ssf.createServerSocket(0, 5, InetAddress.getLoopbackAddress())) {
            shape("bound-server-socket", ss, "sun.security.ssl.SSLServerSocketImpl", SSLServerSocket.class);
            System.out.println("bound bound=" + ss.isBound() + " closed=" + ss.isClosed() + " port>0=" + (ss.getLocalPort() > 0));
            System.out.println("bound toStringKind=" + ss.toString().replaceAll("[0-9]{3,}", "N"));
            System.out.println("bound clientMode=" + ss.getUseClientMode() + " protocols=" + Arrays.asList(ss.getEnabledProtocols()));
            CountDownLatch accepted = new CountDownLatch(1);
            ExecutorService ex = Executors.newSingleThreadExecutor(r -> { Thread t = new Thread(r); t.setDaemon(true); return t; });
            Future<String> server = ex.submit(() -> {
                try (SSLSocket s = (SSLSocket) ss.accept()) {
                    accepted.countDown();
                    s.startHandshake();
                    StringBuilder sb = new StringBuilder();
                    sb.append("accepted class=").append(s.getClass().getName())
                      .append(" impl=").append(Class.forName("sun.security.ssl.SSLSocketImpl").isInstance(s))
                      .append(" clientMode=").append(s.getUseClientMode())
                      .append(" session=").append(s.getSession().getProtocol()).append('/').append(s.getSession().getCipherSuite())
                      .append(" peerCerts=").append(f(() -> s.getSession().getPeerCertificates().length))
                      .append(" localCerts=").append(f(() -> s.getSession().getLocalCertificates().length));
                    InputStream in = s.getInputStream();
                    byte[] b = new byte[5];
                    int n = 0;
                    while (n < 5) { int r = in.read(b, n, 5 - n); if (r < 0) break; n += r; }
                    s.getOutputStream().write(("echo:" + new String(b, 0, n)).getBytes());
                    s.getOutputStream().flush();
                    s.getInputStream().read();
                    return sb.toString();
                }
            });
            try (SSLSocket c = (SSLSocket) sf.createSocket("localhost", ss.getLocalPort())) {
                sockProbe("client", c);
                c.getOutputStream().write("hello".getBytes());
                c.getOutputStream().flush();
                byte[] b = new byte[16];
                int n = 0, r;
                while (n < 10 && (r = c.getInputStream().read(b, n, 16 - n)) > 0) n += r;
                System.out.println("client echo=" + new String(b, 0, n));
            }
            try {
                System.out.println(server.get(30, TimeUnit.SECONDS));
            } catch (TimeoutException x) {
                System.out.println("server side: HANG");
            }
            ex.shutdownNow();
        }

        // 4. layered socket over a plain one
        try (ServerSocket plain = new ServerSocket(0, 5, InetAddress.getLoopbackAddress())) {
            ExecutorService ex = Executors.newSingleThreadExecutor(r -> { Thread t = new Thread(r); t.setDaemon(true); return t; });
            Future<Integer> srv = ex.submit(() -> { try (Socket s = plain.accept()) { return s.getInputStream().read(); } });
            try (Socket raw = new Socket(InetAddress.getLoopbackAddress(), plain.getLocalPort());
                 SSLSocket layered = (SSLSocket) sf.createSocket(raw, "localhost", plain.getLocalPort(), true)) {
                shape("layered", layered, "sun.security.ssl.SSLSocketImpl", SSLSocket.class);
                System.out.println("layered connected=" + layered.isConnected() + " clientMode=" + layered.getUseClientMode());
            }
            ex.shutdownNow();
        }
        System.out.println("done");
    }
}
