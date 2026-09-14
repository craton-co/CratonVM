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
 * <h2>The four doors F18 registered, and why they needed a fixture</h2>
 *
 * <p>{@code invalidate()}, {@code getPeerHost()}, {@code getPeerPort()} and
 * {@code getSessionContext()} had <b>no registration at all</b> until
 * {@code docs/known-issues/jdk-only/F18-1-four-session-doors-with-no-registration-and-the-twin-that-read-another-table-20260813.md}.
 * An unregistered {@code SSLSession} method is not a wrong value — it resolves to the
 * {@code Code}-less interface declaration and raises {@code AbstractMethodError}, which
 * would have aborted DOOR 1 here and taken every later check with it, exactly the way
 * {@code getHandshakeSession} did before E31-1 registered it. F18 §7 recorded that this
 * file asserted <b>nothing</b> about any of the four; that is what the arms below close.
 *
 * <p>HotSpot 25.0.3+9-LTS, measured (scratchpad/f25, both doors identical):
 *
 * <pre>
 *   getPeerHost()           = null      (and NOT the engine's own peer host — see below)
 *   getPeerPort()           = -1        (NOT 0; 0 is an unwritten slot, not a port)
 *   getSessionContext()     = null
 *   invalidate()            returns normally, and moves NOTHING on this session:
 *                           isValid stays false, getSessionContext stays null,
 *                           getId keeps its exact bytes, the suite and protocol
 *                           sentinels are unchanged, and a second call is idempotent
 *   getPeerPrincipal()      SSLPeerUnverifiedException: peer not authenticated
 *   getPeerCertificates()   SSLPeerUnverifiedException: peer not authenticated
 * </pre>
 *
 * <p><b>What this file cannot prove, stated so nobody reads its green as more than it
 * is.</b> F18's headline measurement is that on a session that genuinely negotiated,
 * {@code invalidate()} moves <i>two</i> accessors — {@code isValid()} to false <b>and</b>
 * {@code getSessionContext()} to null — correcting an "isValid and nothing else" claim
 * repeated in four comments across three files. The second half is only visible where a
 * live context exists, and this file opens no connection, so it asserts the other side of
 * the same contract: that {@code invalidate()} does not <i>mint</i> a context, an id or a
 * suite on a session that negotiated nothing. Likewise F18 measured
 * {@code getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal())} — true, and
 * the reason {@code getPeerPrincipal} is not a sibling accessor but the leaf certificate's
 * subject. Both doors refuse here, so that row has no vector without a handshake and is
 * <b>not</b> written; see this lane's record.
 *
 * <p>{@code getApplicationBufferSize()} is measured 16704 on the null session and is
 * deliberately NOT asserted: F18 §8.3 records CratonVM's 16384 as a knowing under-report,
 * so a row here would be red for a reason this file does not own.
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

        // APPENDED, so every row above keeps its position in this door's sequence.
        //
        // Three of F18's four doors. Each is a plain read here and each has a
        // DIFFERENT wrong-answer shape a plausible implementation produces:
        //
        //   getPeerHost   — the engine created with createSSLEngine("localhost", 443)
        //                   answers "localhost" from getPeerHost(), and its SESSION
        //                   still answers null (measured). An implementation that
        //                   forwarded the engine's own peer host into the session
        //                   passes on the no-argument door and fails on that one.
        //                   Worse, slot 3 is the ATTRIBUTE MAP on the 4-field session
        //                   shape, so a width-blind read hands a java.util.HashMap back
        //                   through a ()Ljava/lang/String; descriptor as soon as
        //                   anything has called putValue — E31-1 §2, and the arm
        //                   `attributes` below is the executable form of it.
        //   getPeerPort   — 0 is what an UNWRITTEN slot holds; -1 is what "no peer"
        //                   means. Both wide producers write -1 explicitly, so a 0 here
        //                   is the allocator's fill being reported as an answer, and a
        //                   connected peer never reports port 0 anyway.
        //   getSessionContext — null is the INTERFACE's own answer for "unavailable in
        //                   this environment" (SSLSession.java:77-84), not a stand-in
        //                   for one. Contrast getCipherSuite, which may not return null
        //                   at all and therefore needs the JSSE sentinel above.
        ck(door + ".getPeerHost", String.valueOf(s.getPeerHost()), "null");
        ck(door + ".getPeerPort", s.getPeerPort(), -1);
        ck(door + ".getSessionContext", String.valueOf(s.getSessionContext()), "null");

        // ...and the MESSAGES of the two refusals, which the class-name rows above
        // cannot see. F18 measured both doors refusing identically; an IOException
        // whose message names the right thing never matches the caller's catch, and
        // the converse — the right class carrying some other explanation — is what
        // lets a wrong internal cause survive a repair.
        ck(door + ".getPeerCertificates.message", refusalMessage(() -> s.getPeerCertificates()),
                "peer not authenticated");
        ck(door + ".getPeerPrincipal.message", refusalMessage(() -> s.getPeerPrincipal()),
                "peer not authenticated");
    }

    /**
     * DOOR-agnostic arm for {@code invalidate()}, the fourth of F18's doors and the only
     * one that MUTATES.
     *
     * <p>Run on a session of its own so nothing above it observes the mutation, and run on
     * BOTH doors because the two are minted by different code paths with different field
     * widths, and width is what decides what a slot means (F18 §2.3).
     *
     * <p>The claim is deliberately narrow. On a session that negotiated nothing HotSpot
     * moves NOTHING — so this arm is the negative half of F18's contract, and it is the
     * half that catches the two mistakes a fixture-less fix makes: raising
     * {@code AbstractMethodError} because the door was never registered, and inverting the
     * bit so that invalidating a session makes it valid.
     */
    static void invalidateMovesNothingHere(String door, SSLSession s) {
        ck(door + ".invalidate.raises", raised(s::invalidate), "none");
        ck(door + ".invalidate.isValid", s.isValid(), Boolean.FALSE);
        // The second accessor F18 found. It is null before and after here, so the row
        // asserts that invalidate() does not MINT a context — the direction this file
        // can see. The drop from a live SSLSessionContextImpl to null needs a handshake.
        ck(door + ".invalidate.getSessionContext", String.valueOf(s.getSessionContext()), "null");
        // getId keeps its exact bytes. This is the row a "simplification" of getId onto
        // the validity predicate breaks, which is why it is asserted rather than assumed.
        ck(door + ".invalidate.getId.length", s.getId().length, 0);
        ck(door + ".invalidate.getCipherSuite", s.getCipherSuite(), "SSL_NULL_WITH_NULL_NULL");
        ck(door + ".invalidate.getPeerPort", s.getPeerPort(), -1);
        // Idempotent: a second call neither throws nor flips anything back.
        ck(door + ".invalidate.twice.raises", raised(s::invalidate), "none");
        ck(door + ".invalidate.twice.isValid", s.isValid(), Boolean.FALSE);
    }

    /**
     * The attribute map must not shadow the identity doors.
     *
     * <p>E31-1 §2's recorded defect: slot 3 carries the peer host on the wide session
     * shapes and the ATTRIBUTE MAP on the 4-field one, so a width-blind reader returns a
     * {@code java.util.HashMap} through {@code ()Ljava/lang/String;} — and Jetty's
     * {@code SecureRequestCustomizer.retrieveSni()} calls {@code putValue} on every SSL
     * request, so the trap is armed in the field and by nothing in this suite.
     *
     * <p>Written as a state CHANGE rather than a state read: rows 1-6 establish that the
     * attribute really landed, and only then do the three identity doors get asked again.
     * Without the first half a VM whose {@code putValue} silently did nothing would pass
     * the second half for the wrong reason.
     */
    static void attributes() throws Exception {
        SSLSocketFactory f = (SSLSocketFactory) SSLSocketFactory.getDefault();
        SSLSocket sock = (SSLSocket) f.createSocket();
        SSLSession s = sock.getSession();
        ck("attrs.before.valueNames", Arrays.toString(s.getValueNames()), "[]");
        ck("attrs.before.getValue", String.valueOf(s.getValue("cratonvm.f25")), "null");
        ck("attrs.putValue.raises", raised(() -> s.putValue("cratonvm.f25", "v")), "none");
        ck("attrs.after.getValue", String.valueOf(s.getValue("cratonvm.f25")), "v");
        ck("attrs.after.getValue.class",
                s.getValue("cratonvm.f25") == null
                        ? "null" : s.getValue("cratonvm.f25").getClass().getName(),
                "java.lang.String");
        ck("attrs.after.valueNames", Arrays.toString(s.getValueNames()), "[cratonvm.f25]");

        // The three rows this arm exists for. The attribute is now present, so a reader
        // that resolves slot 3 without consulting the session's WIDTH answers the map
        // here — visibly, because String.valueOf of a HashMap is "{cratonvm.f25=v}".
        ck("attrs.shadow.getPeerHost", String.valueOf(s.getPeerHost()), "null");
        ck("attrs.shadow.getPeerPort", s.getPeerPort(), -1);
        ck("attrs.shadow.getSessionContext", String.valueOf(s.getSessionContext()), "null");

        ck("attrs.removeValue.raises", raised(() -> s.removeValue("cratonvm.f25")), "none");
        ck("attrs.removed.valueNames", Arrays.toString(s.getValueNames()), "[]");
        sock.close();
    }

    /** Both doors, for the one arm that mutates the session it is given. */
    static void invalidate() throws Exception {
        SSLSocketFactory f = (SSLSocketFactory) SSLSocketFactory.getDefault();
        SSLSocket sock = (SSLSocket) f.createSocket();
        invalidateMovesNothingHere("socketInv", sock.getSession());
        sock.close();

        SSLContext c = SSLContext.getInstance("TLS");
        c.init(null, null, null);
        SSLEngine e = c.createSSLEngine();
        e.setUseClientMode(true);
        invalidateMovesNothingHere("engineInv", e.getSession());
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

    /**
     * The detail message of the refusal, or a description of what came out instead.
     *
     * <p>Separate from {@link #refusal} on purpose: the class-name row and the message row
     * fail on disjoint defects, and collapsing them into one string would make a single
     * check answer two questions and report neither clearly.
     */
    static String refusalMessage(ThrowingCall c) {
        try {
            Object v = c.call();
            return "RETURNED " + (v == null ? "null" : v.getClass().getName());
        } catch (SSLPeerUnverifiedException e) {
            return String.valueOf(e.getMessage());
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    /** The class of whatever {@code r} raised, or {@code "none"} — a door that ANSWERS. */
    static String raised(ThrowingRun r) {
        try {
            r.run();
            return "none";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    interface ThrowingCall {
        Object call() throws Exception;
    }

    interface ThrowingRun {
        void run() throws Exception;
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
        // Both arms below mint sessions of their own, and both MUTATE them — one through
        // invalidate(), one through putValue() — so neither may run against a session an
        // earlier arm has asserted on. Last, for the same reason the unmodifiable-set row
        // is last in RJdkSecurity.
        invalidate();
        attributes();
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
