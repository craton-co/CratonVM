import java.io.InputStream;
import java.util.Properties;

public class PropsProbe {
    public static void main(String[] args) throws Exception {
        String path = "META-INF/services/org/apache/activemq/broker/xbean";
        ClassLoader cl = Thread.currentThread().getContextClassLoader();
        System.out.println("TCCL = " + cl);
        InputStream in = null;
        if (cl != null) in = cl.getResourceAsStream(path);
        System.out.println("TCCL stream null? " + (in == null));
        if (in == null) {
            in = PropsProbe.class.getClassLoader().getResourceAsStream(path);
            System.out.println("App CL stream null? " + (in == null));
        }
        if (in == null) {
            System.out.println("NO STREAM");
            return;
        }
        // Read raw bytes first
        byte[] buf = new byte[2048];
        int total = 0;
        java.io.ByteArrayOutputStream baos = new java.io.ByteArrayOutputStream();
        int n;
        while ((n = in.read(buf)) > 0) { baos.write(buf, 0, n); total += n; }
        in.close();
        System.out.println("bytes read = " + total);
        byte[] data = baos.toByteArray();
        System.out.println("first 80 bytes as hex:");
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < Math.min(80, data.length); i++) {
            sb.append(String.format("%02x ", data[i] & 0xff));
        }
        System.out.println(sb.toString());
        System.out.println("as string (first 300 chars):");
        String s = new String(data, java.nio.charset.StandardCharsets.ISO_8859_1);
        System.out.println(s.substring(0, Math.min(300, s.length())));
        System.out.println("---END RAW---");

        // Now try Properties.load
        Properties p = new Properties();
        p.load(new java.io.ByteArrayInputStream(data));
        System.out.println("Properties size = " + p.size());
        System.out.println("class property = " + p.getProperty("class"));
        for (Object k : p.keySet()) {
            System.out.println("  key=[" + k + "] val=[" + p.getProperty((String) k) + "]");
        }
    }
}
