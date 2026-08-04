import java.io.*;
import java.net.URL;
public class ResProbe {
    public static void main(String[] a) throws Exception {
        String name = "/META-INF/loader/spring-boot-loader.jar";
        URL u = ResProbe.class.getResource(name);
        System.out.println("url=" + u);
        try (InputStream in = ResProbe.class.getResourceAsStream(name)) {
            if (in == null) { System.out.println("stream=null"); return; }
            byte[] buf = new byte[16];
            int n = in.read(buf);
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i < n; i++) sb.append(String.format("%02x", buf[i]));
            long total = n;
            byte[] big = new byte[65536];
            int r;
            while ((r = in.read(big)) > 0) total += r;
            System.out.println("first16=" + sb + " total=" + total);
        }
    }
}
