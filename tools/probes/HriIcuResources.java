import java.io.InputStream;
import java.text.Normalizer;

/**
 * java.base resources that live only in the run-time image (no class path
 * entry holds them): ICU data read by IDN / Normalizer / StringPrep, and the
 * Unicode name table. Under --compatible the Module.getResourceAsStream native
 * answered them from the flat class path and returned null, so IDN.<clinit>
 * failed and poisoned the process (see the Hibernate Reactive ICU page).
 * Every row must match HotSpot 25.
 */
public class HriIcuResources {
    static void row(String k, Object v) {
        System.out.println(k + "=" + v);
    }

    static String stream(InputStream is) {
        if (is == null) return "null";
        try (is) {
            int n = 0;
            byte[] b = new byte[8192];
            for (int r; (r = is.read(b)) > 0; ) n += r;
            return "bytes>0:" + (n > 0);
        } catch (Exception e) {
            return e.getClass().getName();
        }
    }

    static String call(java.util.concurrent.Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        String uprops = "jdk/internal/icu/impl/data/icudt76b/uprops.icu";
        row("module.uprops", stream(Object.class.getModule().getResourceAsStream(uprops)));
        row("module.uprops.slash", stream(Object.class.getModule().getResourceAsStream("/" + uprops)));
        row("class.uprops", stream(Object.class.getResourceAsStream("/" + uprops)));
        row("module.uniName", stream(Object.class.getModule().getResourceAsStream("java/lang/uniName.dat")));
        row("module.missing", stream(Object.class.getModule().getResourceAsStream("jdk/internal/icu/impl/data/nope.icu")));
        row("module.class", stream(Object.class.getModule().getResourceAsStream("java/lang/Object.class")));
        row("module.sql.class", stream(java.sql.Connection.class.getModule().getResourceAsStream("java/sql/Connection.class")));
        row("loader.uprops", stream(ClassLoader.getSystemResourceAsStream(uprops)));
        row("loader.uprops.url", ClassLoader.getSystemResource(uprops));
        row("loader.uprops.urls", call(() -> ClassLoader.getSystemResources(uprops).hasMoreElements()));
        row("loader.uniName", stream(ClassLoader.getSystemResourceAsStream("java/lang/uniName.dat")));
        row("loader.object.url", ClassLoader.getSystemResource("java/lang/Object.class"));
        row("platform.uprops", stream(ClassLoader.getPlatformClassLoader().getResourceAsStream(uprops)));
        row("platform.uprops.url", ClassLoader.getPlatformClassLoader().getResource(uprops));
        row("ccl.uprops", stream(Thread.currentThread().getContextClassLoader().getResourceAsStream(uprops)));
        row("ccl.uprops.url", Thread.currentThread().getContextClassLoader().getResource(uprops));
        row("loader.meta.url", ClassLoader.getSystemResource("META-INF/services/java.nio.file.spi.FileSystemProvider"));
        row("idn.toASCII", call(() -> java.net.IDN.toASCII("bücher.example")));
        row("idn.toUnicode", call(() -> java.net.IDN.toUnicode("xn--bcher-kva.example")));
        row("normalizer.nfd", call(() -> Normalizer.normalize("é", Normalizer.Form.NFD).length()));
        row("normalizer.nfkc", call(() -> Normalizer.normalize("ﬁ", Normalizer.Form.NFKC)));
        row("char.name", call(() -> Character.getName(0x00e9)));
        row("char.codePointOf", call(() -> Character.codePointOf("LATIN SMALL LETTER E WITH ACUTE")));
    }
}
