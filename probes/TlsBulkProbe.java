import java.io.ByteArrayOutputStream;
import java.io.FileInputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.security.KeyStore;
import java.security.SecureRandom;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;

import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLServerSocketFactory;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;
import javax.net.ssl.TrustManagerFactory;

/**
 * Decompose the cost of TLS bulk transfer the way {@code TestSsl.testPost}
 * exercises it, so the ~50x gap that test shows can be attributed to a phase
 * instead of guessed at.
 *
 * <p>The shape is testPost's, deliberately: N client threads each push
 * {@code sizeMiB} through their own {@code SSLSocket} in 128 KiB blocks, the
 * server drains a whole body before echoing it (exactly what
 * {@code SimplePostServlet} does — read fully into a buffer, then write), and
 * the client reads the echo back. What this adds is that each phase is timed
 * separately, and the readback runs in BOTH shapes:
 *
 * <ul>
 *   <li>{@code read()} one byte at a time — what testPost does, ~16.7 million
 *       calls per MiB-16 thread;</li>
 *   <li>{@code read(byte[], int, int)} in 8 KiB blocks — the same bytes through
 *       the bulk entry point.</li>
 * </ul>
 *
 * <p>Both shapes move identical bytes over identical sockets, so the ratio
 * between them is the per-call overhead of the single-byte path with everything
 * else (handshake, record layer, kernel, GC) held constant. Running it at
 * {@code -threads 1} and {@code -threads 8} separates per-call cost from lock
 * contention: a per-call cost is flat in the thread count, contention is not.
 *
 * <p>The server runs in this same process, as testPost's does. It is a plain
 * {@code SSLServerSocket} rather than a Tomcat connector because the point is
 * the JSSE stream path, not the connector.
 *
 * <pre>
 *   TlsBulkProbe -keystore &lt;localhost-rsa.jks&gt; -threads 8 -sizeMiB 16
 * </pre>
 *
 * <p><strong>Does not currently run on CratonVM.</strong> The server half — a
 * plain in-process {@code SSLServerSocket} — fails there before any bytes are
 * measured: the accepted socket's output stream reports
 * {@code SSLSocketOutputStream.write: stream is closed} and the client then
 * times out (Windows {@code os error 10060}). Reproduced at 2 threads x 1 MiB,
 * i.e. nothing to do with bulk volume. It works on HotSpot.
 *
 * <p>That lane is NOT the one {@code TestSsl.testPost} uses — testPost's server
 * is a Tomcat NIO connector driving a rustls-backed {@code SSLEngine}, not an
 * {@code SSLServerSocket} — so this was set aside rather than chased during the
 * testPost investigation (see
 * {@code fixed-suite-bugs/tomcat/testssl-testpost-connection-dies-under-concurrent-bulk-tls-FIXED.md}).
 * Use {@code TlsPostShapeProbe} for the testPost shape. This file is kept
 * because the server-side failure above is itself worth a page.
 */
public final class TlsBulkProbe {

    private static final int BLOCK = 128 * 1024;

    public static void main(String[] args) throws Exception {
        String keystore = null;
        String truststore = null;
        String storePass = "changeit";
        int threads = 8;
        int sizeMiB = 16;
        boolean singleByte = true;
        boolean bulk = true;
        for (int i = 0; i < args.length; i++) {
            switch (args[i]) {
                case "-keystore": keystore = args[++i]; break;
                case "-truststore": truststore = args[++i]; break;
                case "-storepass": storePass = args[++i]; break;
                case "-threads": threads = Integer.parseInt(args[++i]); break;
                case "-sizeMiB": sizeMiB = Integer.parseInt(args[++i]); break;
                case "-only":
                    String only = args[++i];
                    singleByte = only.equals("single") || only.equals("both");
                    bulk = only.equals("bulk") || only.equals("both");
                    break;
                default: throw new IllegalArgumentException("unknown arg " + args[i]);
            }
        }
        if (keystore == null || truststore == null) {
            throw new IllegalArgumentException("-keystore and -truststore are required");
        }

        final int size = sizeMiB * 1024 * 1024;
        final byte[] data = new byte[size];
        for (int i = 0; i < size; i++) {
            data[i] = (byte) (i & 0x7f);
        }

        SSLContext serverCtx = serverContext(keystore, storePass);
        SSLContext clientCtx = clientContext(truststore, storePass);

        SSLServerSocketFactory ssf = serverCtx.getServerSocketFactory();
        final SSLServerSocket server = (SSLServerSocket) ssf.createServerSocket(0);
        final int port = server.getLocalPort();
        System.out.println("[probe] server port=" + port + " threads=" + threads
                + " sizeMiB=" + sizeMiB);

        final int connections = threads * ((singleByte ? 1 : 0) + (bulk ? 1 : 0));
        final CountDownLatch serverDone = new CountDownLatch(connections);
        Thread acceptor = new Thread(() -> {
            try {
                for (int i = 0; i < connections; i++) {
                    final SSLSocket s = (SSLSocket) server.accept();
                    new Thread(() -> {
                        try {
                            echoOnce(s, size);
                        } catch (Exception e) {
                            System.out.println("[probe] SERVER-ERROR " + e);
                        } finally {
                            serverDone.countDown();
                        }
                    }, "echo").start();
                }
            } catch (Exception e) {
                System.out.println("[probe] ACCEPT-ERROR " + e);
            }
        }, "acceptor");
        acceptor.setDaemon(true);
        acceptor.start();

        SSLSocketFactory csf = clientCtx.getSocketFactory();
        if (singleByte) {
            runPhase("single-byte", csf, port, threads, data, true);
        }
        if (bulk) {
            runPhase("bulk-8k", csf, port, threads, data, false);
        }
        serverDone.await();
        server.close();
        System.out.println("[probe] DONE");
    }

    /** The server half of one connection: drain {@code size} bytes, echo them back. */
    private static void echoOnce(SSLSocket s, int size) throws Exception {
        InputStream in = s.getInputStream();
        OutputStream out = s.getOutputStream();
        ByteArrayOutputStream body = new ByteArrayOutputStream(size);
        byte[] chunk = new byte[8192];
        while (body.size() < size) {
            int n = in.read(chunk);
            if (n < 0) {
                break;
            }
            body.write(chunk, 0, n);
        }
        out.write(body.toByteArray());
        out.flush();
        s.close();
    }

    private static void runPhase(String label, SSLSocketFactory csf, int port, int threads,
            byte[] data, boolean readSingleByte) throws Exception {
        final AtomicLong connectNs = new AtomicLong();
        final AtomicLong writeNs = new AtomicLong();
        final AtomicLong readNs = new AtomicLong();
        final AtomicLong errors = new AtomicLong();
        final CountDownLatch latch = new CountDownLatch(threads);
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            new Thread(() -> {
                try {
                    long c0 = System.nanoTime();
                    SSLSocket socket = (SSLSocket) csf.createSocket("localhost", port);
                    socket.startHandshake();
                    connectNs.addAndGet(System.nanoTime() - c0);

                    OutputStream os = socket.getOutputStream();
                    long w0 = System.nanoTime();
                    for (int off = 0; off < data.length; off += BLOCK) {
                        os.write(data, off, Math.min(BLOCK, data.length - off));
                    }
                    os.flush();
                    writeNs.addAndGet(System.nanoTime() - w0);

                    InputStream is = socket.getInputStream();
                    long r0 = System.nanoTime();
                    if (readSingleByte) {
                        for (int i = 0; i < data.length; i++) {
                            int b = is.read();
                            if (b != (data[i] & 0xff)) {
                                System.out.println("[probe] MISMATCH at " + i + " got " + b);
                                errors.incrementAndGet();
                                break;
                            }
                        }
                    } else {
                        byte[] buf = new byte[8192];
                        int got = 0;
                        while (got < data.length) {
                            int n = is.read(buf, 0, buf.length);
                            if (n < 0) {
                                System.out.println("[probe] EOF at " + got);
                                errors.incrementAndGet();
                                break;
                            }
                            got += n;
                        }
                    }
                    readNs.addAndGet(System.nanoTime() - r0);
                    socket.close();
                } catch (Exception e) {
                    System.out.println("[probe] CLIENT-ERROR " + e);
                    errors.incrementAndGet();
                } finally {
                    latch.countDown();
                }
            }, label + "-" + t).start();
        }
        latch.await();
        long wall = System.nanoTime() - t0;
        long bytes = (long) data.length * threads;
        System.out.printf("[probe] %-11s wall=%7.2fs connect=%7.2fs write=%7.2fs read=%7.2fs "
                + "read-ns-per-byte=%8.1f errors=%d%n",
                label, wall / 1e9, connectNs.get() / 1e9, writeNs.get() / 1e9,
                readNs.get() / 1e9, (double) readNs.get() / bytes, errors.get());
    }

    private static SSLContext serverContext(String keystore, String pass) throws Exception {
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(load(keystore, pass), pass.toCharArray());
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(kmf.getKeyManagers(), null, new SecureRandom());
        return ctx;
    }

    /**
     * Trust is a real {@link TrustManagerFactory} over the fixture CA, not an
     * accept-everything {@code X509TrustManager}. A hand-written trust-all
     * manager is not honoured by every TLS backend, so it fails the handshake
     * with {@code UnknownIssuer} instead of measuring anything — a vacuous run
     * that reads like a probe bug. This is also what {@code TesterSupport}
     * does, so the probe's trust path is the suite's trust path.
     */
    private static SSLContext clientContext(String truststore, String pass) throws Exception {
        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(load(truststore, pass));
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(null, tmf.getTrustManagers(), new SecureRandom());
        return ctx;
    }

    private static KeyStore load(String path, String pass) throws Exception {
        KeyStore ks = KeyStore.getInstance("JKS");
        try (InputStream in = new FileInputStream(path)) {
            ks.load(in, pass.toCharArray());
        }
        return ks;
    }
}
