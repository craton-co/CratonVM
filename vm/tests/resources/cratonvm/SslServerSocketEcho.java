package cratonvm;

import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyFactory;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.security.spec.PKCS8EncodedKeySpec;
import java.util.Base64;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicReference;

import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManagerFactory;

/**
 * A plain in-process {@code SSLServerSocket} echo: accept one connection, read
 * a body, write it back, and verify the bytes at the client.
 *
 * <p>Regression fixture for the accepted socket's TLS stream id. The server
 * half used to fail with {@code SSLSocketOutputStream.write: stream is closed}
 * before moving a byte, because {@code SSLServerSocket.accept()} recorded the
 * id ONLY as a raw {@code set_field} on a real-JDK-layout {@code SSLSocket}
 * (dropped — field #2 is reference-typed) and in the raw rather than the
 * {@code RUSTLS_SOCK_ID_BASE}-offset id space. Every I/O method on the accepted
 * socket then resolved -1 and read it as "closed".
 *
 * <p>Prints exactly one terminal line: {@code ECHO-OK <bytes>} or
 * {@code ECHO-FAIL <reason>}. The runner asserts on {@code ECHO-OK}, so a
 * fixture that dies before printing cannot be mistaken for a pass.
 *
 * <p>Identity comes from the repo's own tracked test PEMs rather than a
 * generated keystore, so the fixture needs no {@code keytool} and no binary
 * blob. Client trust is a real {@link TrustManagerFactory} over the CA — a
 * hand-written accept-everything {@code X509TrustManager} is not honoured by
 * every TLS backend and fails the handshake with {@code UnknownIssuer}, which
 * would look like this bug and is not.
 */
public final class SslServerSocketEcho {

    private static final char[] PASS = "changeit".toCharArray();
    /** Small on purpose: this is a wiring test, not a throughput test. */
    private static final int BODY = 64 * 1024;

    public static void main(String[] args) {
        try {
            run(Path.of(args[0]));
        } catch (Throwable t) {
            System.out.println("ECHO-FAIL " + t);
            t.printStackTrace();
        }
    }

    private static void run(Path certDir) throws Exception {
        SSLContext serverCtx = SSLContext.getInstance("TLS");
        KeyManagerFactory kmf =
                KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(serverKeyStore(certDir), PASS);
        serverCtx.init(kmf.getKeyManagers(), null, null);

        SSLContext clientCtx = SSLContext.getInstance("TLS");
        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(trustStore(certDir));
        clientCtx.init(null, tmf.getTrustManagers(), null);

        SSLServerSocket server =
                (SSLServerSocket) serverCtx.getServerSocketFactory().createServerSocket(0);
        int port = server.getLocalPort();

        final byte[] payload = new byte[BODY];
        for (int i = 0; i < BODY; i++) {
            payload[i] = (byte) (i & 0x7f);
        }

        final AtomicReference<String> serverError = new AtomicReference<>();
        final CountDownLatch served = new CountDownLatch(1);
        Thread acceptor = new Thread(() -> {
            try (SSLSocket accepted = (SSLSocket) server.accept()) {
                InputStream in = accepted.getInputStream();
                OutputStream out = accepted.getOutputStream();
                byte[] body = new byte[BODY];
                new DataInputStream(in).readFully(body);
                // THE regression: on the broken build this threw
                // "SSLSocketOutputStream.write: stream is closed".
                out.write(body);
                out.flush();
            } catch (Throwable t) {
                serverError.set(String.valueOf(t));
            } finally {
                served.countDown();
            }
        }, "echo-server");
        acceptor.setDaemon(true);
        acceptor.start();

        int verified = 0;
        try (SSLSocket client = (SSLSocket) clientCtx.getSocketFactory()
                .createSocket("localhost", port)) {
            client.getOutputStream().write(payload);
            client.getOutputStream().flush();
            ByteArrayOutputStream echoed = new ByteArrayOutputStream(BODY);
            byte[] buf = new byte[8192];
            while (echoed.size() < BODY) {
                int n = client.getInputStream().read(buf);
                if (n < 0) {
                    break;
                }
                echoed.write(buf, 0, n);
            }
            byte[] got = echoed.toByteArray();
            if (got.length != BODY) {
                System.out.println("ECHO-FAIL short read " + got.length + " of " + BODY
                        + (serverError.get() == null ? "" : " serverError=" + serverError.get()));
                return;
            }
            for (int i = 0; i < BODY; i++) {
                if (got[i] != payload[i]) {
                    System.out.println("ECHO-FAIL byte " + i + " was " + got[i]);
                    return;
                }
                verified++;
            }
        }
        served.await();
        server.close();
        if (serverError.get() != null) {
            System.out.println("ECHO-FAIL server " + serverError.get());
            return;
        }
        System.out.println("ECHO-OK " + verified);
    }

    /** PKCS12 keystore holding the tracked localhost cert + its PKCS#8 key. */
    private static KeyStore serverKeyStore(Path certDir) throws Exception {
        Certificate cert = pem(certDir.resolve("server.crt"));
        PrivateKey key = privateKey(certDir.resolve("server.key"));
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, PASS);
        ks.setKeyEntry("server", key, PASS, new Certificate[] { cert });
        return ks;
    }

    private static KeyStore trustStore(Path certDir) throws Exception {
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, PASS);
        ks.setCertificateEntry("ca", pem(certDir.resolve("ca.crt")));
        return ks;
    }

    private static Certificate pem(Path p) throws Exception {
        try (InputStream in = Files.newInputStream(p)) {
            return CertificateFactory.getInstance("X.509").generateCertificate(in);
        }
    }

    private static PrivateKey privateKey(Path p) throws Exception {
        String text = Files.readString(p)
                .replace("-----BEGIN PRIVATE KEY-----", "")
                .replace("-----END PRIVATE KEY-----", "")
                .replaceAll("\\s", "");
        byte[] der = Base64.getDecoder().decode(text);
        return KeyFactory.getInstance("RSA").generatePrivate(new PKCS8EncodedKeySpec(der));
    }
}
