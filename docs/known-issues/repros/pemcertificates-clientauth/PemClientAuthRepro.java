import java.io.FileInputStream;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.security.spec.PKCS8EncodedKeySpec;
import java.security.KeyFactory;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;
import java.util.Base64;

public class PemClientAuthRepro {
    public static void main(String[] args) throws Exception {
        String scratch = args[0];

        // ---- Server side: PEM cert+key, in-memory KeyStore (matches PemSslStoreBundle) ----
        byte[] certPem = java.nio.file.Files.readAllBytes(java.nio.file.Path.of(scratch, "test-cert.pem"));
        byte[] keyPem = java.nio.file.Files.readAllBytes(java.nio.file.Path.of(scratch, "test-key.pem"));
        CertificateFactory cf = CertificateFactory.getInstance("X.509");
        Certificate serverCert = cf.generateCertificate(new java.io.ByteArrayInputStream(certPem));

        String keyPemStr = new String(keyPem, java.nio.charset.StandardCharsets.US_ASCII);
        int kStart = keyPemStr.indexOf("-----BEGIN PRIVATE KEY-----") + "-----BEGIN PRIVATE KEY-----".length();
        int kEnd = keyPemStr.indexOf("-----END PRIVATE KEY-----");
        String keyB64 = keyPemStr.substring(kStart, kEnd).replaceAll("\\s", "");
        byte[] keyDer = Base64.getDecoder().decode(keyB64);
        PrivateKey serverKey = KeyFactory.getInstance("RSA").generatePrivate(new PKCS8EncodedKeySpec(keyDer));

        KeyStore serverKs = KeyStore.getInstance(KeyStore.getDefaultType());
        serverKs.load(null);
        serverKs.setKeyEntry("ssl", serverKey, new char[0], new Certificate[]{serverCert});

        KeyStore serverTrust = KeyStore.getInstance(KeyStore.getDefaultType());
        serverTrust.load(null);
        serverTrust.setCertificateEntry("ssl", serverCert);

        KeyManagerFactory serverKmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        serverKmf.init(serverKs, new char[0]);
        TrustManagerFactory serverTmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        serverTmf.init(serverTrust);

        SSLContext serverCtx = SSLContext.getInstance("TLS");
        serverCtx.init(serverKmf.getKeyManagers(), serverTmf.getTrustManagers(), null);

        SSLServerSocket serverSocket = (SSLServerSocket) serverCtx.getServerSocketFactory().createServerSocket(0);
        serverSocket.setNeedClientAuth(true);
        int port = serverSocket.getLocalPort();
        System.out.println("server listening on " + port);

        Thread serverThread = new Thread(() -> {
            try {
                SSLSocket s = (SSLSocket) serverSocket.accept();
                s.startHandshake();
                System.out.println("SERVER handshake OK, peer=" + s.getSession().getPeerPrincipal());
                s.close();
            } catch (Exception ex) {
                System.out.println("SERVER handshake FAILED: " + ex);
                ex.printStackTrace();
            }
        });
        serverThread.start();

        // ---- Client side: PKCS12 test.p12 ----
        KeyStore clientKs = KeyStore.getInstance("PKCS12");
        try (FileInputStream in = new FileInputStream(java.nio.file.Path.of(scratch, "test.p12").toFile())) {
            clientKs.load(in, "secret".toCharArray());
        }
        KeyManagerFactory clientKmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        clientKmf.init(clientKs, "secret".toCharArray());

        KeyStore clientTrust = KeyStore.getInstance(KeyStore.getDefaultType());
        clientTrust.load(null);
        clientTrust.setCertificateEntry("ssl", serverCert);
        TrustManagerFactory clientTmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        clientTmf.init(clientTrust);

        SSLContext clientCtx = SSLContext.getInstance("TLS");
        clientCtx.init(clientKmf.getKeyManagers(), clientTmf.getTrustManagers(), null);

        try {
            SSLSocket client = (SSLSocket) clientCtx.getSocketFactory().createSocket("localhost", port);
            client.startHandshake();
            System.out.println("CLIENT handshake OK, peer=" + client.getSession().getPeerPrincipal());
            client.close();
        } catch (Exception ex) {
            System.out.println("CLIENT handshake FAILED: " + ex);
            ex.printStackTrace();
        }
        serverThread.join();
    }
}
