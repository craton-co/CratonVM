import java.net.*;
import java.security.*;
import java.util.*;
import javax.net.*;
import javax.net.ssl.*;

/** L6 — the TLS surface that is pure bookkeeping: `javax.net.ssl.SSLParameters`,
 *  `SSLContext`, the factories, and the parameter half of `SSLSocket` /
 *  `SSLServerSocket` / `SSLEngine`.
 *
 *  **No handshake, and no socket is ever connected.** The lane page says why:
 *  a live handshake is the regression corpus's instrument, not a probe's, and
 *  a probe that needs a peer is a probe that measures the host. Everything
 *  here is a get/set round-trip, a validation refusal, or a list-filtering
 *  rule, all of which are decided before any byte moves.
 *
 *  The suite and protocol LISTS are printed sorted. They are a deterministic
 *  property of the provider on each VM — not a value a VM chooses per run —
 *  and if the two disagree that is exactly the kind of difference this lane
 *  exists to find. What is NOT printed is any session id, any timestamp, any
 *  socket-local port, or any peer identity.
 *
 *  `SSLEngine` is created but never asked to `wrap`: `getUseClientMode`,
 *  `setEnabledCipherSuites` validation and the handshake-status *before*
 *  `beginHandshake` are all answerable from state the constructor set.
 */
public class L6TlsParamSweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    static String[] sortedArr(String[] a) {
        if (a == null) return new String[0];
        String[] c = a.clone();
        Arrays.sort(c);
        return c;
    }

    static String sorted(String[] a) {
        if (a == null) return "null";
        String[] c = a.clone();
        Arrays.sort(c);
        return c.length + " " + Arrays.toString(c);
    }

    public static void main(String[] args) throws Exception {
        // ---- SSLParameters: a value object, so every setter must round-trip
        tv("params default cipherSuites", () -> sorted(new SSLParameters().getCipherSuites()));
        tv("params default protocols", () -> sorted(new SSLParameters().getProtocols()));
        tv("params default needClientAuth", () -> new SSLParameters().getNeedClientAuth());
        tv("params default wantClientAuth", () -> new SSLParameters().getWantClientAuth());
        tv("params default endpointId", () -> String.valueOf(new SSLParameters().getEndpointIdentificationAlgorithm()));
        tv("params default useCipherSuitesOrder", () -> new SSLParameters().getUseCipherSuitesOrder());
        tv("params default serverNames", () -> String.valueOf(new SSLParameters().getServerNames()));
        tv("params default sniMatchers", () -> String.valueOf(new SSLParameters().getSNIMatchers()));
        tv("params default algorithmConstraints", () -> String.valueOf(new SSLParameters().getAlgorithmConstraints()));
        tv("params default appProtocols", () -> sorted(new SSLParameters().getApplicationProtocols()));
        tv("params default maxPacketSize", () -> new SSLParameters().getMaximumPacketSize());
        tv("params default enableRetransmissions", () -> new SSLParameters().getEnableRetransmissions());

        tv("params ctor 1-arg", () -> sorted(new SSLParameters(new String[] {"A", "B"}).getCipherSuites()));
        tv("params ctor 2-arg protocols", () -> sorted(new SSLParameters(new String[] {"A"}, new String[] {"P"}).getProtocols()));
        tv("params ctor null suites", () -> String.valueOf(new SSLParameters((String[]) null).getCipherSuites()));
        tv("params setCipherSuites copies", () -> {
            String[] in = {"A", "B"};
            SSLParameters q = new SSLParameters();
            q.setCipherSuites(in);
            in[0] = "Z";
            return sorted(q.getCipherSuites());
        });
        tv("params getCipherSuites copies", () -> {
            SSLParameters q = new SSLParameters();
            q.setCipherSuites(new String[] {"A", "B"});
            q.getCipherSuites()[0] = "Z";
            return sorted(q.getCipherSuites());
        });
        tv("params setCipherSuites null", () -> {
            SSLParameters q = new SSLParameters();
            q.setCipherSuites(null);
            return String.valueOf(q.getCipherSuites());
        });
        tv("params setProtocols null", () -> {
            SSLParameters q = new SSLParameters();
            q.setProtocols(null);
            return String.valueOf(q.getProtocols());
        });
        tv("params need excludes want", () -> {
            SSLParameters q = new SSLParameters();
            q.setWantClientAuth(true);
            q.setNeedClientAuth(true);
            return q.getNeedClientAuth() + " " + q.getWantClientAuth();
        });
        tv("params want excludes need", () -> {
            SSLParameters q = new SSLParameters();
            q.setNeedClientAuth(true);
            q.setWantClientAuth(true);
            return q.getNeedClientAuth() + " " + q.getWantClientAuth();
        });
        tv("params endpointId roundtrip", () -> {
            SSLParameters q = new SSLParameters();
            q.setEndpointIdentificationAlgorithm("HTTPS");
            return q.getEndpointIdentificationAlgorithm();
        });
        tv("params endpointId null", () -> {
            SSLParameters q = new SSLParameters();
            q.setEndpointIdentificationAlgorithm("HTTPS");
            q.setEndpointIdentificationAlgorithm(null);
            return String.valueOf(q.getEndpointIdentificationAlgorithm());
        });
        tv("params maxPacketSize negative", () -> {
            SSLParameters q = new SSLParameters();
            q.setMaximumPacketSize(-1);
            return q.getMaximumPacketSize();
        });
        tv("params serverNames roundtrip", () -> {
            SSLParameters q = new SSLParameters();
            q.setServerNames(Arrays.asList(new SNIHostName("a.example")));
            List<SNIServerName> l = q.getServerNames();
            return l.size() + " " + ((SNIHostName) l.get(0)).getAsciiName() + " type=" + l.get(0).getType();
        });
        tv("params serverNames duplicate type", () -> {
            SSLParameters q = new SSLParameters();
            q.setServerNames(Arrays.asList(new SNIHostName("a.example"), new SNIHostName("b.example")));
            return "no throw";
        });
        tv("params serverNames null", () -> {
            SSLParameters q = new SSLParameters();
            q.setServerNames(null);
            return String.valueOf(q.getServerNames());
        });
        tv("params serverNames empty", () -> {
            SSLParameters q = new SSLParameters();
            q.setServerNames(Collections.emptyList());
            return String.valueOf(q.getServerNames());
        });
        tv("params sniMatchers roundtrip", () -> {
            SSLParameters q = new SSLParameters();
            q.setSNIMatchers(Arrays.asList(SNIHostName.createSNIMatcher("a\\.example")));
            return q.getSNIMatchers().size();
        });
        tv("params appProtocols null element", () -> {
            SSLParameters q = new SSLParameters();
            q.setApplicationProtocols(new String[] {"h2", null});
            return "no throw";
        });
        tv("params appProtocols empty element", () -> {
            SSLParameters q = new SSLParameters();
            q.setApplicationProtocols(new String[] {"h2", ""});
            return "no throw";
        });
        tv("params appProtocols roundtrip", () -> {
            SSLParameters q = new SSLParameters();
            q.setApplicationProtocols(new String[] {"h2", "http/1.1"});
            return sorted(q.getApplicationProtocols());
        });
        tv("params appProtocols null", () -> {
            SSLParameters q = new SSLParameters();
            q.setApplicationProtocols(null);
            return "no throw";
        });

        // ---- SNIHostName / SNIServerName validation, which is pure parsing
        tv("SNIHostName ok", () -> new SNIHostName("a.example").getAsciiName());
        tv("SNIHostName trailing dot", () -> new SNIHostName("a.example.").getAsciiName());
        tv("SNIHostName empty", () -> new SNIHostName("").getAsciiName());
        tv("SNIHostName leading dot", () -> new SNIHostName(".a").getAsciiName());
        tv("SNIHostName underscore", () -> new SNIHostName("a_b.example").getAsciiName());
        tv("SNIHostName uppercase", () -> new SNIHostName("A.EXAMPLE").getAsciiName());
        tv("SNIHostName equals folds case", () -> new SNIHostName("A.example").equals(new SNIHostName("a.EXAMPLE")));
        tv("SNIHostName toString", () -> new SNIHostName("a.example").toString());
        tv("SNIHostName null", () -> new SNIHostName((String) null).getAsciiName());
        tv("SNIHostName ipv4 literal", () -> new SNIHostName("127.0.0.1").getAsciiName());
        tv("SNIHostName encoded roundtrip", () -> {
            SNIHostName h = new SNIHostName("a.example");
            return Arrays.toString(h.getEncoded()).length() > 0;
        });

        // ---- SSLContext: the algorithm names, and the uninitialised refusals
        for (String proto : new String[] {
                "TLS", "TLSv1.2", "TLSv1.3", "SSL", "SSLv3", "TLSv1", "TLSv1.1",
                "Default", "DTLS", "DTLSv1.2", "NoSuchProtocol"})
            tv("SSLContext.getInstance " + proto, () -> {
                SSLContext c = SSLContext.getInstance(proto);
                return c.getProtocol() + " provider-present=" + (c.getProvider() != null);
            });
        tv("SSLContext.getInstance null", () -> SSLContext.getInstance(null).getProtocol());
        tv("SSLContext.getInstance empty", () -> SSLContext.getInstance("").getProtocol());
        tv("SSLContext.getInstance bad provider", () -> SSLContext.getInstance("TLS", "NoSuchProvider").getProtocol());
        tv("SSLContext.getDefault protocol", () -> SSLContext.getDefault().getProtocol());
        tv("SSLContext default supported params", () -> sorted(SSLContext.getDefault().getSupportedSSLParameters().getProtocols()));
        tv("SSLContext default default params", () -> sorted(SSLContext.getDefault().getDefaultSSLParameters().getProtocols()));
        tv("SSLContext default suites", () -> sorted(SSLContext.getDefault().getDefaultSSLParameters().getCipherSuites()));
        tv("SSLContext supported suites", () -> sorted(SSLContext.getDefault().getSupportedSSLParameters().getCipherSuites()));
        tv("SSLContext uninitialised socketFactory", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            return c.getSocketFactory().getClass().getSuperclass().getName();
        });
        tv("SSLContext uninitialised createSSLEngine", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            return c.createSSLEngine().getClass().getSuperclass().getName();
        });
        tv("SSLContext init nulls then engine", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLEngine e = c.createSSLEngine();
            return e.getUseClientMode() + " " + e.getHandshakeStatus();
        });
        tv("SSLContext sessionContexts", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            return (c.getClientSessionContext() != null) + " " + (c.getServerSessionContext() != null);
        });
        tv("SSLContext setDefault null", () -> { SSLContext.setDefault(null); return "no throw"; });

        // ---- the factories, without creating a connected socket
        tv("SSLSocketFactory.getDefault class family",
           () -> SSLSocketFactory.getDefault().getClass().getSuperclass().getName());
        tv("SSLSocketFactory default suites",
           () -> sorted(((SSLSocketFactory) SSLSocketFactory.getDefault()).getDefaultCipherSuites()));
        tv("SSLSocketFactory supported suites",
           () -> sorted(((SSLSocketFactory) SSLSocketFactory.getDefault()).getSupportedCipherSuites()));
        tv("SSLServerSocketFactory default suites",
           () -> sorted(((SSLServerSocketFactory) SSLServerSocketFactory.getDefault()).getDefaultCipherSuites()));
        tv("SocketFactory.getDefault class",
           () -> SocketFactory.getDefault().getClass().getName());
        tv("ServerSocketFactory.getDefault class",
           () -> ServerSocketFactory.getDefault().getClass().getName());
        tv("SocketFactory default is not SSL",
           () -> SocketFactory.getDefault() instanceof SSLSocketFactory);
        tv("SSLSocketFactory.getDefault stable",
           () -> SSLSocketFactory.getDefault() == SSLSocketFactory.getDefault());

        // ---- an UNCONNECTED SSLSocket: everything answerable without a peer
        tv("unconnected SSLSocket surface", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            StringBuilder sb = new StringBuilder();
            sb.append("connected=").append(s.isConnected());
            sb.append(" clientMode=").append(s.getUseClientMode());
            sb.append(" need=").append(s.getNeedClientAuth());
            sb.append(" want=").append(s.getWantClientAuth());
            sb.append(" createSessions=").append(s.getEnableSessionCreation());
            sb.append(" enabledProtocols=").append(sorted(s.getEnabledProtocols()));
            sb.append(" supportedProtocols=").append(sorted(s.getSupportedProtocols()));
            s.close();
            return sb.toString();
        });
        tv("unconnected SSLSocket suites", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            String r = sorted(s.getEnabledCipherSuites());
            s.close();
            return r;
        });
        tv("SSLSocket setEnabledCipherSuites unknown", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            try { s.setEnabledCipherSuites(new String[] {"TLS_NO_SUCH_SUITE"}); return "no throw"; }
            finally { s.close(); }
        });
        tv("SSLSocket setEnabledCipherSuites empty", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            try { s.setEnabledCipherSuites(new String[0]); return "no throw"; }
            finally { s.close(); }
        });
        tv("SSLSocket setEnabledCipherSuites null", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            try { s.setEnabledCipherSuites(null); return "no throw"; }
            finally { s.close(); }
        });
        tv("SSLSocket setEnabledProtocols unknown", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            try { s.setEnabledProtocols(new String[] {"NoSuchTLS"}); return "no throw"; }
            finally { s.close(); }
        });
        tv("SSLSocket params round-trip", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            SSLParameters q = s.getSSLParameters();
            q.setNeedClientAuth(true);
            q.setEndpointIdentificationAlgorithm("HTTPS");
            s.setSSLParameters(q);
            String r = s.getNeedClientAuth() + " " + s.getSSLParameters().getEndpointIdentificationAlgorithm();
            s.close();
            return r;
        });
        tv("SSLSocket session before handshake", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            SSLSession ss = s.getSession();
            String r = "proto=" + ss.getCipherSuite() + " " + ss.getProtocol()
                     + " valid=" + ss.isValid() + " peerHost=" + ss.getPeerHost();
            s.close();
            return r;
        });
        tv("SSLSocket getPeerCertificates before handshake", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            try { return String.valueOf(s.getSession().getPeerCertificates().length); }
            catch (Throwable e) { return "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()); }
            finally { s.close(); }
        });
        tv("SSLSocket handshake with no peer refuses", () -> {
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            try { s.startHandshake(); return "no throw"; }
            catch (Throwable e) { return e.getClass().getName(); }
            finally { s.close(); }
        });

        // ---- an unbound SSLServerSocket
        tv("SSLServerSocket surface", () -> {
            SSLServerSocket s = (SSLServerSocket)
                ((SSLServerSocketFactory) SSLServerSocketFactory.getDefault()).createServerSocket();
            String r = "bound=" + s.isBound() + " need=" + s.getNeedClientAuth()
                     + " want=" + s.getWantClientAuth() + " clientMode=" + s.getUseClientMode()
                     + " sessions=" + s.getEnableSessionCreation();
            s.close();
            return r;
        });
        tv("SSLServerSocket params round-trip", () -> {
            SSLServerSocket s = (SSLServerSocket)
                ((SSLServerSocketFactory) SSLServerSocketFactory.getDefault()).createServerSocket();
            SSLParameters q = s.getSSLParameters();
            q.setWantClientAuth(true);
            s.setSSLParameters(q);
            String r = s.getWantClientAuth() + " " + s.getNeedClientAuth();
            s.close();
            return r;
        });

        // ---- SSLEngine, created but never asked to move a byte
        tv("SSLEngine defaults", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLEngine e = c.createSSLEngine();
            return "clientMode=" + e.getUseClientMode()
                 + " status=" + e.getHandshakeStatus()
                 + " inboundDone=" + e.isInboundDone()
                 + " outboundDone=" + e.isOutboundDone()
                 + " need=" + e.getNeedClientAuth()
                 + " want=" + e.getWantClientAuth()
                 + " peerHost=" + e.getPeerHost()
                 + " peerPort=" + e.getPeerPort();
        });
        tv("SSLEngine with peer hints", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLEngine e = c.createSSLEngine("peer.invalid", 443);
            return e.getPeerHost() + ":" + e.getPeerPort();
        });
        tv("SSLEngine setUseClientMode true", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLEngine e = c.createSSLEngine();
            e.setUseClientMode(true);
            return e.getUseClientMode();
        });
        tv("SSLEngine enabled suites", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            return sorted(c.createSSLEngine().getEnabledCipherSuites());
        });
        tv("SSLEngine supported protocols", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            return sorted(c.createSSLEngine().getSupportedProtocols());
        });
        tv("SSLEngine unknown suite refused", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLEngine e = c.createSSLEngine();
            try { e.setEnabledCipherSuites(new String[] {"TLS_NO_SUCH_SUITE"}); return "no throw"; }
            catch (Throwable t) { return t.getClass().getName(); }
        });
        tv("SSLEngine session before handshake", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLSession s = c.createSSLEngine().getSession();
            return s.getCipherSuite() + " " + s.getProtocol() + " valid=" + s.isValid()
                 + " appBufSize>0=" + (s.getApplicationBufferSize() > 0)
                 + " packetBufSize>0=" + (s.getPacketBufferSize() > 0);
        });
        tv("SSLEngine closeOutbound then status", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLEngine e = c.createSSLEngine();
            e.setUseClientMode(true);
            e.closeOutbound();
            return e.isOutboundDone() + " " + e.isInboundDone() + " " + e.getHandshakeStatus();
        });
        tv("SSLEngine closeInbound before handshake", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLEngine e = c.createSSLEngine();
            e.setUseClientMode(true);
            try { e.closeInbound(); return "no throw"; }
            catch (Throwable t) { return t.getClass().getName() + " msg=" + esc(t.getMessage()); }
        });

        // ---- the manager factories: names, defaults, and the uninitialised refusal
        tv("KeyManagerFactory default algorithm", () -> KeyManagerFactory.getDefaultAlgorithm());
        tv("TrustManagerFactory default algorithm", () -> TrustManagerFactory.getDefaultAlgorithm());
        tv("KeyManagerFactory getInstance default", () -> {
            KeyManagerFactory f = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
            return f.getAlgorithm() + " provider-present=" + (f.getProvider() != null);
        });
        tv("KeyManagerFactory getInstance bogus", () -> KeyManagerFactory.getInstance("NoSuchKmf").getAlgorithm());
        tv("KeyManagerFactory getInstance null", () -> KeyManagerFactory.getInstance(null).getAlgorithm());
        tv("KeyManagerFactory uninitialised getKeyManagers", () -> {
            KeyManagerFactory f = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
            try { return "n=" + f.getKeyManagers().length; }
            catch (Throwable t) { return t.getClass().getName() + " msg=" + esc(t.getMessage()); }
        });
        tv("KeyManagerFactory init null keystore", () -> {
            KeyManagerFactory f = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
            try { f.init(null, null); return "n=" + f.getKeyManagers().length; }
            catch (Throwable t) { return t.getClass().getName() + " msg=" + esc(t.getMessage()); }
        });
        tv("TrustManagerFactory getInstance default", () -> {
            TrustManagerFactory f = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
            return f.getAlgorithm() + " provider-present=" + (f.getProvider() != null);
        });
        tv("TrustManagerFactory uninitialised getTrustManagers", () -> {
            TrustManagerFactory f = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
            try { return "n=" + f.getTrustManagers().length; }
            catch (Throwable t) { return t.getClass().getName() + " msg=" + esc(t.getMessage()); }
        });
        tv("TrustManagerFactory init null = default trust store", () -> {
            TrustManagerFactory f = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
            try {
                f.init((java.security.KeyStore) null);
                TrustManager[] m = f.getTrustManagers();
                return "n=" + m.length + " first-is-X509=" + (m[0] instanceof X509TrustManager);
            } catch (Throwable t) { return t.getClass().getName() + " msg=" + esc(t.getMessage()); }
        });

        // ---- the exception family's own shape
        tv("SSLException msg", () -> new SSLException("m").getMessage());
        tv("SSLException cause ctor", () -> {
            SSLException e = new SSLException(new IllegalStateException("c"));
            return esc(e.getMessage()) + " cause=" + e.getCause().getClass().getName();
        });
        tv("SSLHandshakeException is SSLException", () -> new SSLHandshakeException("m") instanceof SSLException);
        tv("SSLPeerUnverifiedException is SSLException", () -> new SSLPeerUnverifiedException("m") instanceof SSLException);
        tv("SSLKeyException is SSLException", () -> new SSLKeyException("m") instanceof SSLException);
        tv("SSLProtocolException is SSLException", () -> new SSLProtocolException("m") instanceof SSLException);
        tv("SSLException is IOException", () -> new SSLException("m") instanceof java.io.IOException);

        // ---- the lists, asked of the VM about ITSELF
        //
        // Appended at the END so every row number above keeps its identity.
        //
        // The suite and protocol lists above cannot match HotSpot's — this
        // VM's TLS is rustls and supports a different set, which the lane's
        // retirement record calls out as not a defect to fix by lying about
        // the list. But "the list is different" and "the VM gives two
        // different answers to the same question" are not the same claim, and
        // only the second is measurable without settling the first. Every row
        // here is `true` on HotSpot BY CONSTRUCTION — it asks one JSSE
        // whether it agrees with itself — so a `false` is this VM's own
        // internal contradiction and nothing to do with which suites rustls
        // implements.
        tv("agree: SSLContext supported == SSLSocketFactory supported", () ->
            Arrays.equals(
                sortedArr(SSLContext.getDefault().getSupportedSSLParameters().getCipherSuites()),
                sortedArr(((SSLSocketFactory) SSLSocketFactory.getDefault()).getSupportedCipherSuites())));
        tv("agree: SSLSocketFactory default == SSLServerSocketFactory default", () ->
            Arrays.equals(
                sortedArr(((SSLSocketFactory) SSLSocketFactory.getDefault()).getDefaultCipherSuites()),
                sortedArr(((SSLServerSocketFactory) SSLServerSocketFactory.getDefault()).getDefaultCipherSuites())));
        tv("agree: SSLEngine enabled == SSLEngine supported", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLEngine e = c.createSSLEngine();
            return Arrays.equals(sortedArr(e.getEnabledCipherSuites()),
                                 sortedArr(e.getSupportedCipherSuites()));
        });
        tv("agree: SSLSocket supported protocols == SSLEngine supported protocols", () -> {
            SSLContext c = SSLContext.getInstance("TLS");
            c.init(null, null, null);
            SSLSocket s = (SSLSocket) ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket();
            String[] a = sortedArr(s.getSupportedProtocols());
            s.close();
            return Arrays.equals(a, sortedArr(c.createSSLEngine().getSupportedProtocols()));
        });

        System.out.println("rows " + rows);
        System.out.println("DONE L6TlsParamSweep");
    }
}
