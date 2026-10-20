import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLParameters;

public class SslSupportedParametersProbe {
    public static void main(String[] args) throws Exception {
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(null, null, null);
        System.out.println("context class: " + ctx.getClass());
        try {
            SSLParameters p = ctx.getSupportedSSLParameters();
            System.out.println("getSupportedSSLParameters class: " + p.getClass());
            String[] suites = p.getCipherSuites();
            System.out.println("cipherSuites runtime class: " + suites.getClass());
            System.out.println("cipherSuites length: " + suites.length);
        } catch (Throwable t) {
            t.printStackTrace();
            System.exit(1);
        }
        System.out.println("ALL OK");
    }
}
