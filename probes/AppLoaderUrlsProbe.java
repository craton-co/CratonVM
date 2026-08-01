import java.net.URL;
import java.net.URLClassLoader;

/** Reports the shape of the application class loader and what getURLs() answers. */
public class AppLoaderUrlsProbe {

    public static void main(String[] args) throws Exception {
        ClassLoader scl = ClassLoader.getSystemClassLoader();
        System.out.println("systemClassLoader   = " + scl);
        System.out.println("  class             = " + (scl == null ? "null" : scl.getClass().getName()));
        System.out.println("  isURLClassLoader  = " + (scl instanceof URLClassLoader));
        ClassLoader own = AppLoaderUrlsProbe.class.getClassLoader();
        System.out.println("own class loader    = " + own);
        System.out.println("  class             = " + (own == null ? "null" : own.getClass().getName()));
        System.out.println("  isURLClassLoader  = " + (own instanceof URLClassLoader));
        System.out.println("  same as scl       = " + (own == scl));
        if (own instanceof URLClassLoader ucl) {
            URL[] urls = ucl.getURLs();
            System.out.println("own.getURLs() count = " + urls.length);
            for (URL u : urls) {
                System.out.println("    " + u);
            }
        }
        System.out.println("java.class.path     = " + System.getProperty("java.class.path"));
    }
}
