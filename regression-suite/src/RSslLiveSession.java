import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.URI;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.Principal;
import java.security.Signature;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.util.Arrays;
import java.util.Optional;
import javax.net.ssl.HostnameVerifier;
import javax.net.ssl.HttpsURLConnection;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLSession;
import javax.net.ssl.SSLSessionContext;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManagerFactory;

/**
 * The JSSE contract for a session that has GENUINELY NEGOTIATED, asked through every door a
 * completed TLS 1.3 handshake hands one out of.
 *
 * <h2>Why this file exists as a separate vector</h2>
 *
 * <p>{@code RSslNullSession} is the other half of this pair and its <b>no-network property is
 * load-bearing</b> — it opens no connection, so it can only ever assert what a session that
 * negotiated NOTHING reports. Two measured halves of lane F18's work therefore had no row
 * anywhere in the tree, and F25-1 NOMINATION 1 asked for this file by name:
 *
 * <ul>
 *   <li>{@code invalidate()} drops {@code getSessionContext()} from a real
 *       {@code SSLSessionContextImpl} to <b>null</b>. Four comments across three files still
 *       said "invalidate() moves isValid() and nothing else"; F18 measured the second
 *       accessor, and it is only visible where a live context exists. {@code RSslNullSession}
 *       asserts the other side of the same contract (that {@code invalidate()} does not MINT
 *       a context) and by construction cannot see this one. Family {@code invalidate}.
 *   <li>{@code getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal())} is
 *       <b>true</b>, and {@code getPeerPrincipal()} is a
 *       {@code javax.security.auth.x500.X500Principal}. F18's point is that the two doors
 *       cannot legally disagree, which is what makes ONE shared resolver correct rather than
 *       merely tidy — CratonVM serves them from two different tables (F10-1 NOMINATION 2:
 *       {@code t27_tls}'s {@code getPeerCertificates} reads the session-keyed table, and
 *       {@code ssl_security}'s {@code getPeerPrincipal} reads the socket registry, which has
 *       no entry for a connection that was never a registered socket). Family
 *       {@code handshake}, both directions of {@code equals}.
 * </ul>
 *
 * <p>It also supplies the first executable form of three things that were previously only
 * predictions:
 *
 * <ul>
 *   <li><b>F10-1 §6</b> — a completed HTTPS handshake must report {@code isValid() == true}
 *       and a 32-byte id. CratonVM's two minters wrote {@code -1} into the session's stream-id
 *       slot, which its own {@code session_has_negotiated} predicate reads as "negotiated
 *       nothing", so both answers were the null session's. F10 replaced the literal with a
 *       marker constant; nothing has ever executed the result.
 *   <li><b>F10-1 NOMINATION 1</b> — {@code getSSLSession()} must return the SAME object twice.
 *       CratonVM mints a fresh session per accessor call, which was invisible while every call
 *       returned {@code byte[0]} and is the most visible remaining divergence once the ids are
 *       real. Row {@code client.sslSession.sameObjectTwice}.
 *   <li><b>E31-1 §2</b> on a session that is WIDE — slot 3 is the peer host on the 6- and
 *       8-field session shapes and the ATTRIBUTE MAP on the 4-field one. {@code RSslNullSession}
 *       arms that trap on the narrow shape; the {@code attrs} family here arms it on the shape a
 *       real handshake produces, which is a different row in the width table and therefore a
 *       different bug.
 * </ul>
 *
 * <h2>The measured contract — HotSpot 25.0.3+9-LTS {@code Microsoft-13877124}, this host</h2>
 *
 * <pre>
 *   completed client session (HttpsURLConnection.getSSLSession):
 *     isValid()               = true
 *     getId().length          = 32,  equal content across calls, DIFFERENT array object
 *     getCipherSuite()        = TLS_AES_256_GCM_SHA384   (the negotiated name, not asserted -- see below)
 *     getProtocol()           = TLSv1.3
 *     getSessionContext()     = sun.security.ssl.SSLSessionContextImpl   (NON-null)
 *     getPeerPrincipal()      = CN=localhost, a javax.security.auth.x500.X500Principal
 *                               and .equals() the leaf certificate's subject, both directions
 *     getPeerCertificates()   = 1 certificate
 *     getLocalPrincipal()     = null      (a client with no configured identity)
 *     getLocalCertificates()  = null
 *     getPeerHost()/getPeerPort() = localhost / the server's port
 *   after invalidate() on that session:
 *     isValid()               = false
 *     getSessionContext()     = null      &lt;-- THE ROW THIS FILE EXISTS FOR
 *     everything else         byte-for-byte unchanged, and a second invalidate() is a no-op
 *   two connections through ONE SSLContext:
 *     ids differ              (TLS 1.3 mints a new session id per handshake)
 *   the SERVER's view of the same handshake:
 *     isValid()               = true, 32-byte id, non-null context
 *     getLocalPrincipal()     = CN=localhost
 *     getPeerPrincipal()      THROWS SSLPeerUnverifiedException: peer not authenticated
 * </pre>
 *
 * <p>That last block is the sharpest vector in the file: ONE session, genuinely negotiated,
 * answers {@code getLocalPrincipal()} with a principal and {@code getPeerPrincipal()} with a
 * refusal. "Negotiated" and "peer authenticated" are different questions, and an implementation
 * that answers the second from the first is wrong in whichever direction it guesses.
 *
 * <h2>Determinism: what is asserted and what is deliberately not</h2>
 *
 * <p>The suite diffs both VMs' {@code CK} lines byte for byte, so nothing per-run may be
 * printed. No port, no session id, no host name derived from the peer address reaches a row.
 *
 * <p><b>The negotiated cipher-suite NAME is not asserted, on purpose.</b> CratonVM's TLS is
 * rustls and HotSpot's is JSSE; their preference orders are free to differ and a disagreement
 * there is not a defect this file is written to catch. What IS asserted is everything a
 * fabrication would have to survive: that the suite is not the {@code SSL_NULL_WITH_NULL_NULL}
 * sentinel (E12-1's finding is that CratonVM once fabricated a REAL, strong, supported suite
 * name for a session that had negotiated nothing — so the sentinel row runs in the opposite
 * direction here), and that {@code HttpsURLConnection.getCipherSuite()} and
 * {@code SSLSession.getCipherSuite()} — two doors, two registrars — return the SAME string.
 * The protocol is asserted as membership of {TLSv1.2, TLSv1.3} plus a not-{@code NONE} row for
 * the same reason.
 *
 * <p>{@code getApplicationBufferSize()} is deliberately absent: F18 §8.3 records CratonVM's
 * 16384 as a KNOWING under-report (HotSpot measures 16676 negotiated, 16704 null), so a row
 * would be red for a reason this file does not own. Recorded so it is not "helpfully" added.
 *
 * <h2>Network shape — loopback only, bounded, and loud</h2>
 *
 * <p>The server is a {@code javax.net.ssl.SSLServerSocket} bound to {@code 127.0.0.1:0} on a
 * daemon thread, speaking four hand-written HTTP/1.1 responses. Nothing resolves or dials an
 * external host. {@code RJdkNet}, {@code RChannelInterrupt}, {@code RSocketChannelInterrupt}
 * and {@code RJdkAsyncChannel} already bind loopback sockets, so this is the suite's existing
 * shape and not a new class of dependency. {@code com.sun.net.httpserver} was deliberately NOT
 * used even though the oracle harness this was derived from used it: it would make this the
 * only vector in the suite depending on {@code jdk.httpserver}, and every question here can be
 * asked through {@code javax.net.ssl} alone.
 *
 * <p><b>The key material is generated in this process and never touches the disk.</b> The
 * X.509 certificate below is assembled as DER by hand and signed with
 * {@code Signature.getInstance("SHA256withRSA")} — all public API. This is why the vector needs
 * no resource file, no {@code keytool} step and no expiry date, and it corrects
 * {@code jdk-only-coverage.txt} §3 and {@code RJdkSecurity}'s own header, both of which say a
 * portable self-signed certificate "requires internal sun.security APIs". It does not.
 *
 * <p><b>Failure is loud, never a hang.</b> Every socket carries a 20 s timeout, a daemon
 * watchdog halts the VM after 90 s naming the phase it died in, and {@code main} catches every
 * {@code Throwable}, reports it on the {@code CK} prefix and calls {@code System.exit(1)} —
 * because a fixture that throws while a non-daemon accept loop is alive hangs until the
 * harness's own {@code TIMEOUT} kills it, and a timeout is a much worse diagnosis than an
 * assertion.
 *
 * <h2>The read-ordering trap, measured</h2>
 *
 * <p>Once the response body is drained, HotSpot returns the connection to its
 * {@code KeepAliveCache} and EVERY connection-level accessor throws
 * {@code IllegalStateException: connection not yet open} — the same exception it throws for a
 * connection that never handshaked. So the order of the reads below is part of the test, and
 * the {@code drainTrap} family pins it: an app that drains and THEN asks gets an exception on
 * HotSpot too, and a later lane must not file that as a CratonVM defect. The session object
 * itself survives the recycle, which is the row that separates the two claims.
 */
public class RSslLiveSession {

    static int checks = 0;
    static int failures = 0;
    static int mark = 0;

    static void ck(String what, Object got, Object want) {
        checks++;
        boolean ok = (got == null) ? (want == null) : got.equals(want);
        if (!ok) {
            failures++;
        }
        System.out.println("CK RSslLiveSession " + what + " = " + got
                + (ok ? "" : "  WANT " + want));
    }

    static void sectionEnd(String name, int expected) {
        int n = checks - mark;
        mark = checks;
        if (n != expected) {
            throw new AssertionError(
                    "block " + name + " ran " + n + " checks, header says " + expected);
        }
        System.out.println("CK RSslLiveSession " + name + "=" + n);
    }

    interface Op {
        Object run() throws Exception;
    }

    /** The class of whatever {@code op} raised, or {@code "none"} — a door that ANSWERS. */
    static String raised(Op op) {
        try {
            op.run();
            return "none";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    /** The MESSAGE of whatever {@code op} raised, or {@code "none"} when it did not raise. */
    static String message(Op op) {
        try {
            op.run();
            return "none";
        } catch (Throwable t) {
            return String.valueOf(t.getMessage());
        }
    }

    // ---------------------------------------------------------------------------------------
    // A self-signed CN=localhost certificate, built as DER in this process.
    //
    // Only the fields JSSE reads are emitted: v3, a fixed serial, sha256WithRSAEncryption, a
    // one-RDN issuer/subject, a validity window wide enough that this vector cannot expire, the
    // public key in its own X.509 SubjectPublicKeyInfo encoding (which is exactly what
    // PublicKey.getEncoded() already returns), and ONE extension.
    //
    // The SAN carries dNSName=localhost and DELIBERATELY NO iPAddress. That single omission is
    // what makes the `verifier` family reachable: JSSE's built-in endpoint identification
    // matches https://localhost and CANNOT match https://127.0.0.1, and HttpsURLConnection
    // consults a custom HostnameVerifier ONLY when its own check has already failed. With an
    // iPAddress SAN present the verifier is never invoked and every row about it silently
    // measures a null. See the `verifier` family's own comment — that is not hypothetical, it
    // is how an earlier oracle concluded the verifier's session was a different object.
    // ---------------------------------------------------------------------------------------

    static byte[] tlv(int tag, byte[] body) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        o.write(tag);
        int n = body.length;
        if (n < 128) {
            o.write(n);
        } else if (n < 256) {
            o.write(0x81);
            o.write(n);
        } else {
            o.write(0x82);
            o.write((n >> 8) & 0xff);
            o.write(n & 0xff);
        }
        o.write(body, 0, body.length);
        return o.toByteArray();
    }

    static byte[] cat(byte[]... parts) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        for (byte[] p : parts) {
            o.write(p, 0, p.length);
        }
        return o.toByteArray();
    }

    static byte[] seq(byte[]... parts) {
        return tlv(0x30, cat(parts));
    }

    static byte[] oid(int... enc) {
        byte[] b = new byte[enc.length];
        for (int i = 0; i < enc.length; i++) {
            b[i] = (byte) enc[i];
        }
        return tlv(0x06, b);
    }

    static byte[] ascii(int tag, String s) {
        return tlv(tag, s.getBytes(java.nio.charset.StandardCharsets.US_ASCII));
    }

    static X509Certificate selfSigned(KeyPair kp) throws Exception {
        byte[] alg = seq(oid(0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b),
                new byte[] { 0x05, 0x00 });
        byte[] name = seq(tlv(0x31, seq(oid(0x55, 0x04, 0x03), ascii(0x13, "localhost"))));
        byte[] validity = seq(ascii(0x17, "200101000000Z"), ascii(0x17, "491231235959Z"));
        byte[] san = seq(ascii(0x82, "localhost"));
        byte[] exts = tlv(0xa3, seq(seq(oid(0x55, 0x1d, 0x11), tlv(0x04, san))));
        byte[] tbs = seq(tlv(0xa0, new byte[] { 0x02, 0x01, 0x02 }),
                new byte[] { 0x02, 0x01, 0x2a }, alg, name, validity, name,
                kp.getPublic().getEncoded(), exts);
        Signature signer = Signature.getInstance("SHA256withRSA");
        signer.initSign(kp.getPrivate());
        signer.update(tbs);
        byte[] der = seq(tbs, alg, tlv(0x03, cat(new byte[] { 0 }, signer.sign())));
        return (X509Certificate) CertificateFactory.getInstance("X.509")
                .generateCertificate(new java.io.ByteArrayInputStream(der));
    }

    static final char[] PW = "cratonvm-f36".toCharArray();
    static final int SO_TIMEOUT_MS = 20000;
    static SSLContext serverCtx;
    static SSLContext clientCtx;
    static X509Certificate leaf;

    static void buildContexts() throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        leaf = selfSigned(kp);
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, null);
        ks.setKeyEntry("k", kp.getPrivate(), PW, new Certificate[] { leaf });
        KeyStore ts = KeyStore.getInstance("PKCS12");
        ts.load(null, null);
        ts.setCertificateEntry("ca", leaf);
        KeyManagerFactory kmf =
                KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, PW);
        serverCtx = SSLContext.getInstance("TLS");
        serverCtx.init(kmf.getKeyManagers(), null, null);
        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ts);
        clientCtx = SSLContext.getInstance("TLS");
        clientCtx.init(null, tmf.getTrustManagers(), null);
    }

    // ---- the loopback server ---------------------------------------------------------------

    /** The server's view of the FIRST handshake, captured on the accept thread. */
    static final java.util.List<Object[]> serverRows =
            java.util.Collections.synchronizedList(new java.util.ArrayList<Object[]>());
    static volatile String serverError = "none";
    static volatile String phase = "startup";

    static void row(String what, Object got, Object want) {
        serverRows.add(new Object[] { what, got, want });
    }

    /**
     * Accept {@code n} connections, capturing the server-side session of the first, and answer
     * each with a fixed 2-byte body. Daemon: a fixture that fails an assertion must not be kept
     * alive by its own server.
     */
    static void serve(SSLServerSocket ss, int n) {
        Thread t = new Thread(() -> {
            int served = 0;
            int attempts = 0;
            while (served < n && attempts < n + 32) {
                attempts++;
                SSLSocket s = null;
                try {
                    s = (SSLSocket) ss.accept();
                    s.setSoTimeout(SO_TIMEOUT_MS);
                    InputStream in = s.getInputStream();
                    int state = 0;
                    while (state < 4) {              // consume the request head, CRLF CRLF
                        int b = in.read();
                        if (b < 0) {
                            break;
                        }
                        if ((state == 0 || state == 2) && b == '\r') {
                            state++;
                        } else if ((state == 1 || state == 3) && b == '\n') {
                            state++;
                        } else {
                            state = 0;
                        }
                    }
                    if (serverRows.isEmpty()) {
                        capture(s.getSession());
                    }
                    OutputStream out = s.getOutputStream();
                    out.write(("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n"
                            + "Connection: close\r\n\r\nok")
                            .getBytes(java.nio.charset.StandardCharsets.ISO_8859_1));
                    out.flush();
                    served++;
                    // MEASURED, and it is worth 60 SECONDS on this vector. SSLSocket.close()
                    // performs the TLS closure handshake: it sends close_notify and then
                    // waits for the peer's, bounded by SO_TIMEOUT. This vector deliberately
                    // leaves response bodies unread until the drainTrap family, so the client
                    // sends no close_notify until then -- and the accept thread, which is the
                    // only thing that can accept the NEXT connection, sat in close() for the
                    // full 20 s each time. Four connections, ~62 s per run against the
                    // harness's 120 s TIMEOUT. Shrinking the timeout to 200 ms for the
                    // closure handshake alone bounds it: the response is already flushed, so
                    // nothing the client still needs depends on a graceful close.
                    s.setSoTimeout(200);
                } catch (java.net.SocketTimeoutException retry) {
                    // MEASURED, and the reason this is not a plain for-loop: the accept
                    // timeout must not CONSUME a connection slot. On a loaded box more than
                    // SO_TIMEOUT_MS can elapse between two of this vector's four
                    // connections while the main thread is asserting, and a loop that
                    // counted the timeout as an iteration would leave a later connect()
                    // with no acceptor -- a 20 s stall per missing slot, and then a hang
                    // that only the watchdog ends. Retrying is bounded by `attempts` so a
                    // genuinely dead client still terminates the thread.
                    //
                    // serverError is deliberately NOT written here: a retry is not an error,
                    // and assigning "none" would clobber a real one recorded earlier.
                } catch (Throwable e) {
                    serverError = e.getClass().getName() + ": " + e.getMessage();
                } finally {
                    if (s != null) {
                        try {
                            s.close();
                        } catch (Throwable ignored) {
                            // A timeout waiting for the peer's close_notify is EXPECTED here
                            // (see above) and must not be reported as a server error: the
                            // response was already flushed, so the exchange succeeded.
                        }
                    }
                }
            }
        }, "RSslLiveSession-server");
        t.setDaemon(true);
        t.start();
    }

    /**
     * The server side of a completed handshake in which the CLIENT presented no certificate.
     *
     * <p>Captured here rather than asserted here because it runs on the accept thread: a
     * failure would otherwise be a stack trace on a thread nobody joins. The rows are replayed
     * through {@code ck} on the main thread by {@link #serverSide()}.
     */
    static void capture(SSLSession s) {
        row("server.session.isNull", s == null, Boolean.FALSE);
        if (s == null) {
            return;
        }
        row("server.isValid", s.isValid(), Boolean.TRUE);
        row("server.getId.length", s.getId().length, 32);
        row("server.sessionContext.isNull", s.getSessionContext() == null, Boolean.FALSE);
        row("server.localPrincipal", String.valueOf(s.getLocalPrincipal()), "CN=localhost");
        row("server.localPrincipal.class",
                s.getLocalPrincipal() == null ? "null" : s.getLocalPrincipal().getClass().getName(),
                "javax.security.auth.x500.X500Principal");
        row("server.localCertificates.length",
                s.getLocalCertificates() == null ? -1 : s.getLocalCertificates().length, 1);
        // THE PAIR. One negotiated session; the local identity resolves and the peer's refuses.
        row("server.peerPrincipal.raises", raised(s::getPeerPrincipal),
                "javax.net.ssl.SSLPeerUnverifiedException");
        row("server.peerPrincipal.message", message(s::getPeerPrincipal),
                "peer not authenticated");
        row("server.peerCertificates.raises", raised(s::getPeerCertificates),
                "javax.net.ssl.SSLPeerUnverifiedException");
        row("server.peerCertificates.message", message(s::getPeerCertificates),
                "peer not authenticated");
        row("server.cipherSuite.isNullSentinel",
                "SSL_NULL_WITH_NULL_NULL".equals(s.getCipherSuite()), Boolean.FALSE);
        row("server.peerPort.isPositive", s.getPeerPort() > 0, Boolean.TRUE);
        row("server.valueNames.length", s.getValueNames().length, 0);
    }

    // ---- the client doors --------------------------------------------------------------------

    static int port;

    static HttpsURLConnection open(String host, HostnameVerifier hv) throws Exception {
        URI u = URI.create("https://" + host + ":" + port + "/live");
        HttpsURLConnection c = (HttpsURLConnection) u.toURL().openConnection();
        c.setSSLSocketFactory(clientCtx.getSocketFactory());
        c.setConnectTimeout(SO_TIMEOUT_MS);
        c.setReadTimeout(SO_TIMEOUT_MS);
        if (hv != null) {
            c.setHostnameVerifier(hv);
        }
        c.connect();
        return c;
    }

    static HttpsURLConnection c1;
    static SSLSession s1;
    static byte[] id1;

    /** DOOR 1 — the session of a completed handshake, read BEFORE the body is drained. */
    static void handshake() throws Exception {
        phase = "handshake";
        c1 = open("localhost", null);
        ck("client.responseCode", c1.getResponseCode(), 200);
        Optional<SSLSession> o1 = c1.getSSLSession();
        ck("client.sslSession.isPresent", o1.isPresent(), Boolean.TRUE);
        // F10-1 NOMINATION 1. HotSpot hands back ONE object; CratonVM's accessors each call
        // https_session_object, which allocates. That was invisible while every call answered
        // byte[0] and is visible the moment the ids are real, because getId() is seeded from
        // the object's identity — two calls would then disagree about the same connection.
        ck("client.sslSession.sameObjectTwice", o1.get() == c1.getSSLSession().get(),
                Boolean.TRUE);
        s1 = o1.get();

        ck("client.isValid", s1.isValid(), Boolean.TRUE);
        ck("client.getId.length", s1.getId().length, 32);
        id1 = s1.getId();
        ck("client.getId.twiceEqualContent", Arrays.equals(id1, s1.getId()), Boolean.TRUE);
        // JSSE clones defensively, so a caller cannot mutate the session's id. An accessor that
        // handed out its own array would pass the row above and fail this one.
        ck("client.getId.twiceSameArray", id1 == s1.getId(), Boolean.FALSE);

        // Not the negotiated NAME — see the header. The sentinel row runs in the direction
        // E12-1 could not: there, a fabricated real suite name stood in for "nothing
        // negotiated"; here a real negotiation must not report the sentinel.
        ck("client.cipherSuite.isNullSentinel",
                "SSL_NULL_WITH_NULL_NULL".equals(s1.getCipherSuite()), Boolean.FALSE);
        ck("client.cipherSuite.matchesConnection",
                c1.getCipherSuite().equals(s1.getCipherSuite()), Boolean.TRUE);
        ck("client.protocol.isModernTls",
                "TLSv1.3".equals(s1.getProtocol()) || "TLSv1.2".equals(s1.getProtocol()),
                Boolean.TRUE);
        ck("client.protocol.isNoneSentinel", "NONE".equals(s1.getProtocol()), Boolean.FALSE);

        ck("client.sessionContext.isNull", s1.getSessionContext() == null, Boolean.FALSE);
        ck("client.sessionContext.isSSLSessionContext",
                s1.getSessionContext() instanceof SSLSessionContext, Boolean.TRUE);

        ck("client.peerCertificates.length", s1.getPeerCertificates().length, 1);
        ck("client.peerPrincipal.class",
                s1.getPeerPrincipal() == null ? "null" : s1.getPeerPrincipal().getClass().getName(),
                "javax.security.auth.x500.X500Principal");
        ck("client.peerPrincipal.name", String.valueOf(s1.getPeerPrincipal()), "CN=localhost");
        // F18's headline, both directions. equals() is not symmetric by accident here: these are
        // two DIFFERENT objects that must compare equal, and CratonVM derives them from two
        // different tables, so an implementation can get one direction and not the other.
        Principal leafSubject =
                ((X509Certificate) s1.getPeerCertificates()[0]).getSubjectX500Principal();
        ck("client.peerPrincipal.equalsLeafSubject", s1.getPeerPrincipal().equals(leafSubject),
                Boolean.TRUE);
        ck("client.leafSubject.equalsPeerPrincipal", leafSubject.equals(s1.getPeerPrincipal()),
                Boolean.TRUE);

        // A client with no configured identity has neither, and "null" is the contract — not an
        // empty array, and not the server's own certificate reflected back.
        ck("client.localPrincipal", String.valueOf(s1.getLocalPrincipal()), "null");
        ck("client.localCertificates", s1.getLocalCertificates() == null ? "null" : "array",
                "null");

        ck("client.peerHost", s1.getPeerHost(), "localhost");
        ck("client.peerPort.isServerPort", s1.getPeerPort() == port, Boolean.TRUE);
        ck("client.valueNames.length", s1.getValueNames().length, 0);

        // The connection-level twins of two session accessors. Separate registrations in this
        // tree, and the whole point of F10-1 NOMINATION 2 is that neighbouring doors on the same
        // completed handshake read different sources.
        ck("client.conn.serverCertificates.length", c1.getServerCertificates().length, 1);
        ck("client.conn.peerPrincipal", String.valueOf(c1.getPeerPrincipal()), "CN=localhost");
        sectionEnd("handshake", 25);
    }

    /**
     * The attribute map on a session that NEGOTIATED, and the three identity doors it must not
     * shadow.
     *
     * <p>E31-1 §2: slot 3 is the peer host on the wide session shapes and the ATTRIBUTE MAP on
     * the narrow one, so a width-blind reader answers a {@code java.util.HashMap} through a
     * {@code ()Ljava/lang/String;} descriptor as soon as anything has called {@code putValue} —
     * and Jetty's {@code SecureRequestCustomizer.retrieveSni()} does, on every SSL request.
     * {@code RSslNullSession} arms that trap on the NARROW shape. This arms it on the shape a
     * handshake produces, where {@code getPeerHost()} has a real answer to lose.
     *
     * <p>The null contract is F25-1 NOMINATION 4, and the pair worth having is
     * {@code putValue}'s PLURAL "arguments can not be null" against {@code getValue}'s SINGULAR
     * "argument can not be null" — two methods, two strings, one letter apart. A single shared
     * constant is wrong in one of the two places and only a message row can see it.
     */
    static void attrs() throws Exception {
        phase = "attrs";
        ck("attrs.putValue.nullName.raises", raised(() -> {
            s1.putValue(null, "v");
            return null;
        }), "java.lang.IllegalArgumentException");
        ck("attrs.putValue.nullName.message", message(() -> {
            s1.putValue(null, "v");
            return null;
        }), "arguments can not be null");
        ck("attrs.putValue.nullValue.raises", raised(() -> {
            s1.putValue("k", null);
            return null;
        }), "java.lang.IllegalArgumentException");
        ck("attrs.putValue.nullValue.message", message(() -> {
            s1.putValue("k", null);
            return null;
        }), "arguments can not be null");
        ck("attrs.getValue.null.raises", raised(() -> s1.getValue(null)),
                "java.lang.IllegalArgumentException");
        ck("attrs.getValue.null.message", message(() -> s1.getValue(null)),
                "argument can not be null");
        ck("attrs.removeValue.null.raises", raised(() -> {
            s1.removeValue(null);
            return null;
        }), "java.lang.IllegalArgumentException");
        ck("attrs.removeValue.null.message", message(() -> {
            s1.removeValue(null);
            return null;
        }), "argument can not be null");

        // Rows 9-12 establish that the attribute really LANDED. Without them the three shadow
        // rows below pass for a VM whose putValue silently did nothing.
        ck("attrs.putValue.raises", raised(() -> {
            s1.putValue("cratonvm.f36", "v");
            return null;
        }), "none");
        ck("attrs.getValue", String.valueOf(s1.getValue("cratonvm.f36")), "v");
        ck("attrs.getValue.class",
                s1.getValue("cratonvm.f36") == null
                        ? "null" : s1.getValue("cratonvm.f36").getClass().getName(),
                "java.lang.String");
        ck("attrs.valueNames", Arrays.toString(s1.getValueNames()), "[cratonvm.f36]");

        // The trap is now armed. String.valueOf of a HashMap is "{cratonvm.f36=v}", so a
        // width-blind slot-3 read is visible rather than merely wrong.
        ck("attrs.shadow.peerHost", s1.getPeerHost(), "localhost");
        ck("attrs.shadow.peerPort.isServerPort", s1.getPeerPort() == port, Boolean.TRUE);
        ck("attrs.shadow.sessionContext.isNull", s1.getSessionContext() == null, Boolean.FALSE);

        ck("attrs.removeValue.raises", raised(() -> {
            s1.removeValue("cratonvm.f36");
            return null;
        }), "none");
        ck("attrs.removed.valueNames", Arrays.toString(s1.getValueNames()), "[]");
        sectionEnd("attrs", 17);
    }

    static HttpsURLConnection c2;
    static SSLSession s2;

    /**
     * A SECOND connection through the SAME {@code SSLContext}.
     *
     * <p>TLS 1.3 mints a new session id per handshake, so "same context implies same id" is not
     * a thing to model. The row matters because both ids were {@code byte[0]} on CratonVM
     * before F10 — equal, and equal for the wrong reason. This is the row that distinguishes
     * "two real ids" from "two empty ones".
     */
    static void distinct() throws Exception {
        phase = "distinct";
        c2 = open("localhost", null);
        ck("distinct.second.responseCode", c2.getResponseCode(), 200);
        s2 = c2.getSSLSession().get();
        ck("distinct.second.isValid", s2.isValid(), Boolean.TRUE);
        ck("distinct.second.getId.length", s2.getId().length, 32);
        ck("distinct.idsDiffer", !Arrays.equals(id1, s2.getId()), Boolean.TRUE);
        ck("distinct.second.peerPrincipal", String.valueOf(s2.getPeerPrincipal()), "CN=localhost");
        ck("distinct.second.sessionContext.isNull", s2.getSessionContext() == null,
                Boolean.FALSE);
        sectionEnd("distinct", 6);
    }

    /**
     * {@code invalidate()} on a session that GENUINELY NEGOTIATED — the half
     * {@code RSslNullSession} structurally cannot reach.
     *
     * <p>F18-1 §3.1 measured that it moves TWO accessors, correcting an "isValid and nothing
     * else" claim repeated in four comments across three files. The second is
     * {@code getSessionContext()}, and it can only be seen where a live context exists to be
     * dropped. Everything else is byte-for-byte unchanged, which is the other half of the same
     * finding: {@code invalidate()} is not a reset.
     */
    static void invalidateLive() throws Exception {
        phase = "invalidate";
        ck("inv.before.sessionContext.isNull", s2.getSessionContext() == null, Boolean.FALSE);
        byte[] before = s2.getId();
        String cipherBefore = s2.getCipherSuite();
        String protoBefore = s2.getProtocol();
        String peerBefore = String.valueOf(s2.getPeerPrincipal());
        ck("inv.raises", raised(() -> {
            s2.invalidate();
            return null;
        }), "none");
        ck("inv.isValid", s2.isValid(), Boolean.FALSE);
        // THE ROW.
        ck("inv.sessionContext.isNull", s2.getSessionContext() == null, Boolean.TRUE);
        ck("inv.getId.unchanged", Arrays.equals(before, s2.getId()), Boolean.TRUE);
        ck("inv.getId.length", s2.getId().length, 32);
        ck("inv.cipherSuite.unchanged", cipherBefore.equals(s2.getCipherSuite()), Boolean.TRUE);
        ck("inv.protocol.unchanged", protoBefore.equals(s2.getProtocol()), Boolean.TRUE);
        ck("inv.peerPrincipal.unchanged",
                peerBefore.equals(String.valueOf(s2.getPeerPrincipal())), Boolean.TRUE);
        ck("inv.peerCertificates.length", s2.getPeerCertificates().length, 1);
        ck("inv.twice.raises", raised(() -> {
            s2.invalidate();
            return null;
        }), "none");
        ck("inv.twice.isValid", s2.isValid(), Boolean.FALSE);
        // Per-session, not per-context: the OTHER live session keeps both accessors. Without
        // these two rows an implementation that invalidated the whole context passes every row
        // above.
        ck("inv.other.stillValid", s1.isValid(), Boolean.TRUE);
        ck("inv.other.sessionContext.isNull", s1.getSessionContext() == null, Boolean.FALSE);
        sectionEnd("invalidate", 14);
    }

    /**
     * The {@code HostnameVerifier} door — {@code huc_verify_hostname} in CratonVM, the SECOND of
     * the two minters F10-1 repaired, and the one where the old value was self-contradicting: a
     * verifier is invoked to decide whether to ACCEPT the peer, and one that asked
     * {@code session.isValid()} was told the handshake it had been invoked to vet had not
     * happened.
     *
     * <p><b>Reaching this door at all is the delicate part.</b> HttpsURLConnection consults a
     * custom verifier ONLY after its own endpoint identification has failed, so the URL here is
     * the IP literal and the certificate deliberately carries no iPAddress SAN. Row 10 is the
     * control: the SAME verifier, on a URL the built-in check DOES match, is never called.
     *
     * <p>That control is not decoration. An earlier oracle set a verifier, connected to
     * {@code localhost} with a certificate that matched, captured a null, compared it against
     * {@code getSSLSession()} and concluded the verifier's session was a DIFFERENT OBJECT. It
     * is the same object — measured here, with the invocation itself asserted first so the
     * comparison cannot be made against a null again.
     */
    static void verifier() throws Exception {
        phase = "verifier";
        final SSLSession[] seen = new SSLSession[1];
        final String[] host = new String[1];
        HttpsURLConnection c3 = open("127.0.0.1", (h, sess) -> {
            host[0] = h;
            seen[0] = sess;
            return true;
        });
        ck("verifier.responseCode", c3.getResponseCode(), 200);
        ck("verifier.invoked", seen[0] != null, Boolean.TRUE);
        ck("verifier.hostArg", host[0], "127.0.0.1");
        SSLSession s3 = c3.getSSLSession().get();
        ck("verifier.sameObjectAsGetSSLSession", seen[0] == s3, Boolean.TRUE);
        ck("verifier.isValid", seen[0].isValid(), Boolean.TRUE);
        ck("verifier.getId.length", seen[0].getId().length, 32);
        ck("verifier.peerPrincipal", String.valueOf(seen[0].getPeerPrincipal()), "CN=localhost");
        ck("verifier.cipherSuite.isNullSentinel",
                "SSL_NULL_WITH_NULL_NULL".equals(seen[0].getCipherSuite()), Boolean.FALSE);
        ck("verifier.sessionContext.isNull", seen[0].getSessionContext() == null, Boolean.FALSE);

        final SSLSession[] unused = new SSLSession[1];
        HttpsURLConnection c4 = open("localhost", (h, sess) -> {
            unused[0] = sess;
            return true;
        });
        ck("verifier.control.responseCode", c4.getResponseCode(), 200);
        ck("verifier.control.notInvokedWhenBuiltInMatches", unused[0] == null, Boolean.TRUE);
        sectionEnd("verifier", 11);
    }

    /** Replay the accept thread's rows on the main thread, where a failure is countable. */
    static void serverSide() {
        phase = "serverSide";
        for (Object[] r : serverRows) {
            ck((String) r[0], r[1], r[2]);
        }
        ck("server.error", serverError, "none");
        sectionEnd("serverSide", 15);
    }

    /**
     * The read-ordering trap, pinned so it is not re-diagnosed.
     *
     * <p>MEASURED on HotSpot: once the body is drained the connection returns to the
     * {@code KeepAliveCache}, its delegate is released, and every CONNECTION-level accessor
     * throws {@code IllegalStateException: connection not yet open} — the same exception a
     * never-handshaked connection throws. The SESSION object survives, which is the row that
     * separates "the connection was recycled" from "the session was destroyed".
     *
     * <p>This family asserts HotSpot's recycle behaviour, so a CratonVM divergence here is a
     * weaker finding than the other six families: answering the session after a drain is
     * arguably friendlier. It is asserted anyway because the harness diffs both VMs' output
     * regardless, and a row with this comment attached is cheaper to adjudicate than a bare
     * difference. Runs LAST, because draining is destructive to the rows above.
     */
    static void drainTrap() throws Exception {
        phase = "drainTrap";
        InputStream in = c1.getInputStream();
        ByteArrayOutputStream body = new ByteArrayOutputStream();
        int b;
        while ((b = in.read()) != -1) {
            body.write(b);
        }
        in.close();
        ck("drain.body", body.toString("UTF-8"), "ok");
        ck("drain.conn.cipherSuite.raises", raised(c1::getCipherSuite),
                "java.lang.IllegalStateException");
        ck("drain.conn.cipherSuite.message", message(c1::getCipherSuite),
                "connection not yet open");
        ck("drain.conn.sslSession.raises", raised(c1::getSSLSession),
                "java.lang.IllegalStateException");
        ck("drain.conn.sslSession.message", message(c1::getSSLSession),
                "connection not yet open");
        ck("drain.session.isValid", s1.isValid(), Boolean.TRUE);
        ck("drain.session.getId.length", s1.getId().length, 32);
        sectionEnd("drainTrap", 7);
    }

    public static void main(String[] args) throws Exception {
        // A fixture that hangs reports nothing; a fixture that halts reports the phase. The
        // watchdog is the last line of defence behind the per-socket timeouts, not the first.
        Thread wd = new Thread(() -> {
            try {
                Thread.sleep(90000);
            } catch (InterruptedException e) {
                return;
            }
            System.out.println("CK RSslLiveSession FAILED watchdog-90s phase=" + phase);
            System.out.flush();
            Runtime.getRuntime().halt(4);
        }, "RSslLiveSession-watchdog");
        wd.setDaemon(true);
        wd.start();

        SSLServerSocket ss = null;
        try {
            buildContexts();
            ss = (SSLServerSocket) serverCtx.getServerSocketFactory()
                    .createServerSocket(0, 8, InetAddress.getByName("127.0.0.1"));
            ss.setSoTimeout(SO_TIMEOUT_MS);
            port = ss.getLocalPort();
            serve(ss, 4);

            handshake();
            attrs();
            distinct();
            invalidateLive();
            verifier();
            serverSide();
            drainTrap();          // destructive; last
        } catch (Throwable t) {
            // The whole reason this is caught rather than thrown: the accept thread is a daemon
            // but an escaping throwable still leaves the VM to unwind on its own timetable, and
            // the harness would then report a TIMEOUT where an assertion is the real diagnosis.
            System.out.println("CK RSslLiveSession FAILED phase=" + phase + " "
                    + t.getClass().getName() + ": " + t.getMessage());
            System.out.flush();
            t.printStackTrace();
            System.exit(1);
        } finally {
            if (ss != null) {
                try {
                    ss.close();
                } catch (Throwable ignored) {
                    // closing the listener is best-effort; the verdict is already decided
                }
            }
        }

        // SEPARATE lines, and `fails` first so the LAST word of the counted line is the count.
        // A combined `checks=N fails=M` line makes harness_check_count return the STRING
        // "N fails=M", G3's `-eq 0` becomes a syntax error swallowed by its own 2>/dev/null,
        // and the guard silently NO-OPS. See regression-suite/harness-guard.sh.
        System.out.println("CK RSslLiveSession fails=" + failures);
        System.out.println("CK RSslLiveSession checks=" + checks);
        if (failures != 0) {
            System.out.flush();
            System.exit(1);
        }
        System.out.println("PASS RSslLiveSession (" + checks + " checks)");
        System.out.flush();
        System.exit(0);
    }
}
