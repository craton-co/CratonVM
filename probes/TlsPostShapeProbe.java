package org.apache.tomcat.util.net;

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;

import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;

import jakarta.servlet.http.HttpServlet;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;

import org.junit.Test;

import org.apache.catalina.Context;
import org.apache.catalina.startup.Tomcat;
import org.apache.catalina.startup.TomcatBaseTest;
import org.apache.tomcat.util.net.TesterSupport;

/**
 * Split {@code TestSsl.testPost}'s ~200 s into phases, against the SAME server
 * it uses.
 *
 * <p>testPost does three things per thread — TLS connect, write 16 MiB in
 * 128 KiB blocks, then read 16 MiB back ONE BYTE AT A TIME — and reports only a
 * pass/fail. That is not enough to attribute the gap against HotSpot: any of the
 * three could hold it. This probe runs the identical client against an identical
 * connector and times each phase, then repeats the readback through
 * {@code read(byte[], int, int)} so the two read shapes can be compared with
 * everything else (handshake, record layer, servlet, kernel, GC) held constant.
 *
 * <p>It is a JUnit class rather than a {@code main} so it inherits
 * {@code TomcatBaseTest}'s fixture and {@code TesterSupport}'s keystore wiring —
 * i.e. so the server under measurement is byte-for-byte the one testPost
 * measures, not a stand-in that could hide or invent a cost.
 *
 * <pre>
 *   run-one.ps1 -Main RunMethods -Args2 org.apache.tomcat.util.net.TlsPostShapeProbe,shapes
 * </pre>
 */
public class TlsPostShapeProbe extends TomcatBaseTest {

    private static final int POST_DATA_SIZE = 16 * 1024 * 1024;
    private static final int BLOCK = 128 * 1024;
    private static final byte[] POST_DATA = new byte[POST_DATA_SIZE];

    static {
        for (int i = 0; i < POST_DATA_SIZE; i++) {
            POST_DATA[i] = 1;
        }
    }

    @Test
    public void shapes() throws Exception {
        // testPost's own setup, in its own order.
        javax.net.SocketFactory socketFactory = TesterSupport.configureClientSsl();

        Tomcat tomcat = getTomcatInstance();
        TesterSupport.initSsl(tomcat);
        org.apache.catalina.connector.Connector connector = tomcat.getConnector();
        connector.setProperty("connectionTimeout", "20000");
        TesterSupport.configureSSLImplementation(tomcat,
                "org.apache.tomcat.util.net.jsse.JSSEImplementation", false);

        Context ctxt = getProgrammaticRootContext();
        Tomcat.addServlet(ctxt, "post", new EchoServlet());
        ctxt.addServletMappingDecoded("/post", "post");
        tomcat.start();

        SSLSocketFactory factory = (SSLSocketFactory) socketFactory;
        // Warm pass first: one connection, so class loading / JIT / TLS setup
        // are not billed to the measured rounds.
        round(factory, 1, 0, "warmup");
        round(factory, 8, 0, "read()-1byte");
        // The same bytes through `read(byte[], 0, k)` at several k. Per-byte
        // cost at chunk k is (C/k + P) for a fixed per-CALL cost C and per-BYTE
        // cost P, so three k values over-determine both — which is the only way
        // to say whether the single-byte shape is dear because of what the call
        // DOES or because of the call itself, without changing the VM.
        round(factory, 8, 1, "read([B,0,1)");
        round(factory, 8, 64, "read([B,0,64)");
        round(factory, 8, 8192, "read([B,0,8192)");
    }

    /**
     * Price the Java-&gt;native transition itself, so the single-byte
     * {@code read()} cost can be split into "the call" and "what the call
     * does" from the Java side alone — no VM instrumentation, and therefore no
     * {@code Instant::now()} overhead confounding the answer.
     *
     * <p>Three natives on the SAME receiver, in increasing order of body:
     * <ul>
     *   <li>{@code OutputStream.flush()} — CratonVM registers this as a
     *       literal no-op (it has nothing to flush; the write natives go
     *       straight to the stream). Transition only.</li>
     *   <li>{@code InputStream.available()} — reads the tls id out of the
     *       receiver's field 0 and asks the readahead for its buffered length.
     *       Transition + field read + table lookup, no byte moved.</li>
     *   <li>{@code InputStream.read()} — the same, plus popping one byte.</li>
     * </ul>
     *
     * <p>flush is the floor; available-minus-flush is the field read and the
     * table lookup; read-minus-available is the pop. Run at 1 thread and at 8
     * to see which of the three scales with concurrency.
     */
    @Test
    public void nativeFloor() throws Exception {
        javax.net.SocketFactory socketFactory = TesterSupport.configureClientSsl();
        Tomcat tomcat = getTomcatInstance();
        TesterSupport.initSsl(tomcat);
        tomcat.getConnector().setProperty("connectionTimeout", "20000");
        TesterSupport.configureSSLImplementation(tomcat,
                "org.apache.tomcat.util.net.jsse.JSSEImplementation", false);
        Context ctxt = getProgrammaticRootContext();
        Tomcat.addServlet(ctxt, "post", new EchoServlet());
        ctxt.addServletMappingDecoded("/post", "post");
        tomcat.start();

        floorRound((SSLSocketFactory) socketFactory, 1);
        floorRound((SSLSocketFactory) socketFactory, 8);
    }

    private void floorRound(SSLSocketFactory factory, int threads) throws Exception {
        final int iters = 2_000_000;
        final int port = getPort();
        final AtomicLong flushNs = new AtomicLong();
        final AtomicLong availNs = new AtomicLong();
        final AtomicLong readNs = new AtomicLong();
        final AtomicLong errors = new AtomicLong();
        final CountDownLatch latch = new CountDownLatch(threads);
        for (int t = 0; t < threads; t++) {
            new Thread(() -> {
                try {
                    SSLSocket socket = (SSLSocket) factory.createSocket("localhost", port);
                    OutputStream os = socket.getOutputStream();
                    InputStream is = socket.getInputStream();
                    // Post a body and leave the response unread, so `read()`
                    // below is served from the readahead rather than blocking.
                    os.write("POST /post HTTP/1.1\r\n".getBytes());
                    os.write("Host: localhost\r\n".getBytes());
                    os.write(("Content-Length: " + POST_DATA.length + "\r\n\r\n").getBytes());
                    for (int i = 0; i < POST_DATA.length / BLOCK; i++) {
                        os.write(POST_DATA, 0, BLOCK);
                    }
                    os.flush();

                    // Warm each shape before measuring it.
                    for (int i = 0; i < 200_000; i++) {
                        os.flush();
                        is.available();
                    }

                    long f0 = System.nanoTime();
                    for (int i = 0; i < iters; i++) {
                        os.flush();
                    }
                    flushNs.addAndGet(System.nanoTime() - f0);

                    long a0 = System.nanoTime();
                    for (int i = 0; i < iters; i++) {
                        is.available();
                    }
                    availNs.addAndGet(System.nanoTime() - a0);

                    long r0 = System.nanoTime();
                    for (int i = 0; i < iters; i++) {
                        if (is.read() < 0) {
                            errors.incrementAndGet();
                            break;
                        }
                    }
                    readNs.addAndGet(System.nanoTime() - r0);
                    socket.close();
                } catch (Exception e) {
                    System.out.println("[floor] ERROR " + e);
                    errors.incrementAndGet();
                } finally {
                    latch.countDown();
                }
            }, "floor-" + t).start();
        }
        latch.await();
        long calls = (long) iters * threads;
        System.out.printf("[floor] threads=%d  flush=%7.1f ns/call  available=%7.1f ns/call  "
                + "read()=%7.1f ns/call   (available-flush=%7.1f, read-available=%7.1f) errors=%d%n",
                threads,
                (double) flushNs.get() / calls,
                (double) availNs.get() / calls,
                (double) readNs.get() / calls,
                (double) (availNs.get() - flushNs.get()) / calls,
                (double) (readNs.get() - availNs.get()) / calls,
                errors.get());
    }

    /**
     * Only the shape the in-native counters are about, so an instrumented run
     * (which pays two {@code Instant::now()} per call) stays affordable.
     */
    @Test
    public void read1Only() throws Exception {
        javax.net.SocketFactory socketFactory = TesterSupport.configureClientSsl();
        Tomcat tomcat = getTomcatInstance();
        TesterSupport.initSsl(tomcat);
        tomcat.getConnector().setProperty("connectionTimeout", "20000");
        TesterSupport.configureSSLImplementation(tomcat,
                "org.apache.tomcat.util.net.jsse.JSSEImplementation", false);
        Context ctxt = getProgrammaticRootContext();
        Tomcat.addServlet(ctxt, "post", new EchoServlet());
        ctxt.addServletMappingDecoded("/post", "post");
        tomcat.start();
        SSLSocketFactory factory = (SSLSocketFactory) socketFactory;
        round(factory, 1, 0, "warmup");
        round(factory, 8, 0, "read()-1byte");
    }

    /**
     * One round of {@code threads} connections. {@code chunk == 0} means the
     * readback uses the no-arg {@code read()}; otherwise it uses
     * {@code read(byte[], 0, chunk)}.
     */
    private void round(SSLSocketFactory factory, int threads, int chunk, String label)
            throws Exception {
        final AtomicLong connectNs = new AtomicLong();
        final AtomicLong writeNs = new AtomicLong();
        final AtomicLong readNs = new AtomicLong();
        final AtomicLong errors = new AtomicLong();
        final CountDownLatch latch = new CountDownLatch(threads);
        final int port = getPort();
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            new Thread(() -> {
                try {
                    long c0 = System.nanoTime();
                    SSLSocket socket = (SSLSocket) factory.createSocket("localhost", port);
                    OutputStream os = socket.getOutputStream();
                    os.write("POST /post HTTP/1.1\r\n".getBytes());
                    os.write("Host: localhost\r\n".getBytes());
                    os.write(("Content-Length: " + POST_DATA.length + "\r\n\r\n").getBytes());
                    connectNs.addAndGet(System.nanoTime() - c0);

                    long w0 = System.nanoTime();
                    for (int i = 0; i < POST_DATA.length / BLOCK; i++) {
                        os.write(POST_DATA, 0, BLOCK);
                        // testPost's own inter-block pause. Kept so the write
                        // phase is comparable to its number, not a faster shape.
                        Thread.sleep(10);
                    }
                    os.flush();
                    writeNs.addAndGet(System.nanoTime() - w0);

                    InputStream is = socket.getInputStream();
                    // Skip headers exactly as testPost does — single-byte, and
                    // only a few hundred bytes, so it is not worth timing apart.
                    byte[] endOfHeaders = "\r\n\r\n".getBytes();
                    int found = 0;
                    while (found != endOfHeaders.length) {
                        int c = is.read();
                        if (c == -1) {
                            errors.incrementAndGet();
                            break;
                        } else if (c == endOfHeaders[found]) {
                            found++;
                        } else {
                            found = 0;
                        }
                    }

                    long r0 = System.nanoTime();
                    if (chunk == 0) {
                        for (int i = 0; i < POST_DATA.length; i++) {
                            if (is.read() != 1) {
                                System.out.println("[shape] " + label + " EOF/mismatch at " + i);
                                errors.incrementAndGet();
                                break;
                            }
                        }
                    } else {
                        byte[] buf = new byte[chunk];
                        int got = 0;
                        while (got < POST_DATA.length) {
                            int n = is.read(buf, 0, chunk);
                            if (n < 0) {
                                System.out.println("[shape] " + label + " EOF at " + got);
                                errors.incrementAndGet();
                                break;
                            }
                            got += n;
                        }
                    }
                    readNs.addAndGet(System.nanoTime() - r0);
                    socket.close();
                } catch (Exception e) {
                    System.out.println("[shape] " + label + " ERROR " + e);
                    errors.incrementAndGet();
                } finally {
                    latch.countDown();
                }
            }, label + "-" + t).start();
        }
        latch.await();
        long wall = System.nanoTime() - t0;
        long bytes = (long) POST_DATA.length * threads;
        System.out.printf("[shape] %-12s threads=%d wall=%7.2fs connect=%7.2fs write=%7.2fs "
                + "read=%7.2fs read-ns-per-byte=%8.1f errors=%d%n",
                label, threads, wall / 1e9, connectNs.get() / 1e9, writeNs.get() / 1e9,
                readNs.get() / 1e9, (double) readNs.get() / bytes, errors.get());
    }

    /** {@code TestSsl.SimplePostServlet}, copied so the server side is identical. */
    public static class EchoServlet extends HttpServlet {
        private static final long serialVersionUID = 1L;

        @Override
        protected void doPost(HttpServletRequest req, HttpServletResponse resp)
                throws java.io.IOException {
            ByteArrayOutputStream baos = new ByteArrayOutputStream(POST_DATA_SIZE);
            byte[] in = new byte[1500];
            InputStream input = req.getInputStream();
            while (true) {
                int n = input.read(in);
                if (n > 0) {
                    baos.write(in, 0, n);
                } else {
                    break;
                }
            }
            byte[] out = baos.toByteArray();
            resp.setContentLength(out.length);
            resp.getOutputStream().write(out);
        }
    }
}
