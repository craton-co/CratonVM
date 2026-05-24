import java.io.File;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.List;
import java.util.Properties;

public class PropsProbe2 {
    public static void main(String[] args) throws Exception {
        // Build URLClassLoader from activemq lib jars (mimics console Main)
        String home = System.getProperty("activemq.home", args.length > 0 ? args[0] : ".");
        File libDir = new File(home, "lib");
        List<URL> urls = new ArrayList<>();
        for (File sub : new File[]{libDir, new File(libDir, "optional"), new File(libDir, "web"), new File(libDir, "extra")}) {
            if (!sub.isDirectory()) continue;
            File[] files = sub.listFiles();
            if (files == null) continue;
            for (File f : files) {
                if (f.getName().endsWith(".jar") || f.getName().endsWith(".zip")) {
                    urls.add(f.toURI().toURL());
                }
            }
        }
        System.out.println("URL count = " + urls.size());
        URLClassLoader ucl = new URLClassLoader(urls.toArray(new URL[0]), PropsProbe2.class.getClassLoader());
        Thread.currentThread().setContextClassLoader(ucl);
        System.out.println("TCCL set to URLClassLoader");

        String path = "META-INF/services/org/apache/activemq/broker/xbean";
        InputStream in = ucl.getResourceAsStream(path);
        System.out.println("URLClassLoader stream null? " + (in == null));
        if (in == null) {
            in = PropsProbe2.class.getClassLoader().getResourceAsStream(path);
            System.out.println("App CL stream null? " + (in == null));
        }
        if (in == null) {
            System.out.println("NO STREAM");
            return;
        }
        byte[] buf = new byte[2048];
        int total = 0;
        java.io.ByteArrayOutputStream baos = new java.io.ByteArrayOutputStream();
        int n;
        while ((n = in.read(buf)) > 0) { baos.write(buf, 0, n); total += n; }
        in.close();
        System.out.println("bytes read = " + total);
        byte[] data = baos.toByteArray();
        System.out.println("as string:");
        String s = new String(data, java.nio.charset.StandardCharsets.ISO_8859_1);
        System.out.println(s);
        System.out.println("---END RAW---");

        Properties p = new Properties();
        p.load(new java.io.ByteArrayInputStream(data));
        System.out.println("Properties size = " + p.size());
        System.out.println("class property = " + p.getProperty("class"));
        for (Object k : p.keySet()) {
            System.out.println("  key=[" + k + "] val=[" + p.getProperty((String) k) + "]");
        }

        // Now use the actual FactoryFinder pattern through TCCL
        ClassLoader tccl = Thread.currentThread().getContextClassLoader();
        System.out.println("---using TCCL again---");
        InputStream in2 = tccl.getResourceAsStream(path);
        System.out.println("TCCL stream null? " + (in2 == null));
        if (in2 != null) {
            Properties p2 = new Properties();
            p2.load(in2);
            in2.close();
            System.out.println("class property2 = " + p2.getProperty("class"));
            System.out.println("size2 = " + p2.size());
        }
    }
}
