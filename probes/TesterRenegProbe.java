/*
 * Local-only diagnostic for `TestSsl.testClientInitiatedRenegotiation[JSSE]`.
 *
 * This is NOT a synthetic replica: it extends Tomcat's own `TomcatBaseTest`
 * and drives `TesterSupport.initSsl` + the real JSSE client API in exactly the
 * sequence `TestSsl.testClientInitiatedRenegotiation` uses. The only
 * difference is that every step reports what actually happened, instead of
 * collapsing into the test's bare `assertTrue(listener.isComplete())`.
 *
 * Answers three questions the bare assertion hides:
 *   1. which TLS version the connection actually negotiated (the test asks for
 *      TLSv1.2 via `SSLContext.getInstance`; a stack that silently gives
 *      TLS 1.3 changes what "renegotiation" even means),
 *   2. whether `startHandshake()` returned or threw,
 *   3. whether the HandshakeCompletedListener was ever invoked.
 */
package org.apache.tomcat.util.net;

import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.Reader;

import javax.net.ssl.HandshakeCompletedEvent;
import javax.net.ssl.HandshakeCompletedListener;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;

import org.junit.Test;

import org.apache.catalina.Context;
import org.apache.catalina.Wrapper;
import org.apache.catalina.startup.TesterServlet;
import org.apache.catalina.startup.Tomcat;
import org.apache.catalina.startup.TomcatBaseTest;
import org.apache.tomcat.util.net.jsse.JSSEImplementation;

public class TesterRenegProbe extends TomcatBaseTest {

    private static final class Listener implements HandshakeCompletedListener {
        volatile boolean complete;
        volatile String protocol;
        volatile String cipher;
        volatile String thread;

        @Override
        public void handshakeCompleted(HandshakeCompletedEvent event) {
            protocol = event.getSession() == null ? "<null session>" : event.getSession().getProtocol();
            cipher = event.getCipherSuite();
            thread = Thread.currentThread().getName();
            complete = true;
        }
    }

    @Test
    public void probe() throws Exception {
        Tomcat tomcat = getTomcatInstance();
        TesterSupport.initSsl(tomcat);
        TesterSupport.configureSSLImplementation(tomcat, JSSEImplementation.class.getName(), false);

        Context root = tomcat.addContext("", TEMP_DIR);
        Wrapper w = Tomcat.addServlet(root, "tester", new TesterServlet());
        w.setAsyncSupported(true);
        root.addServletMappingDecoded("/", "tester");

        tomcat.start();

        SSLContext sslCtx = SSLContext.getInstance(Constants.SSL_PROTO_TLSv1_2);
        sslCtx.init(null, TesterSupport.getTrustManagers(), null);
        SSLSocketFactory socketFactory = sslCtx.getSocketFactory();
        SSLSocket socket = (SSLSocket) socketFactory.createSocket("localhost", getPort());

        OutputStream os = socket.getOutputStream();
        InputStream is = socket.getInputStream();
        Reader r = new InputStreamReader(is);

        doRequest(os, r);
        System.out.println("[probe] initial exchange OK");
        System.out.println("[probe] enabledProtocols = "
                + java.util.Arrays.toString(socket.getEnabledProtocols()));
        System.out.println("[probe] session.protocol = " + sessionProtocol(socket));
        System.out.println("[probe] session.cipher   = " + sessionCipher(socket));

        Listener l = new Listener();
        socket.addHandshakeCompletedListener(l);
        System.out.println("[probe] listener registered");

        try {
            socket.startHandshake();
            System.out.println("[probe] startHandshake() returned normally");
        } catch (Throwable t) {
            System.out.println("[probe] startHandshake() THREW " + t);
        }

        try {
            doRequest(os, r);
            System.out.println("[probe] post-handshake exchange OK");
        } catch (Throwable t) {
            System.out.println("[probe] post-handshake exchange THREW " + t);
        }

        int wait = 0;
        while (wait < 5000 && !l.complete) {
            wait += 50;
            Thread.sleep(50);
        }
        System.out.println("[probe] listener.complete = " + l.complete + " after " + wait + " ms");
        System.out.println("[probe] listener.protocol = " + l.protocol);
        System.out.println("[probe] listener.cipher   = " + l.cipher);
        System.out.println("[probe] listener.thread   = " + l.thread);
        System.out.println("[probe] session.protocol after = " + sessionProtocol(socket));
        socket.close();
    }

    /**
     * JSSE contract: `getSession()` never returns null — it forces a handshake
     * and, if that fails, returns an invalid session whose cipher suite is
     * `SSL_NULL_WITH_NULL_NULL`. Reported explicitly here because a null is
     * itself a finding, not something to NPE on.
     */
    private static String sessionProtocol(SSLSocket s) {
        javax.net.ssl.SSLSession sess = s.getSession();
        return sess == null ? "<getSession() RETURNED NULL>" : sess.getProtocol();
    }

    private static String sessionCipher(SSLSocket s) {
        javax.net.ssl.SSLSession sess = s.getSession();
        return sess == null ? "<getSession() RETURNED NULL>" : sess.getCipherSuite();
    }

    /**
     * The OTHER half of the HandshakeCompletedListener contract, and the one
     * `TestSsl.testClientInitiatedRenegotiation` cannot reach: a socket whose
     * handshake has NOT yet run when the listener is registered.
     *
     * `createSocket(String, int)` handshakes before it hands the socket back,
     * so a listener added afterwards has legitimately missed the event — on
     * stock JSSE too. The layered overload `createSocket(Socket, String, int,
     * boolean)` defers the handshake to `startHandshake()`, so here the
     * listener IS registered before any handshake completes and MUST fire on
     * both VMs. This is what distinguishes "we don't renegotiate" (correct)
     * from "we never deliver the event at all" (the defect).
     */
    @Test
    public void probeLayeredListenerFires() throws Exception {
        Tomcat tomcat = getTomcatInstance();
        TesterSupport.initSsl(tomcat);
        TesterSupport.configureSSLImplementation(tomcat, JSSEImplementation.class.getName(), false);

        Context root = tomcat.addContext("", TEMP_DIR);
        Wrapper w = Tomcat.addServlet(root, "tester", new TesterServlet());
        w.setAsyncSupported(true);
        root.addServletMappingDecoded("/", "tester");

        tomcat.start();

        SSLContext sslCtx = SSLContext.getInstance(Constants.SSL_PROTO_TLSv1_2);
        sslCtx.init(null, TesterSupport.getTrustManagers(), null);
        SSLSocketFactory socketFactory = sslCtx.getSocketFactory();

        java.net.Socket plain = new java.net.Socket("localhost", getPort());
        SSLSocket socket = (SSLSocket) socketFactory.createSocket(plain, "localhost", getPort(), true);

        Listener l = new Listener();
        socket.addHandshakeCompletedListener(l);
        System.out.println("[layered] listener registered BEFORE any handshake");

        socket.startHandshake();
        System.out.println("[layered] startHandshake() returned normally");

        int wait = 0;
        while (wait < 5000 && !l.complete) {
            wait += 50;
            Thread.sleep(50);
        }
        System.out.println("[layered] listener.complete = " + l.complete + " after " + wait + " ms");
        System.out.println("[layered] listener.protocol = " + l.protocol);
        System.out.println("[layered] listener.cipher   = " + l.cipher);
        System.out.println("[layered] session.protocol  = " + sessionProtocol(socket));

        // Exercise the removal contract too: removing a registered listener
        // must succeed, and removing an unregistered one must be rejected.
        socket.removeHandshakeCompletedListener(l);
        System.out.println("[layered] remove of registered listener: OK");
        try {
            socket.removeHandshakeCompletedListener(l);
            System.out.println("[layered] remove of UNregistered listener: no throw (JSSE throws IAE)");
        } catch (IllegalArgumentException e) {
            System.out.println("[layered] remove of UNregistered listener: IllegalArgumentException (matches JSSE)");
        }

        org.junit.Assert.assertTrue(
                "HandshakeCompletedListener must fire for a handshake that had not yet run "
                        + "when the listener was registered", l.complete);
        socket.close();
    }

    private static void doRequest(OutputStream os, Reader r) throws Exception {
        char[] expectedResponseLine = "HTTP/1.1 200 \r\n".toCharArray();
        os.write("GET /tester HTTP/1.1\r\n".getBytes());
        os.write("Host: localhost\r\n".getBytes());
        os.write("Connection: Keep-Alive\r\n\r\n".getBytes());
        os.flush();
        StringBuilder got = new StringBuilder();
        for (char c : expectedResponseLine) {
            int read = r.read();
            got.append((char) read);
            if (read != c) {
                throw new IllegalStateException("response line mismatch, got: " + got);
            }
        }
        char[] endOfHeaders = "\r\n\r\n".toCharArray();
        int found = 0;
        while (found != endOfHeaders.length) {
            int c = r.read();
            if (c == -1) {
                throw new IllegalStateException("EOF in headers");
            }
            if (c == endOfHeaders[found]) {
                found++;
            } else {
                found = 0;
            }
        }
        // TesterServlet's body is "OK" — drain it so the stream is positioned
        // at the start of the next response for the second request.
        char[] body = new char[2];
        int n = r.read(body, 0, 2);
        if (n != 2) {
            throw new IllegalStateException("short body read: " + n);
        }
    }
}
