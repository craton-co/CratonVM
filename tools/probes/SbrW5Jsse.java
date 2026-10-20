import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.URL;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.*;
import java.security.cert.*;
import java.util.*;
import javax.net.ssl.*;

/**
 * SBR wave 5, lane JSSE: what `SbrW4Ssl` does not cover once the handshake half
 * is the JDK's own -- ALPN through real `SSLParameters` (the field, not the
 * native side table) on an `SSLEngine` pair and an `SSLSocket` pair, the
 * server-side ALPN selector, endpoint identification refusing a wrong host,
 * the JDK default factories, and the native `HttpsURLConnection` reading a real
 * `SSLSocketFactoryImpl`'s trust (trusted, untrusted) against a loopback
 * `SSLServerSocket`.
 *
 * <p>Fresh RSA-2048 key per run (the native rustls stack of the control refuses
 * RSA-1024); no printed line depends on it. The first run is printed, the same
 * surface re-run {@code iters} times and compared line by line ({@code drift}).
 */
public class SbrW5Jsse {

    static final int ITERS = Integer.getInteger("iters", 6);
    static final char[] PW = "sbr-w5-jsse".toCharArray();

    static PrivateKey priv;
    static X509Certificate leaf, other;
    static SSLContext serverCtx, clientCtx, untrustingCtx;

    // ---- tiny DER builder (SbrW4Ssl's) --------------------------------------------------------
    static byte[] tlv(int tag, byte[] body) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        o.write(tag);
        int n = body.length;
        if (n < 128) o.write(n);
        else if (n < 256) { o.write(0x81); o.write(n); }
        else { o.write(0x82); o.write((n >> 8) & 0xff); o.write(n & 0xff); }
        o.write(body, 0, n);
        return o.toByteArray();
    }
    static byte[] cat(byte[]... ps) { ByteArrayOutputStream o = new ByteArrayOutputStream(); for (byte[] p : ps) o.write(p, 0, p.length); return o.toByteArray(); }
    static byte[] seq(byte[]... ps) { return tlv(0x30, cat(ps)); }
    static byte[] oid(int... e) { byte[] b = new byte[e.length]; for (int i = 0; i < e.length; i++) b[i] = (byte) e[i]; return tlv(0x06, b); }
    static byte[] ascii(int tag, String s) { return tlv(tag, s.getBytes(StandardCharsets.US_ASCII)); }

    static X509Certificate selfSigned(PrivateKey k, PublicKey p, String cn, int serial) throws Exception {
        byte[] alg = seq(oid(0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b), new byte[] {0x05, 0x00});
        byte[] name = seq(
                tlv(0x31, seq(oid(0x55, 0x04, 0x06), ascii(0x13, "US"))),
                tlv(0x31, seq(oid(0x55, 0x04, 0x0a), ascii(0x13, "Craton JSSE"))),
                tlv(0x31, seq(oid(0x55, 0x04, 0x03), ascii(0x13, cn))));
        byte[] validity = seq(ascii(0x17, "200101000000Z"), ascii(0x17, "491231235959Z"));
        byte[] san = seq(ascii(0x82, "localhost"));
        byte[] exts = tlv(0xa3, seq(seq(oid(0x55, 0x1d, 0x11), tlv(0x04, san))));
        byte[] tbs = seq(tlv(0xa0, new byte[] {0x02, 0x01, 0x02}), new byte[] {0x02, 0x01, (byte) serial},
                alg, name, validity, name, p.getEncoded(), exts);
        Signature s = Signature.getInstance("SHA256withRSA");
        s.initSign(k);
        s.update(tbs);
        byte[] der = seq(tbs, alg, tlv(0x03, cat(new byte[] {0}, s.sign())));
        return (X509Certificate) CertificateFactory.getInstance("X.509").generateCertificate(new ByteArrayInputStream(der));
    }

    static String cls(Object o) { return o == null ? "null" : o.getClass().getName(); }
    static String ex(Throwable t) { return t.getClass().getName(); }

    static SSLContext ctx(KeyStore keys, KeyStore trust) throws Exception {
        KeyManager[] kms = null;
        TrustManager[] tms = null;
        if (keys != null) {
            KeyManagerFactory kmf = KeyManagerFactory.getInstance("PKIX");
            kmf.init(keys, PW);
            kms = kmf.getKeyManagers();
        }
        if (trust != null) {
            TrustManagerFactory tmf = TrustManagerFactory.getInstance("PKIX");
            tmf.init(trust);
            tms = tmf.getTrustManagers();
        }
        SSLContext c = SSLContext.getInstance("TLS");
        c.init(kms, tms, null);
        return c;
    }

    static void setup() throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        priv = kp.getPrivate();
        leaf = selfSigned(priv, kp.getPublic(), "localhost", 42);
        other = selfSigned(priv, kp.getPublic(), "Other Root", 7);
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, null);
        ks.setKeyEntry("k", priv, PW, new java.security.cert.Certificate[] {leaf});
        KeyStore ts = KeyStore.getInstance("PKCS12");
        ts.load(null, null);
        ts.setCertificateEntry("ca", leaf);
        KeyStore bad = KeyStore.getInstance("PKCS12");
        bad.load(null, null);
        bad.setCertificateEntry("o", other);
        serverCtx = ctx(ks, null);
        clientCtx = ctx(null, ts);
        untrustingCtx = ctx(null, bad);
    }

    // ---- sections -----------------------------------------------------------------------------
    static void params(List<String> out) {
        SSLParameters p = new SSLParameters();
        out.add("p.fresh=" + Arrays.toString(p.getApplicationProtocols()));
        p.setApplicationProtocols(new String[] {"h2", "http/1.1"});
        String[] got = p.getApplicationProtocols();
        got[0] = "mutated";
        out.add("p.set=" + Arrays.toString(p.getApplicationProtocols()) + " copy=" + !"mutated".equals(p.getApplicationProtocols()[0]));
        try { p.setApplicationProtocols(null); out.add("p.null=ok"); } catch (Exception e) { out.add("p.null=" + ex(e) + ": " + e.getMessage()); }
        try { p.setApplicationProtocols(new String[] {"h2", ""}); out.add("p.empty=ok"); } catch (Exception e) { out.add("p.empty=" + ex(e) + ": " + e.getMessage()); }
        SSLParameters q = new SSLParameters(new String[] {"TLS_AES_128_GCM_SHA256"}, new String[] {"TLSv1.3"});
        out.add("p.ctor=" + Arrays.toString(q.getCipherSuites()) + " " + Arrays.toString(q.getProtocols()) + " alpn=" + q.getApplicationProtocols().length);
    }

    static void defaults(List<String> out) throws Exception {
        out.add("def.sf=" + cls(SSLSocketFactory.getDefault()));
        out.add("def.ssf=" + cls(SSLServerSocketFactory.getDefault()));
        out.add("def.ctxSf=" + cls(SSLContext.getDefault().getSocketFactory()) + " same=" + (SSLSocketFactory.getDefault() == SSLSocketFactory.getDefault()));
        out.add("def.huc=" + cls(HttpsURLConnection.getDefaultSSLSocketFactory()));
        SSLSocket u = (SSLSocket) SSLSocketFactory.getDefault().createSocket();
        out.add("def.unconnected=" + cls(u) + " connected=" + u.isConnected() + " client=" + u.getUseClientMode());
        u.close();
        SSLServerSocket ss = (SSLServerSocket) SSLServerSocketFactory.getDefault().createServerSocket();
        out.add("def.ssUnbound=" + cls(ss) + " bound=" + ss.isBound() + " client=" + ss.getUseClientMode());
        ss.close();
    }

    static int runTasks(SSLEngine e) { int n = 0; Runnable t; while ((t = e.getDelegatedTask()) != null) { t.run(); n++; } return n; }

    /** Drives an engine pair to the end of the handshake; returns the failure's class or "ok". */
    static String handshake(SSLEngine c, SSLEngine s) throws Exception {
        int pkt = Math.max(c.getSession().getPacketBufferSize(), s.getSession().getPacketBufferSize()) * 4;
        ByteBuffer c2s = ByteBuffer.allocate(pkt), s2c = ByteBuffer.allocate(pkt), dst = ByteBuffer.allocate(pkt * 2);
        c.beginHandshake();
        s.beginHandshake();
        try {
            for (int i = 0; i < 200; i++) {
                if (c.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING
                        && s.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING
                        && c2s.position() == 0 && s2c.position() == 0) return "ok";
                c.wrap(ByteBuffer.allocate(0), c2s); runTasks(c);
                s.wrap(ByteBuffer.allocate(0), s2c); runTasks(s);
                c2s.flip(); s.unwrap(c2s, dst); c2s.compact(); runTasks(s);
                s2c.flip(); c.unwrap(s2c, dst); s2c.compact(); runTasks(c);
            }
            return "stuck";
        } catch (SSLException e) {
            return ex(e);
        }
    }

    static void alpnEngine(List<String> out) throws Exception {
        SSLEngine c = clientCtx.createSSLEngine("localhost", 443);
        c.setUseClientMode(true);
        SSLParameters cp = c.getSSLParameters();
        out.add("eng.alpn.before=" + Arrays.toString(cp.getApplicationProtocols()));
        cp.setApplicationProtocols(new String[] {"h2", "http/1.1"});
        c.setSSLParameters(cp);
        out.add("eng.alpn.roundTrip=" + Arrays.toString(c.getSSLParameters().getApplicationProtocols()));
        SSLEngine s = serverCtx.createSSLEngine();
        s.setUseClientMode(false);
        SSLParameters sp = s.getSSLParameters();
        sp.setApplicationProtocols(new String[] {"http/1.1"});
        s.setSSLParameters(sp);
        out.add("eng.alpn.hsNull=" + c.getHandshakeApplicationProtocol() + " app=" + c.getApplicationProtocol());
        out.add("eng.alpn.hs=" + handshake(c, s));
        out.add("eng.alpn.neg=" + c.getApplicationProtocol() + "/" + s.getApplicationProtocol());

        SSLEngine c2 = clientCtx.createSSLEngine("localhost", 443);
        c2.setUseClientMode(true);
        SSLParameters cp2 = c2.getSSLParameters();
        cp2.setApplicationProtocols(new String[] {"spdy/3", "h2"});
        c2.setSSLParameters(cp2);
        SSLEngine s2 = serverCtx.createSSLEngine();
        s2.setUseClientMode(false);
        final List<String> offered = new ArrayList<>();
        s2.setHandshakeApplicationProtocolSelector((eng, protos) -> { offered.addAll(protos); return protos.contains("h2") ? "h2" : null; });
        out.add("eng.sel.set=" + (s2.getHandshakeApplicationProtocolSelector() != null));
        out.add("eng.sel.hs=" + handshake(c2, s2));
        out.add("eng.sel.neg=" + c2.getApplicationProtocol() + "/" + s2.getApplicationProtocol() + " offered=" + offered);

        SSLEngine c3 = clientCtx.createSSLEngine("localhost", 443);
        c3.setUseClientMode(true);
        SSLEngine s3 = serverCtx.createSSLEngine();
        s3.setUseClientMode(false);
        SSLParameters sp3 = s3.getSSLParameters();
        sp3.setApplicationProtocols(new String[] {"h2"});
        s3.setSSLParameters(sp3);
        out.add("eng.noClientAlpn.hs=" + handshake(c3, s3) + " neg=[" + c3.getApplicationProtocol() + "]/[" + s3.getApplicationProtocol() + "]");
    }

    static void endpointId(List<String> out) throws Exception {
        SSLEngine c = clientCtx.createSSLEngine("wrong.example", 443);
        c.setUseClientMode(true);
        SSLParameters cp = c.getSSLParameters();
        cp.setEndpointIdentificationAlgorithm("HTTPS");
        c.setSSLParameters(cp);
        SSLEngine s = serverCtx.createSSLEngine();
        s.setUseClientMode(false);
        out.add("eia.wrongHost=" + handshake(c, s));
        SSLEngine c2 = clientCtx.createSSLEngine("wrong.example", 443);
        c2.setUseClientMode(true);
        SSLEngine s2 = serverCtx.createSSLEngine();
        s2.setUseClientMode(false);
        out.add("eia.none=" + handshake(c2, s2));
        SSLEngine c3 = untrustingCtx.createSSLEngine("localhost", 443);
        c3.setUseClientMode(true);
        SSLEngine s3 = serverCtx.createSSLEngine();
        s3.setUseClientMode(false);
        out.add("eia.untrusted=" + handshake(c3, s3));
    }

    static void alpnSocket(List<String> out) throws Exception {
        SSLServerSocket ss = (SSLServerSocket) serverCtx.getServerSocketFactory().createServerSocket(0, 5, InetAddress.getLoopbackAddress());
        SSLParameters ssp = ss.getSSLParameters();
        ssp.setApplicationProtocols(new String[] {"h2", "http/1.1"});
        ss.setSSLParameters(ssp);
        ss.setSoTimeout(20000);
        final String[] srv = new String[1];
        Thread t = new Thread(() -> {
            try (SSLSocket a = (SSLSocket) ss.accept()) {
                a.setSoTimeout(20000);
                int b = a.getInputStream().read();
                a.getOutputStream().write(b + 1);
                a.getOutputStream().flush();
                srv[0] = "sock.srv=" + a.getApplicationProtocol() + " client=" + a.getUseClientMode();
            } catch (Exception e) {
                srv[0] = "sock.srv.err=" + e;
            }
        });
        t.setDaemon(true);
        t.start();
        SSLSocket s = (SSLSocket) clientCtx.getSocketFactory().createSocket(InetAddress.getLoopbackAddress(), ss.getLocalPort());
        s.setSoTimeout(20000);
        SSLParameters p = s.getSSLParameters();
        p.setApplicationProtocols(new String[] {"http/1.1"});
        s.setSSLParameters(p);
        s.startHandshake();
        out.add("sock.alpn=" + s.getApplicationProtocol() + " params=" + Arrays.toString(s.getSSLParameters().getApplicationProtocols()));
        s.getOutputStream().write(41);
        s.getOutputStream().flush();
        out.add("sock.echo=" + s.getInputStream().read());
        s.close();
        t.join(20000);
        out.add(srv[0]);
        ss.close();
    }

    /** A one-shot HTTPS/1.1 server on loopback; returns its port. */
    static SSLServerSocket httpsServer(int requests) throws Exception {
        SSLServerSocket ss = (SSLServerSocket) serverCtx.getServerSocketFactory().createServerSocket(0, 5, InetAddress.getLoopbackAddress());
        ss.setSoTimeout(20000);
        Thread t = new Thread(() -> {
            for (int i = 0; i < requests; i++) {
                try (SSLSocket a = (SSLSocket) ss.accept()) {
                    a.setSoTimeout(20000);
                    InputStream in = a.getInputStream();
                    StringBuilder head = new StringBuilder();
                    int ch;
                    while ((ch = in.read()) != -1) {
                        head.append((char) ch);
                        if (head.length() >= 4 && head.substring(head.length() - 4).equals("\r\n\r\n")) break;
                    }
                    String line = head.toString().split("\r\n")[0];
                    byte[] body = ("hello " + line).getBytes(StandardCharsets.US_ASCII);
                    OutputStream o = a.getOutputStream();
                    o.write(("HTTP/1.1 200 OK\r\nContent-Length: " + body.length + "\r\nConnection: close\r\n\r\n").getBytes(StandardCharsets.US_ASCII));
                    o.write(body);
                    o.flush();
                } catch (Exception e) {
                    // an untrusting client aborts the handshake: expected, keep serving
                }
            }
        });
        t.setDaemon(true);
        t.start();
        return ss;
    }

    static void huc(List<String> out) throws Exception {
        SSLServerSocket ss = httpsServer(2);
        int port = ss.getLocalPort();
        HttpsURLConnection h = (HttpsURLConnection) new URL("https://localhost:" + port + "/x").openConnection();
        h.setSSLSocketFactory(clientCtx.getSocketFactory());
        h.setConnectTimeout(20000);
        h.setReadTimeout(20000);
        out.add("huc.sf=" + cls(h.getSSLSocketFactory()));
        try {
            int code = h.getResponseCode();
            byte[] body = h.getInputStream().readAllBytes();
            out.add("huc.trusted=" + code + " " + new String(body, StandardCharsets.US_ASCII) + " suite=" + (h.getCipherSuite() != null));
        } catch (Exception e) {
            out.add("huc.trusted=" + ex(e));
        }
        HttpsURLConnection h2 = (HttpsURLConnection) new URL("https://localhost:" + port + "/y").openConnection();
        h2.setSSLSocketFactory(untrustingCtx.getSocketFactory());
        h2.setConnectTimeout(20000);
        h2.setReadTimeout(20000);
        try {
            out.add("huc.untrusted=" + h2.getResponseCode());
        } catch (Exception e) {
            out.add("huc.untrusted=" + ex(e));
        }
        ss.close();
    }

    static List<String> all() {
        List<String> out = new ArrayList<>();
        run(out, "params", () -> params(out));
        run(out, "def", () -> defaults(out));
        run(out, "alpnEng", () -> alpnEngine(out));
        run(out, "eia", () -> endpointId(out));
        run(out, "alpnSock", () -> alpnSocket(out));
        run(out, "huc", () -> huc(out));
        return out;
    }

    interface Body { void run() throws Exception; }

    static void run(List<String> out, String tag, Body b) {
        int before = out.size();
        try {
            b.run();
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            out.add(tag + ".THREW=" + t + " at " + (st.length > 0 ? st[0] : "?"));
        }
        for (int i = before; i < out.size(); i++) out.set(i, tag + "|" + out.get(i));
    }

    /** SSLSocket loopback: handshake, then handshake + 1 MiB, per round (lane record, timing). */
    static void perf(int rounds) throws Exception {
        SSLServerSocket ss = (SSLServerSocket) serverCtx.getServerSocketFactory().createServerSocket(0, 50, InetAddress.getLoopbackAddress());
        Thread t = new Thread(() -> {
            byte[] buf = new byte[65536];
            while (true) {
                try (SSLSocket a = (SSLSocket) ss.accept()) {
                    InputStream in = a.getInputStream();
                    int n;
                    long total = 0;
                    while ((n = in.read(buf)) > 0) total += n;
                } catch (Exception e) {
                    if (ss.isClosed()) return;
                }
            }
        });
        t.setDaemon(true);
        t.start();
        byte[] mib = new byte[1 << 20];
        for (int i = 0; i < mib.length; i++) mib[i] = (byte) i;
        for (int r = -2; r < rounds; r++) {
            long t0 = System.nanoTime();
            int hs = 5;
            for (int i = 0; i < hs; i++) {
                try (SSLSocket s = (SSLSocket) clientCtx.getSocketFactory().createSocket(InetAddress.getLoopbackAddress(), ss.getLocalPort())) {
                    s.startHandshake();
                }
            }
            long t1 = System.nanoTime();
            int mb = 4;
            for (int i = 0; i < mb; i++) {
                try (SSLSocket s = (SSLSocket) clientCtx.getSocketFactory().createSocket(InetAddress.getLoopbackAddress(), ss.getLocalPort())) {
                    s.getOutputStream().write(mib);
                    s.getOutputStream().flush();
                }
            }
            long t2 = System.nanoTime();
            if (r < 0) continue;
            System.out.printf(Locale.ROOT, "PERFSOCK round=%d handshake_ms=%.2f hs_plus_mib_ms=%.2f%n", r, (t1 - t0) / 1e6 / hs, (t2 - t1) / 1e6 / mb);
        }
        ss.close();
    }

    public static void main(String[] a) throws Exception {
        setup();
        if (a.length > 0 && a[0].equals("perf")) { perf(a.length > 1 ? Integer.parseInt(a[1]) : 5); return; }
        List<String> first = all();
        for (String s : first) System.out.println(s);
        int drift = 0;
        for (int i = 0; i < ITERS; i++) {
            List<String> again = all();
            for (int j = 0; j < Math.max(first.size(), again.size()); j++) {
                String x = j < first.size() ? first.get(j) : null, y = j < again.size() ? again.get(j) : null;
                if (!Objects.equals(x, y)) {
                    if (drift < 10) System.out.println("DRIFT iter=" + i + " first=" + x + " now=" + y);
                    drift++;
                }
            }
        }
        System.out.println("SUMMARY lines=" + first.size() + " iters=" + ITERS + " drift=" + drift);
    }
}
