/**
 * Witness for compatible-sslsocket-checks-the-host-name-without-an-endpoint-identification-algorithm: a key pair
 * with CN=localhost and no subjectAltName is both key and trust material. Create it first:
 *   keytool -genkeypair -alias s -keyalg RSA -keysize 2048 -dname CN=localhost -validity 30 -keystore ks.p12 -storetype PKCS12 -storepass changeit -keypass changeit
 * HotSpot: plain-localhost ok, plain-ip ok, https-localhost ok, https-ip SSLHandshakeException.
 */
import javax.net.ssl.*; import java.io.*; import java.net.*; import java.security.*;
public class SanLessSslSocket {
  static String run(SSLContext c, String host, boolean https) {
    try (SSLServerSocket ss=(SSLServerSocket)c.getServerSocketFactory().createServerSocket(0)) {
      Thread t=new Thread(()->{ try (SSLSocket s=(SSLSocket)ss.accept()) { s.startHandshake(); s.getInputStream().read(); } catch(Throwable e){} }); t.setDaemon(true); t.start();
      try (SSLSocket s=(SSLSocket)c.getSocketFactory().createSocket(host, ss.getLocalPort())) {
        if (https) { SSLParameters p=s.getSSLParameters(); p.setEndpointIdentificationAlgorithm("HTTPS"); s.setSSLParameters(p); }
        s.startHandshake();
        return "ok "+s.getSession().getProtocol()+" "+s.getSession().getPeerCertificates().length;
      }
    } catch(Throwable e){ return "!"+e.getClass().getName()+":"+String.valueOf(e.getMessage()).split("\n")[0]; }
  }
  public static void main(String[] a) throws Exception {
    KeyStore ks=KeyStore.getInstance("PKCS12"); try(InputStream in=new FileInputStream("ks.p12")){ ks.load(in,"changeit".toCharArray()); }
    KeyManagerFactory kmf=KeyManagerFactory.getInstance("SunX509"); kmf.init(ks,"changeit".toCharArray());
    TrustManagerFactory tmf=TrustManagerFactory.getInstance("PKIX"); tmf.init(ks);
    SSLContext c=SSLContext.getInstance("TLS"); c.init(kmf.getKeyManagers(), tmf.getTrustManagers(), null);
    System.out.println("plain-localhost "+run(c,"localhost",false));
    System.out.println("plain-ip "+run(c,"127.0.0.1",false));
    System.out.println("https-localhost "+run(c,"localhost",true));
    System.out.println("https-ip "+run(c,"127.0.0.1",true));
  }
}
