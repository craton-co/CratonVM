import org.apache.catalina.util.ServerInfo;
import org.apache.tomcat.util.descriptor.web.WebXml;
import org.apache.tomcat.util.buf.UDecoder;
public class TomcatProbe {
    public static void main(String[] args) throws Exception {
        // 1. Version info (loads catalina.jar's core classes).
        System.out.println("Tomcat: " + ServerInfo.getServerInfo());
        System.out.println("Server number: " + ServerInfo.getServerNumber());
        System.out.println("Server built: " + ServerInfo.getServerBuilt());

        // 2. Build a WebXml descriptor — exercises tomcat-util's
        //    config + descriptor classes (used by every webapp deploy).
        WebXml webXml = new WebXml();
        webXml.setVersion("5.0");
        webXml.setDistributable(true);
        if (!webXml.isDistributable()) {
            System.out.println("FAIL: WebXml.isDistributable"); System.exit(1);
        }
        System.out.println("WebXml version=" + webXml.getVersion() + " distributable=" + webXml.isDistributable());

        // 3. URL decoder round-trip — tomcat-util.jar URL decoding path.
        String enc = "hello%20world%21";
        String dec = UDecoder.URLDecode(enc, java.nio.charset.StandardCharsets.UTF_8);
        if (!"hello world!".equals(dec)) {
            System.out.println("FAIL: URL decode: '" + dec + "'"); System.exit(1);
        }
        System.out.println("URL decode OK: " + dec);

        System.out.println("OK");
        System.exit(0);
    }
}
