import java.security.cert.Certificate;
import java.util.Arrays;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLPeerUnverifiedException;
import javax.net.ssl.SSLSession;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;

/**
 * The JSSE contract for a session that has negotiated NOTHING, asked through every door that can
 * hand one out without a network.
 *
 * <h2>What this vector exists to catch</h2>
 *
 * <p>Diagnosed in {@code docs/known-issues/jdk-only/E12-1-the-null-session-and-the-fabricated-cipher.md}.
 * CratonVM's {@code SSLSession} accessors carried {@code unwrap_or_else} fallbacks for the
 * "no handshake has happened" state, and the fallbacks were not JSSE's answers:
 *
 * <ul>
 *   <li>{@code "TLS_AES_256_GCM_SHA384"} and {@code "TLS_AES_128_GCM_SHA256"} — <b>fabricated</b>.
 *       Not merely wrong: these are real, strong, <i>supported</i> suite names, so a caller cannot
 *       tell them from a genuine negotiation. Security-sensitive code branches on this string.
 *   <li>{@code "UNKNOWN"} / {@code "?"} / {@code "TLS"} — not JSSE vocabulary at all.
 *   <li>a 32-byte pseudo-random {@code getId()} for a session that has no id.
 *   <li>{@code SSLSocket.getSession()} returning <b>null</b>, which JSSE forbids — the caller's
 *       {@code getSession().getCipherSuite()} got a {@code NullPointerException} where HotSpot has
 *       a defined answer.
 * </ul>
 *
 * <p>HotSpot 25.0.3+9-LTS, measured (scratchpad/e12):
 *
 * <pre>
 *   unconnected SSLSocket / pre-handshake SSLEngine:
 *     getSession()            = sun.security.ssl.SSLSessionImpl   (never null)
 *     getCipherSuite()        = SSL_NULL_WITH_NULL_NULL
 *     getProtocol()           = NONE
 *     getId()                 = byte[0]
 *     isValid()               = false
 *     getPeerCertificates()   THROWS SSLPeerUnverifiedException
 *     getPeerPrincipal()      THROWS SSLPeerUnverifiedException
 *     getLocalCertificates()  = null
 *     getLocalPrincipal()     = null
 *     getPacketBufferSize()   = 16709
 *     getHandshakeSession()   = null   (on the socket and on the engine)
 * </pre>
 *
 * <h2>Why the sentinel is honesty and not a fabrication</h2>
 *
 * <p>{@code --jdk-only} exists to refuse invented stand-ins. {@code SSL_NULL_WITH_NULL_NULL} is not
 * one: it is the JDK's own answer for "no cipher negotiated", and it is <b>unofferable</b> —
 * {@link #unofferable()} is the executable form of that argument. A value that cannot be selected
 * can never be the outcome of a handshake, so returning it can never be mistaken for one. A
 * fabricated suite name has exactly the opposite property.
 *
 * <h2>NO NETWORK</h2>
 *
 * <p>Every arm uses an object that was never connected. Nothing here binds, listens, resolves or
 * dials, so the vector is safe in a sandboxed or offline CI and cannot flake on a port.
 */
public class RSslNullSession {

    static int checks = 0;
    static int failures = 0;

    static void ck(String what, Object got, Object want) {
        checks++;
        boolean ok = (got == null) ? (want == null) : got.equals(want);
        if (!ok) {
            failures++;
        }
        System.out.println("CK RSslNullSession " + what + " = " + got + (ok ? "" : "  WANT " + want));
    }

    /** A session that has negotiated nothing, whichever door produced it. */
    static void nullSession(String door, SSLSession s) {
        checks++;
        if (s == null) {
            failures++;
            System.out.println("CK RSslNullSession " + door + ".session = null  WANT a non-null invalid session");
            return;
        }
        System.out.println("CK RSslNullSession " + door + ".session = non-null");

        ck(door + ".getCipherSuite", s.getCipherSuite(), "SSL_NULL_WITH_NULL_NULL");
        ck(door + ".getProtocol", s.getProtocol(), "NONE");

        byte[] id = s.getId();
        ck(door + ".getId.isNull", id == null, Boolean.FALSE);
        ck(door + ".getId.length", id == null ? -1 : id.length, 0);

        ck(door + ".isValid", s.isValid(), Boolean.FALSE);

        // getPeerCertificates and getPeerPrincipal REFUSE. Note the exception is the SSL one, not
        // IllegalStateException: callers catch SSLPeerUnverifiedException specifically to mean
        // "no peer certificate", and it is the SAME exception a handshaked-but-anonymous peer
        // produces. The unhandshaken and the anonymous cases are deliberately indistinguishable
        // here; that is HotSpot's choice, not an approximation.
        ck(door + ".getPeerCertificates", refusal(() -> s.getPeerCertificates()),
                "javax.net.ssl.SSLPeerUnverifiedException");
        ck(door + ".getPeerPrincipal", refusal(() -> s.getPeerPrincipal()),
                "javax.net.ssl.SSLPeerUnverifiedException");

        // The LOCAL side answers null rather than refusing — the accessors are NOT uniform, and a
        // fix that made all of them throw would be as wrong as one that made all of them answer.
        Certificate[] local = s.getLocalCertificates();
        ck(door + ".getLocalCertificates", local == null ? "null" : "array[" + local.length + "]", "null");
        ck(door + ".getLocalPrincipal", String.valueOf(s.getLocalPrincipal()), "null");

        // Layout-independent protocol constants; the packet size is the same in every state.
        ck(door + ".getPacketBufferSize", s.getPacketBufferSize(), 16709);

        // getValueNames is an EMPTY array, not null.
        String[] names = s.getValueNames();
        ck(door + ".getValueNames", names == null ? "null" : "array[" + names.length + "]", "array[0]");

        // Two reads of getId() must agree — an id, even an empty one, is stable per session.
        ck(door + ".getId.stable", Arrays.equals(s.getId(), s.getId()), Boolean.TRUE);
    }

    static String refusal(ThrowingCall c) {
        try {
            Object v = c.call();
            return "RETURNED " + (v == null ? "null" : v.getClass().getName());
        } catch (SSLPeerUnverifiedException e) {
            return "javax.net.ssl.SSLPeerUnverifiedException";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    interface ThrowingCall {
        Object call() throws Exception;
    }

    /**
     * DOOR 1 — a fresh SSLSocket from the zero-arg {@code createSocket()}, never connected.
     *
     * <p>This is the exact object CratonVM's own {@code SSLSocketFactory.createSocket()V} native
     * mints with a tls id of -1, so the arm reaches the defect rather than reporting its own reach.
     */
    static void unconnectedSocket() throws Exception {
        SSLSocketFactory f = (SSLSocketFactory) SSLSocketFactory.getDefault();
        SSLSocket s = (SSLSocket) f.createSocket();
        ck("socket.isConnected", s.isConnected(), Boolean.FALSE);
        ck("socket.getHandshakeSession", String.valueOf(s.getHandshakeSession()), "null");
        nullSession("socket", s.getSession());
        // getEnabledProtocols is CONFIGURATION, not negotiation: it must never report the
        // session's "nothing negotiated" sentinel, and it must not be a one-element guess.
        String[] en = s.getEnabledProtocols();
        ck("socket.getEnabledProtocols.hasSentinel", Arrays.asList(en).contains("NONE"), Boolean.FALSE);
        ck("socket.getEnabledProtocols", Arrays.toString(en), "[TLSv1.3, TLSv1.2]");
        s.close();
        // Closing an unconnected socket does not change any of it.
        nullSession("socketClosed", s.getSession());
    }

    /** DOOR 2 — an SSLEngine that has never had {@code beginHandshake()} called on it. */
    static void preHandshakeEngine() throws Exception {
        SSLContext c = SSLContext.getInstance("TLS");
        c.init(null, null, null);
        SSLEngine e = c.createSSLEngine();
        e.setUseClientMode(true);
        ck("engine.getHandshakeSession", String.valueOf(e.getHandshakeSession()), "null");
        nullSession("engine", e.getSession());
    }

    /**
     * The argument for the sentinel, mechanised: the value we return for "nothing negotiated" must
     * not be a value we claim to support. A VM whose supported list contained it would have made
     * the sentinel indistinguishable from a negotiation, which is the exact property that makes
     * {@code TLS_AES_256_GCM_SHA384} an unacceptable fallback.
     */
    static void unofferable() throws Exception {
        SSLSocketFactory f = (SSLSocketFactory) SSLSocketFactory.getDefault();
        java.util.List<String> sup = Arrays.asList(f.getSupportedCipherSuites());
        ck("unofferable.sentinelIsSupported", sup.contains("SSL_NULL_WITH_NULL_NULL"), Boolean.FALSE);
        // ... and the fabricated literals ARE supported, which is why they could never serve as a
        // "nothing negotiated" marker. If either of these ever reads false the argument in this
        // file's header has to be re-derived, not the code.
        ck("unofferable.aes256IsSupported", sup.contains("TLS_AES_256_GCM_SHA384"), Boolean.TRUE);
        ck("unofferable.aes128IsSupported", sup.contains("TLS_AES_128_GCM_SHA256"), Boolean.TRUE);
    }

    public static void main(String[] args) throws Exception {
        unconnectedSocket();
        preHandshakeEngine();
        unofferable();
        // SEPARATE lines, and `failures` first so the LAST word of the counted
        // line is the count. These were one line — `CK RSslNullSession
        // checks=47 failures=0` — and that spelling makes the harness BLIND to
        // the count while looking like it publishes one: harness_check_count
        // (regression-suite/harness-guard.sh) does `sub(/^.*checks=/, "");
        // print`, i.e. it takes the whole rest of the line, so it returned the
        // string `47 failures=0`; G3 then evaluates `[ "$gc_count" -eq 0 ]`,
        // which is a syntax error swallowed by its own 2>/dev/null, and the
        // guard silently no-ops. One value per line.
        // docs/known-issues/jdk-only/W8-E9-1-three-broken-oracles-and-the-suite-denominator.md §1
        System.out.println("CK RSslNullSession failures=" + failures);
        System.out.println("CK RSslNullSession checks=" + checks);
        if (failures != 0) {
            throw new AssertionError("RSslNullSession: " + failures + " of " + checks + " checks diverged");
        }
        System.out.println("PASS RSslNullSession (" + checks + " checks)");
    }
}
