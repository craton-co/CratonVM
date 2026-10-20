import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.*;
import java.security.cert.*;
import java.security.spec.*;
import java.util.*;
import javax.net.ssl.*;
import javax.security.auth.x500.X500Principal;

/**
 * SBR wave 4, lane SSL: the JSSE surface whose `Bridge` natives won over real
 * bytecode in the `--jdk-only` census (`SSLContext`, `SSLEngineImpl`,
 * `SSLEngineResult`, `SSLSocket`/`SSLServerSocket`/`SSLSocketFactory`,
 * `KeyManagerFactory`, `TrustManagerFactory`, `X509TrustManagerImpl`,
 * `SSLSocketImpl$AppInputStream`/`$AppOutputStream`, the keystore SPIs and
 * `X500Principal`/`X500Name`).
 *
 * <p>The key is generated per run (RSA-1024; RSA-2048 in perf mode) and the
 * self-signed DER certificate is assembled here; no printed line depends on
 * the key's value, so every printed line is deterministic and diffs against HotSpot 25. The FIRST
 * run's lines are printed; the same surface is re-run {@code iters} times and
 * each line compared with the first run -- {@code drift} counts mismatches,
 * so a cold answer the warm path does not reproduce shows up.
 *
 * <p>{@code java SbrW4Ssl perf [rounds]} instead times an SSLEngine loopback
 * TLS handshake and a 1 MiB application-data transfer (lane record, timing).
 */
public class SbrW4Ssl {

    static final int ITERS = Integer.getInteger("iters", 12);
    static final char[] PW = "sbr-w4-ssl".toCharArray();

    static PrivateKey priv;
    static PublicKey pub;
    static X509Certificate leaf;
    static X509Certificate other;
    static SSLContext serverCtx, clientCtx;

    // ---- tiny DER builder (RSslLiveSession's) -------------------------------------------------
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
                tlv(0x31, seq(oid(0x55, 0x04, 0x0a), ascii(0x13, "Craton, SBR"))),
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

    static long fp(byte[] b) { long h = 1125899906842597L; for (byte x : b) h = 31 * h + x; return h; }
    static String cls(Object o) { return o == null ? "null" : o.getClass().getName(); }
    static String ex(Throwable t) { return t.getClass().getName(); }

    static void setup() throws Exception { setup(false); }

    /** {@code rsa2048}: a fresh RSA-2048 key (timing only -- the native rustls stack refuses RSA-1024). */
    static void setup(boolean rsa2048) throws Exception {
        // A fresh key every run (no key material in the source). No printed line
        // depends on the key's value.
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(rsa2048 ? 2048 : 1024);
        KeyPair kp = kpg.generateKeyPair();
        priv = kp.getPrivate();
        pub = kp.getPublic();
        leaf = selfSigned(priv, pub, "localhost", 42);
        other = selfSigned(priv, pub, "Other Root", 7);
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, null);
        ks.setKeyEntry("k", priv, PW, new java.security.cert.Certificate[] {leaf});
        KeyStore ts = KeyStore.getInstance("PKCS12");
        ts.load(null, null);
        ts.setCertificateEntry("ca", leaf);
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, PW);
        serverCtx = SSLContext.getInstance("TLS");
        serverCtx.init(kmf.getKeyManagers(), null, null);
        TrustManagerFactory tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ts);
        clientCtx = SSLContext.getInstance("TLS");
        clientCtx.init(null, tmf.getTrustManagers(), null);
    }

    // ---- sections -----------------------------------------------------------------------------
    static void factories(List<String> out) throws Exception {
        out.add("kmf.default=" + KeyManagerFactory.getDefaultAlgorithm());
        out.add("tmf.default=" + TrustManagerFactory.getDefaultAlgorithm());
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, null);
        ks.setKeyEntry("k", priv, PW, new java.security.cert.Certificate[] {leaf});
        KeyManagerFactory kmf = KeyManagerFactory.getInstance("SunX509");
        out.add("kmf.alg=" + kmf.getAlgorithm() + " prov=" + kmf.getProvider().getName());
        kmf.init(ks, PW);
        KeyManager[] kms = kmf.getKeyManagers();
        out.add("kmf.kms=" + kms.length + " " + cls(kms[0]));
        X509KeyManager xkm = (X509KeyManager) kms[0];
        out.add("km.aliases=" + Arrays.toString(xkm.getServerAliases("RSA", null)));
        out.add("km.chain=" + (xkm.getCertificateChain("k") == null ? "null" : xkm.getCertificateChain("k").length));
        out.add("km.key=" + (xkm.getPrivateKey("k") == null ? "null" : xkm.getPrivateKey("k").getAlgorithm()));
        KeyManagerFactory kmf2 = KeyManagerFactory.getInstance("PKIX");
        kmf2.init(ks, PW);
        out.add("kmf2.kms=" + cls(kmf2.getKeyManagers()[0]));
        try { KeyManagerFactory.getInstance("NoSuch"); out.add("kmf.nosuch=ok"); } catch (Exception e) { out.add("kmf.nosuch=" + ex(e)); }
        try { KeyManagerFactory.getInstance("SunX509").getKeyManagers(); out.add("kmf.uninit=ok"); } catch (Exception e) { out.add("kmf.uninit=" + ex(e)); }

        KeyStore ts = KeyStore.getInstance("PKCS12");
        ts.load(null, null);
        ts.setCertificateEntry("ca", leaf);
        TrustManagerFactory tmf = TrustManagerFactory.getInstance("PKIX");
        out.add("tmf.alg=" + tmf.getAlgorithm() + " prov=" + tmf.getProvider().getName());
        tmf.init(ts);
        TrustManager[] tms = tmf.getTrustManagers();
        out.add("tmf.tms=" + tms.length + " " + cls(tms[0]));
        X509TrustManager xtm = (X509TrustManager) tms[0];
        X509Certificate[] acc = xtm.getAcceptedIssuers();
        out.add("tm.issuers=" + acc.length + " " + (acc.length > 0 ? acc[0].getSubjectX500Principal().getName() : "-"));
        try { xtm.checkServerTrusted(new X509Certificate[] {leaf}, "RSA"); out.add("tm.trusted=ok"); } catch (Exception e) { out.add("tm.trusted=" + ex(e)); }
        try { xtm.checkServerTrusted(new X509Certificate[] {other}, "RSA"); out.add("tm.untrusted=ok"); } catch (Exception e) { out.add("tm.untrusted=" + ex(e)); }
        try { xtm.checkServerTrusted(new X509Certificate[0], "RSA"); out.add("tm.empty=ok"); } catch (Exception e) { out.add("tm.empty=" + ex(e)); }
        try { xtm.checkClientTrusted(new X509Certificate[] {leaf}, "RSA"); out.add("tm.client=ok"); } catch (Exception e) { out.add("tm.client=" + ex(e)); }
        TrustManagerFactory tmfSimple = TrustManagerFactory.getInstance("SunX509");
        tmfSimple.init(ts);
        out.add("tmf.simple=" + cls(tmfSimple.getTrustManagers()[0]));
        TrustManagerFactory tmfDef = TrustManagerFactory.getInstance("PKIX");
        tmfDef.init((KeyStore) null);
        X509TrustManager dtm = (X509TrustManager) tmfDef.getTrustManagers()[0];
        out.add("tm.cacerts.nonEmpty=" + (dtm.getAcceptedIssuers().length > 50));
        try { TrustManagerFactory.getInstance("PKIX").getTrustManagers(); out.add("tmf.uninit=ok"); } catch (Exception e) { out.add("tmf.uninit=" + ex(e)); }
    }

    static void context(List<String> out) throws Exception {
        SSLContext c = SSLContext.getInstance("TLS");
        out.add("ctx.proto=" + c.getProtocol() + " prov=" + c.getProvider().getName());
        try { c.getSocketFactory(); out.add("ctx.uninitSf=ok"); } catch (Exception e) { out.add("ctx.uninitSf=" + ex(e)); }
        c.init(null, null, null);
        out.add("ctx.v13=" + SSLContext.getInstance("TLSv1.3").getProtocol());
        SSLContext d = SSLContext.getDefault();
        out.add("ctx.default=" + d.getProtocol() + " same=" + (d == SSLContext.getDefault()));
        SSLParameters sp = c.getSupportedSSLParameters();
        out.add("ctx.supProtos=" + Arrays.toString(sp.getProtocols()));
        SSLParameters dp = c.getDefaultSSLParameters();
        out.add("ctx.defProtos=" + Arrays.toString(dp.getProtocols()));
        out.add("ctx.defSuites0=" + dp.getCipherSuites()[0] + " n>10=" + (dp.getCipherSuites().length > 10));
        try { SSLContext.getInstance("NoSuchTLS"); out.add("ctx.nosuch=ok"); } catch (Exception e) { out.add("ctx.nosuch=" + ex(e)); }
        SSLEngine e = c.createSSLEngine();
        out.add("eng.client=" + e.getUseClientMode() + " host=" + e.getPeerHost() + " port=" + e.getPeerPort());
        SSLEngine e2 = c.createSSLEngine("example.test", 8443);
        out.add("eng2.host=" + e2.getPeerHost() + ":" + e2.getPeerPort());
        e2.setUseClientMode(true);
        out.add("eng2.client=" + e2.getUseClientMode());
        out.add("eng.protos=" + Arrays.toString(e2.getEnabledProtocols()));
        out.add("eng.supProtos=" + Arrays.toString(e2.getSupportedProtocols()));
        out.add("eng.suites0=" + e2.getEnabledCipherSuites()[0] + " sup>=en=" + (e2.getSupportedCipherSuites().length >= e2.getEnabledCipherSuites().length));
        out.add("eng.hs=" + e2.getHandshakeStatus());
        out.add("eng.sess=" + e2.getSession().getProtocol() + " " + e2.getSession().getCipherSuite() + " valid=" + e2.getSession().isValid());
        out.add("eng.hsSess=" + (e2.getHandshakeSession() == null ? "null" : "non-null"));
        out.add("eng.task=" + (e2.getDelegatedTask() == null ? "null" : "non-null"));
        SSLParameters p = e2.getSSLParameters();
        p.setProtocols(new String[] {"TLSv1.2"});
        p.setEndpointIdentificationAlgorithm("HTTPS");
        e2.setSSLParameters(p);
        out.add("eng.setParams=" + Arrays.toString(e2.getEnabledProtocols()) + " eia=" + e2.getSSLParameters().getEndpointIdentificationAlgorithm());
        e2.setEnabledCipherSuites(new String[] {"TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"});
        out.add("eng.setSuites=" + Arrays.toString(e2.getEnabledCipherSuites()));
        e2.closeOutbound();
        out.add("eng.closedOut=" + e2.isOutboundDone() + " in=" + e2.isInboundDone());
        SSLEngineResult r = new SSLEngineResult(SSLEngineResult.Status.OK, SSLEngineResult.HandshakeStatus.NEED_WRAP, 3, 5);
        out.add("res=" + r.getStatus() + "/" + r.getHandshakeStatus() + "/" + r.bytesConsumed() + "/" + r.bytesProduced());
        SSLSocketFactory sf = c.getSocketFactory();
        out.add("sf=" + cls(sf) + " suites0=" + sf.getDefaultCipherSuites()[0]);
        out.add("ssf=" + cls(c.getServerSocketFactory()));
        out.add("sf.default=" + cls(SSLSocketFactory.getDefault()));
    }

    /** One SSLEngine loopback: handshake, {@code bytes} client->server, 1 KiB back, close. */
    static void engine(List<String> out, String[] protos, int bytes, boolean trace) throws Exception {
        SSLEngine c = clientCtx.createSSLEngine("localhost", 443);
        c.setUseClientMode(true);
        SSLEngine s = serverCtx.createSSLEngine();
        s.setUseClientMode(false);
        if (protos != null) { c.setEnabledProtocols(protos); s.setEnabledProtocols(protos); }
        SSLParameters cp = c.getSSLParameters();
        cp.setEndpointIdentificationAlgorithm("HTTPS");
        c.setSSLParameters(cp);
        int pkt = Math.max(c.getSession().getPacketBufferSize(), s.getSession().getPacketBufferSize());
        int app = Math.max(c.getSession().getApplicationBufferSize(), s.getSession().getApplicationBufferSize());
        ByteBuffer c2s = ByteBuffer.allocate(pkt * 4), s2c = ByteBuffer.allocate(pkt * 4);
        byte[] payload = new byte[bytes];
        for (int i = 0; i < bytes; i++) payload[i] = (byte) (i * 7 + (i >>> 9));
        ByteBuffer cSrc = ByteBuffer.wrap(payload);
        byte[] back = new byte[1024];
        for (int i = 0; i < back.length; i++) back[i] = (byte) (255 - i);
        ByteBuffer sSrc = ByteBuffer.wrap(back);
        ByteBuffer sDst = ByteBuffer.allocate(bytes + app * 2), cDst = ByteBuffer.allocate(back.length + app * 2);
        StringBuilder tr = new StringBuilder();
        c.beginHandshake();
        s.beginHandshake();
        boolean cDone = false, sDone = false;
        int steps = 0, tasks = 0;
        while (steps++ < 100000) {
            boolean hsDone = c.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING
                    && s.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING;
            SSLEngineResult r;
            r = c.wrap(hsDone ? cSrc : ByteBuffer.allocate(0), c2s);
            if (trace) tr.append("cw:").append(code(r)).append(' ');
            tasks += runTasks(c);
            r = s.wrap(hsDone && !sSrc.hasRemaining() ? ByteBuffer.allocate(0) : (hsDone ? sSrc : ByteBuffer.allocate(0)), s2c);
            if (trace) tr.append("sw:").append(code(r)).append(' ');
            tasks += runTasks(s);
            c2s.flip();
            r = s.unwrap(c2s, sDst);
            c2s.compact();
            if (trace) tr.append("su:").append(code(r)).append(' ');
            tasks += runTasks(s);
            s2c.flip();
            r = c.unwrap(s2c, cDst);
            s2c.compact();
            if (trace) tr.append("cu:").append(code(r)).append(' ');
            tasks += runTasks(c);
            if (!cSrc.hasRemaining() && !sSrc.hasRemaining() && sDst.position() == bytes && cDst.position() == back.length
                    && c2s.position() == 0 && s2c.position() == 0) break;
        }
        SSLSession cs = c.getSession(), ss = s.getSession();
        out.add("hs.proto=" + cs.getProtocol() + " suite=" + cs.getCipherSuite() + " srv=" + ss.getProtocol() + "/" + ss.getCipherSuite());
        out.add("hs.valid=" + cs.isValid() + " idLen=" + cs.getId().length + " peerHost=" + cs.getPeerHost() + ":" + cs.getPeerPort());
        out.add("hs.peer=" + cs.getPeerPrincipal().getName() + " certs=" + cs.getPeerCertificates().length
                + " eq=" + cs.getPeerPrincipal().equals(leaf.getSubjectX500Principal()) + " local=" + (cs.getLocalCertificates() == null ? "null" : "" + cs.getLocalCertificates().length));
        out.add("hs.srvLocal=" + ss.getLocalPrincipal().getName() + " srvPeerUnverified=" + peerUnverified(ss));
        out.add("hs.appSize>0=" + (cs.getApplicationBufferSize() > 0) + " pkt>=app=" + (cs.getPacketBufferSize() >= cs.getApplicationBufferSize()));
        byte[] got = Arrays.copyOf(sDst.array(), sDst.position());
        byte[] got2 = Arrays.copyOf(cDst.array(), cDst.position());
        out.add("data.c2s=" + got.length + " eq=" + Arrays.equals(got, payload) + " s2c=" + got2.length + " eq=" + Arrays.equals(got2, back));
        out.add("hs.tasks>0=" + (tasks > 0) + " hsStatus=" + c.getHandshakeStatus() + "/" + s.getHandshakeStatus());
        if (trace) out.add("hs.trace.head=" + tr.substring(0, Math.min(tr.length(), 240)));
        c.closeOutbound();
        SSLEngineResult r = c.wrap(ByteBuffer.allocate(0), c2s);
        out.add("close.cw=" + r.getStatus() + "/" + r.getHandshakeStatus() + " prod>0=" + (r.bytesProduced() > 0) + " outDone=" + c.isOutboundDone());
        c2s.flip();
        r = s.unwrap(c2s, sDst);
        c2s.compact();
        out.add("close.su=" + r.getStatus() + "/" + r.getHandshakeStatus() + " inDone=" + s.isInboundDone());
        r = s.wrap(ByteBuffer.allocate(0), s2c);
        out.add("close.sw=" + r.getStatus() + "/" + r.getHandshakeStatus() + " outDone=" + s.isOutboundDone());
        s2c.flip();
        r = c.unwrap(s2c, cDst);
        out.add("close.cu=" + r.getStatus() + "/" + r.getHandshakeStatus() + " inDone=" + c.isInboundDone());
        try { c.wrap(ByteBuffer.wrap(new byte[4]), ByteBuffer.allocate(pkt)); out.add("close.wrapAfter=" + c.wrap(ByteBuffer.wrap(new byte[4]), ByteBuffer.allocate(pkt)).getStatus()); }
        catch (Exception e) { out.add("close.wrapAfter=" + ex(e)); }
    }

    static String peerUnverified(SSLSession s) { try { s.getPeerCertificates(); return "false"; } catch (SSLPeerUnverifiedException e) { return "true"; } }

    static String code(SSLEngineResult r) {
        return r.getStatus().name().charAt(0) + "" + r.getHandshakeStatus().name().charAt(5) + (r.bytesProduced() > 0 ? "+" : "") + (r.bytesConsumed() > 0 ? "<" : "");
    }

    static int runTasks(SSLEngine e) { int n = 0; Runnable t; while ((t = e.getDelegatedTask()) != null) { t.run(); n++; } return n; }

    static void engineBad(List<String> out) throws Exception {
        // client does not trust the server: handshake must fail with SSLHandshakeException
        SSLContext untrusting = SSLContext.getInstance("TLS");
        TrustManagerFactory tmf = TrustManagerFactory.getInstance("PKIX");
        KeyStore ts = KeyStore.getInstance("PKCS12");
        ts.load(null, null);
        ts.setCertificateEntry("o", other);
        tmf.init(ts);
        untrusting.init(null, tmf.getTrustManagers(), null);
        SSLEngine c = untrusting.createSSLEngine("localhost", 443);
        c.setUseClientMode(true);
        SSLEngine s = serverCtx.createSSLEngine();
        s.setUseClientMode(false);
        int pkt = c.getSession().getPacketBufferSize() * 4;
        ByteBuffer c2s = ByteBuffer.allocate(pkt), s2c = ByteBuffer.allocate(pkt), dst = ByteBuffer.allocate(pkt);
        String res = "no-failure";
        try {
            for (int i = 0; i < 50; i++) {
                c.wrap(ByteBuffer.allocate(0), c2s); runTasks(c);
                s.wrap(ByteBuffer.allocate(0), s2c); runTasks(s);
                c2s.flip(); s.unwrap(c2s, dst); c2s.compact(); runTasks(s);
                s2c.flip(); c.unwrap(s2c, dst); s2c.compact(); runTasks(c);
            }
        } catch (Exception e) {
            res = ex(e);
        }
        out.add("bad.untrusted=" + res);
    }

    static void socket(List<String> out) throws Exception {
        SSLServerSocket ss = (SSLServerSocket) serverCtx.getServerSocketFactory().createServerSocket(0, 5, InetAddress.getLoopbackAddress());
        ss.setSoTimeout(20000);
        int port = ss.getLocalPort();
        out.add("ss.class=" + cls(ss) + " bound=" + ss.isBound() + " port>0=" + (port > 0) + " client=" + ss.getUseClientMode());
        final String[] srv = new String[1];
        Thread t = new Thread(() -> {
            try (SSLSocket a = (SSLSocket) ss.accept()) {
                a.setSoTimeout(20000);
                InputStream in = a.getInputStream();
                OutputStream o = a.getOutputStream();
                int first = in.read();
                byte[] buf = new byte[4096];
                int total = 1, n;
                long sum = first;
                while (total < 65536 && (n = in.read(buf, 0, buf.length)) > 0) { total += n; for (int i = 0; i < n; i++) sum += buf[i] & 0xff; }
                o.write(("got " + total + " sum " + sum + "\n").getBytes(StandardCharsets.US_ASCII));
                o.flush();
                srv[0] = "srv.proto=" + a.getSession().getProtocol() + " client=" + a.getUseClientMode();
            } catch (Exception e) {
                srv[0] = "srv.err=" + e;
            }
        });
        t.setDaemon(true);
        t.start();
        SSLSocket s = (SSLSocket) clientCtx.getSocketFactory().createSocket(InetAddress.getLoopbackAddress(), port);
        s.setSoTimeout(20000);
        out.add("sock.class=" + cls(s) + " connected=" + s.isConnected() + " client=" + s.getUseClientMode());
        out.add("sock.hsSess=" + (s.getHandshakeSession() == null ? "null" : "non-null"));
        s.startHandshake();
        SSLSession sess = s.getSession();
        out.add("sock.sess=" + sess.getProtocol() + " " + sess.getCipherSuite() + " peer=" + sess.getPeerPrincipal().getName());
        OutputStream o = s.getOutputStream();
        byte[] data = new byte[65536];
        for (int i = 0; i < data.length; i++) data[i] = (byte) (i * 13);
        o.write(data);
        o.flush();
        InputStream in = s.getInputStream();
        StringBuilder sb = new StringBuilder();
        int ch;
        while ((ch = in.read()) != -1 && ch != '\n') sb.append((char) ch);
        out.add("sock.reply=" + sb);
        s.close();
        out.add("sock.closed=" + s.isClosed());
        t.join(20000);
        out.add(srv[0]);
        ss.close();
        out.add("ss.closed=" + ss.isClosed());
        SSLSocket u = (SSLSocket) clientCtx.getSocketFactory().createSocket();
        out.add("sock.unconnected=" + u.isConnected() + " " + cls(u));
        u.close();
    }

    static void keystore(List<String> out) throws Exception {
        KeyStore ks = KeyStore.getInstance("PKCS12");
        out.add("ks.type=" + ks.getType() + " prov=" + ks.getProvider().getName());
        ks.load(null, null);
        ks.setKeyEntry("key1", priv, PW, new java.security.cert.Certificate[] {leaf});
        ks.setCertificateEntry("cert1", other);
        List<String> al = Collections.list(ks.aliases());
        Collections.sort(al);
        out.add("ks.aliases=" + al + " size=" + ks.size());
        out.add("ks.isCert=" + ks.isCertificateEntry("cert1") + "/" + ks.isCertificateEntry("key1") + " isKey=" + ks.isKeyEntry("key1"));
        Key k = ks.getKey("key1", PW);
        out.add("ks.key=" + k.getAlgorithm() + " " + k.getFormat() + " eq=" + Arrays.equals(k.getEncoded(), priv.getEncoded()));
        out.add("ks.cert.eq=" + ks.getCertificate("cert1").equals(other) + " alias=" + ks.getCertificateAlias(leaf));
        out.add("ks.chain=" + ks.getCertificateChain("key1").length);
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        ks.store(bo, PW);
        byte[] p12 = bo.toByteArray();
        out.add("ks.stored>0=" + (p12.length > 500) + " tag=" + (p12[0] & 0xff));
        KeyStore back = KeyStore.getInstance("PKCS12");
        back.load(new ByteArrayInputStream(p12), PW);
        List<String> al2 = Collections.list(back.aliases());
        Collections.sort(al2);
        out.add("ks.reload=" + al2 + " keyEq=" + Arrays.equals(back.getKey("key1", PW).getEncoded(), priv.getEncoded()));
        KeyStore jksView = KeyStore.getInstance("JKS");
        jksView.load(new ByteArrayInputStream(p12), PW);
        List<String> al3 = Collections.list(jksView.aliases());
        Collections.sort(al3);
        out.add("jks.dual=" + al3 + " isCert=" + jksView.isCertificateEntry("cert1"));
        try { back.load(new ByteArrayInputStream(p12), "wrong".toCharArray()); out.add("ks.wrongPw=ok"); } catch (Exception e) { out.add("ks.wrongPw=" + ex(e)); }
        KeyStore jks = KeyStore.getInstance("JKS");
        jks.load(null, null);
        jks.setCertificateEntry("c", leaf);
        jks.setKeyEntry("k", priv, PW, new java.security.cert.Certificate[] {leaf});
        ByteArrayOutputStream jo = new ByteArrayOutputStream();
        jks.store(jo, PW);
        byte[] jb = jo.toByteArray();
        out.add("jks.magic=" + Integer.toHexString(((jb[0] & 0xff) << 24) | ((jb[1] & 0xff) << 16) | ((jb[2] & 0xff) << 8) | (jb[3] & 0xff)));
        KeyStore jr = KeyStore.getInstance("JKS");
        jr.load(new ByteArrayInputStream(jb), PW);
        List<String> al4 = Collections.list(jr.aliases());
        Collections.sort(al4);
        out.add("jks.reload=" + al4 + " isCert=" + jr.isCertificateEntry("c") + " keyEq=" + Arrays.equals(jr.getKey("k", PW).getEncoded(), priv.getEncoded()));
        KeyStore p12view = KeyStore.getInstance("PKCS12");
        p12view.load(new ByteArrayInputStream(jb), PW);
        out.add("p12.dualJks=" + p12view.size() + " type=" + p12view.getType());
    }

    static void x500(List<String> out) throws Exception {
        X500Principal p = leaf.getSubjectX500Principal();
        out.add("x500.rfc2253=" + p.getName());
        out.add("x500.rfc1779=" + p.getName(X500Principal.RFC1779));
        out.add("x500.canon=" + p.getName(X500Principal.CANONICAL));
        X500Principal q = new X500Principal("CN=localhost, O=\"Craton, SBR\", C=US");
        out.add("x500.eq=" + p.equals(q) + " hash=" + (p.hashCode() == q.hashCode()) + " neq=" + p.equals(other.getSubjectX500Principal()));
        out.add("x500.enc=" + fp(p.getEncoded()) + " " + (fp(p.getEncoded()) == fp(q.getEncoded())));
        X500Principal r = new X500Principal("cn=A\\,B+OU=x,  dc=example , dc=org");
        out.add("x500.multi=" + r.getName() + " | " + r.getName(X500Principal.CANONICAL));
        Map<String, String> kw = new HashMap<>();
        kw.put("EMAILADDRESS", "1.2.840.113549.1.9.1");
        out.add("x500.kw=" + new X500Principal("EMAILADDRESS=a@b.c, CN=z").getName(X500Principal.RFC2253, kw));
        try { new X500Principal("not a dn"); out.add("x500.bad=ok"); } catch (Exception e) { out.add("x500.bad=" + ex(e)); }
        out.add("x500.fromIssuer=" + leaf.getIssuerX500Principal().equals(p) + " toString=" + p);
    }

    /** AES-GCM with a 16-byte IV: the shape JSSE's stateless session tickets use. */
    static void gcm16(List<String> out) throws Exception {
        byte[] k = new byte[16], iv = new byte[16];
        for (int i = 0; i < 16; i++) { k[i] = (byte) (i + 1); iv[i] = (byte) (0xA0 + i); }
        javax.crypto.spec.SecretKeySpec key = new javax.crypto.spec.SecretKeySpec(k, "AES");
        javax.crypto.Cipher c = javax.crypto.Cipher.getInstance("AES/GCM/NoPadding");
        c.init(javax.crypto.Cipher.ENCRYPT_MODE, key, new javax.crypto.spec.GCMParameterSpec(128, iv));
        c.updateAAD(new byte[] {1, 2, 3, 4});
        byte[] ct = c.doFinal("stateless ticket body".getBytes(StandardCharsets.US_ASCII));
        StringBuilder h = new StringBuilder();
        for (byte b : ct) h.append(String.format("%02x", b & 0xff));
        out.add("gcm16.enc=" + h);
        c.init(javax.crypto.Cipher.DECRYPT_MODE, key, new javax.crypto.spec.GCMParameterSpec(128, iv));
        c.updateAAD(new byte[] {1, 2, 3, 4});
        out.add("gcm16.dec=" + new String(c.doFinal(ct), StandardCharsets.US_ASCII));
        ct[3] ^= 1;
        c.init(javax.crypto.Cipher.DECRYPT_MODE, key, new javax.crypto.spec.GCMParameterSpec(128, iv));
        c.updateAAD(new byte[] {1, 2, 3, 4});
        try { c.doFinal(ct); out.add("gcm16.tamper=ok"); } catch (Exception e) { out.add("gcm16.tamper=" + ex(e)); }
    }

    static List<String> all(boolean trace) {
        List<String> out = new ArrayList<>();
        run(out, "fact", () -> factories(out));
        run(out, "ctx", () -> context(out));
        run(out, "eng13", () -> engine(out, null, 256 * 1024, trace));
        run(out, "eng12", () -> engine(out, new String[] {"TLSv1.2"}, 64 * 1024, trace));
        run(out, "bad", () -> engineBad(out));
        run(out, "sock", () -> socket(out));
        run(out, "ks", () -> keystore(out));
        run(out, "x500", () -> x500(out));
        run(out, "gcm", () -> gcm16(out));
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

    static void perf(int rounds) throws Exception {
        setup(true);
        for (int w = 0; w < 3; w++) engine(new ArrayList<>(), null, 1 << 20, false);
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            int hs = 5;
            for (int i = 0; i < hs; i++) engine(new ArrayList<>(), null, 1, false);
            long t1 = System.nanoTime();
            int mb = 4;
            for (int i = 0; i < mb; i++) engine(new ArrayList<>(), null, 1 << 20, false);
            long t2 = System.nanoTime();
            double hsMs = (t1 - t0) / 1e6 / hs;
            double mibMs = (t2 - t1) / 1e6 / mb - hsMs;
            System.out.printf(Locale.ROOT, "PERF round=%d handshake_ms=%.2f mib_ms=%.2f total_hs_plus_mib_ms=%.2f%n", r, hsMs, mibMs, (t2 - t1) / 1e6 / mb);
        }
    }

    public static void main(String[] a) throws Exception {
        if (a.length > 0 && a[0].equals("perf")) { perf(a.length > 1 ? Integer.parseInt(a[1]) : 5); return; }
        long t0 = System.nanoTime();
        setup();
        List<String> first = all(true);
        long t1 = System.nanoTime();
        for (String s : first) System.out.println(s);
        int drift = 0;
        for (int i = 0; i < ITERS; i++) {
            List<String> again = all(true);
            for (int j = 0; j < Math.max(first.size(), again.size()); j++) {
                String x = j < first.size() ? first.get(j) : null, y = j < again.size() ? again.get(j) : null;
                if (!Objects.equals(x, y)) {
                    if (drift < 10) System.out.println("DRIFT iter=" + i + " first=" + x + " now=" + y);
                    drift++;
                }
            }
        }
        System.out.println("SUMMARY lines=" + first.size() + " iters=" + ITERS + " drift=" + drift);
        System.err.printf(Locale.ROOT, "first-run ms=%.0f%n", (t1 - t0) / 1e6);
    }
}
