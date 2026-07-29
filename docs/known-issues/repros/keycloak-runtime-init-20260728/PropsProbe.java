import java.io.ByteArrayInputStream;
import java.util.Properties;

public class PropsProbe {
    public static void main(String[] a) throws Exception {
        Properties p = new Properties();
        p.load(new ByteArrayInputStream("infinispan.version=16.0.8\ninfinispan.brand.name=Infinispan\n".getBytes("UTF-8")));
        System.out.println("size=" + p.size());
        System.out.println("get1=" + p.getProperty("infinispan.version"));
        System.out.println("get2=" + p.getProperty("infinispan.version", "0.0.0-SNAPSHOT"));
        System.out.println("missing2=" + p.getProperty("nope", "DEF"));
        System.out.println("== DONE OK ==");
    }
}
