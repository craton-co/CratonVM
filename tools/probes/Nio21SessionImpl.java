import java.io.*;
import java.lang.reflect.Modifier;
import java.net.*;
import java.security.KeyStore;
import java.util.*;
import java.util.concurrent.*;
import javax.net.ssl.*;

/**
 * The SSLSession a program is handed, probed the way a program probes it: its class and ancestry, the casts
 * to ExtendedSSLSession / the JDK's own implementation class, reflection over the declared fields, the
 * accessors, and the attribute API, on a client and a server session after a real loopback handshake and on
 * an engine's pre-handshake session. HotSpot hands out sun.security.ssl.SSLSessionImpl for all of them.
 * Run: -Dnio19.ks=pkcs12 (password "changeit", the key's own cert is trusted). Page:
 * compatible-tls-and-http-carriers-are-instances-of-abstract-classes-FIXED-20261010.md.
 */
public class Nio21SessionImpl {
    interface T { Object get() throws Throwable; }

    static String f(T t) {
        try {
            Object o = t.get();
            return String.valueOf(o);
        } catch (Throwable x) {
            return "!" + x.getClass().getName();
        }
    }

    static void probe(String tag, SSLSession s) {
        Class<?> c = s.getClass();
        System.out.println(tag + " class=" + c.getName());
        System.out.println(tag + " super=" + c.getSuperclass().getName() + " final=" + Modifier.isFinal(c.getModifiers())
                + " abstract=" + Modifier.isAbstract(c.getModifiers()) + " interface=" + c.isInterface());
        System.out.println(tag + " instanceof ExtendedSSLSession=" + (s instanceof ExtendedSSLSession));
        System.out.println(tag + " Impl.isInstance=" + f(() -> Class.forName("sun.security.ssl.SSLSessionImpl").isInstance(s)));
        System.out.println(tag + " hasDeclaredFields=" + (c.getDeclaredFields().length > 0));
        System.out.println(tag + " protocol=" + s.getProtocol());
        System.out.println(tag + " cipher=" + s.getCipherSuite());
        System.out.println(tag + " valid=" + s.isValid());
        System.out.println(tag + " idLen=" + s.getId().length);
        System.out.println(tag + " peerHostPort=" + s.getPeerHost() + ":" + (s.getPeerPort() > 0));
        System.out.println(tag + " localCerts=" + f(() -> s.getLocalCertificates() == null ? "null" : s.getLocalCertificates().length));
        System.out.println(tag + " peerCerts=" + f(() -> s.getPeerCertificates().length));
        System.out.println(tag + " peerPrincipal=" + f(() -> s.getPeerPrincipal()));
        System.out.println(tag + " localPrincipal=" + f(() -> s.getLocalPrincipal()));
        System.out.println(tag + " bufs=" + (s.getPacketBufferSize() > 0) + "," + (s.getApplicationBufferSize() > 0));
        System.out.println(tag + " times=" + (s.getCreationTime() > 0) + "," + (s.getLastAccessedTime() >= s.getCreationTime()));
        s.putValue("k", "v");
        System.out.println(tag + " values=" + Arrays.toString(s.getValueNames()) + " k=" + s.getValue("k"));
        s.removeValue("k");
        System.out.println(tag + " values-after=" + Arrays.toString(s.getValueNames()));
        System.out.println(tag + " equals=" + s.equals(s) + " hash=" + (s.hashCode() == s.hashCode()));
        if (s instanceof ExtendedSSLSession e) {
            System.out.println(tag + " requestedServerNames=" + f(() -> e.getRequestedServerNames()));
            System.out.println(tag + " statusResponses=" + f(() -> e.getStatusResponses()));
            System.out.println(tag + " localSigAlgs=" + f(() -> e.getLocalSupportedSignatureAlgorithms().length > 0));
        }
        System.out.println(tag + " toStringKind=" + s.toString().replaceAll("[0-9A-Fa-f]{16,}", "H").replaceAll("[0-9]{6,}", "N").substring(0, 9));
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

        // 1. the engine's session before any handshake
        SSLEngine e = ctx.createSSLEngine();
        probe("engine-null", e.getSession());

        // 2. real handshake between two engines: the client and server sessions after the handshake
        SSLEngine ce = ctx.createSSLEngine("localhost", 4433);
        SSLEngine se = ctx.createSSLEngine();
        ce.setUseClientMode(true);
        se.setUseClientMode(false);
        java.nio.ByteBuffer c2s = java.nio.ByteBuffer.allocate(65536), s2c = java.nio.ByteBuffer.allocate(65536);
        java.nio.ByteBuffer empty = java.nio.ByteBuffer.allocate(0), sink = java.nio.ByteBuffer.allocate(65536);
        ce.beginHandshake();
        se.beginHandshake();
        for (int i = 0; i < 200; i++) {
            for (SSLEngine en : new SSLEngine[] {ce, se}) {
                java.nio.ByteBuffer out = en == ce ? c2s : s2c, in = en == ce ? s2c : c2s;
                SSLEngineResult.HandshakeStatus hs = en.getHandshakeStatus();
                if (hs == SSLEngineResult.HandshakeStatus.NEED_TASK) {
                    Runnable r;
                    while ((r = en.getDelegatedTask()) != null) r.run();
                } else if (hs == SSLEngineResult.HandshakeStatus.NEED_WRAP) {
                    en.wrap(empty, out);
                } else if (hs == SSLEngineResult.HandshakeStatus.NEED_UNWRAP) {
                    in.flip();
                    if (in.hasRemaining()) en.unwrap(in, sink);
                    in.compact();
                    sink.clear();
                }
            }
        }
        probe("engine-client", ce.getSession());
        probe("engine-server", se.getSession());
        System.out.println("engine-handshake=" + ce.getHandshakeStatus() + "/" + se.getHandshakeStatus());

        // 3. SSLSocket client session (the server only completes the handshake)
        try (SSLServerSocket ss = (SSLServerSocket) ctx.getServerSocketFactory().createServerSocket(0, 5, InetAddress.getLoopbackAddress())) {
            Thread t = new Thread(() -> {
                try (SSLSocket s = (SSLSocket) ss.accept()) {
                    s.startHandshake();
                    s.getInputStream().read();
                } catch (Throwable x) {
                }
            });
            t.setDaemon(true);
            t.start();
            try (SSLSocket c = (SSLSocket) ctx.getSocketFactory().createSocket("localhost", ss.getLocalPort())) {
                c.startHandshake();
                probe("socket-client", c.getSession());
                c.getOutputStream().write(1);
            }
            t.join(10000);
        }
        System.out.println("done");
    }
}
