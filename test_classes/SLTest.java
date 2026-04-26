import java.util.ServiceLoader;
import java.nio.charset.Charset;
import java.nio.charset.spi.CharsetProvider;
import java.util.Iterator;

public class SLTest {
    public static void main(String[] args) {
        int count = 0;
        Iterator<CharsetProvider> it = ServiceLoader.load(CharsetProvider.class).iterator();
        while (it.hasNext()) {
            CharsetProvider p = it.next();
            System.out.println("provider: " + p.getClass().getName());
            count++;
        }
        System.out.println("total providers: " + count);
        // Real check: Charset.forName("UTF-8") must succeed since the
        // built-in providers include one that returns it.
        Charset utf8 = Charset.forName("UTF-8");
        System.out.println("utf8=" + utf8.name());
        if (!"UTF-8".equals(utf8.name())) { System.err.println("FAIL"); System.exit(1); }
        System.out.println("OK");
    }
}
