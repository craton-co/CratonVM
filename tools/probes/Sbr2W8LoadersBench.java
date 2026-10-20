// SBR2 wave 8, lane LOADERS: cost of the class-loader paths the adoption touches.
// Prints ms per phase; run interleaved A/B, medians of >= 5 reps.
// Run: java|cratonvm [--jdk-only] -cp out Sbr2W8LoadersBench [n]
import java.util.ServiceLoader;

public class Sbr2W8LoadersBench {
    // Class-path service: the run dir needs META-INF/services/Sbr2W8LoadersBench$Svc
    // naming Sbr2W8LoadersBench$Impl (module services need the jimage native).
    public interface Svc {}
    public static class Impl implements Svc {}

    static long t() { return System.nanoTime(); }

    public static void main(String[] a) throws Exception {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        long s = t();
        long h = 0;
        for (int i = 0; i < n; i++) {
            h += Class.forName("java.util.ArrayList", false, sys).hashCode();
            h += Class.forName("Sbr2W8LoadersBench", false, sys).hashCode();
        }
        System.out.println("forName ms: " + (t() - s) / 1_000_000);
        s = t();
        for (int i = 0; i < n; i++) {
            h += sys.loadClass("java.util.HashMap").hashCode();
            h += (Sbr2W8LoadersBench.class.getClassLoader() == sys ? 1 : 0);
            h += (String.class.getClassLoader() == null ? 1 : 0);
            h += (java.sql.Connection.class.getClassLoader() != null ? 1 : 0);
        }
        System.out.println("loadClass+getClassLoader ms: " + (t() - s) / 1_000_000);
        s = t();
        for (int i = 0; i < n / 10; i++) {
            h += Sbr2W8LoadersBench.class.desiredAssertionStatus() ? 1 : 0;
            h += ClassLoader.getSystemClassLoader().hashCode() + ClassLoader.getPlatformClassLoader().hashCode();
        }
        System.out.println("assert+getSystem ms: " + (t() - s) / 1_000_000);
        s = t();
        int c = 0;
        for (int i = 0; i < n / 100; i++) {
            for (Svc p : ServiceLoader.load(Svc.class)) c++;
        }
        System.out.println("ServiceLoader ms: " + (t() - s) / 1_000_000 + " " + c);
        System.out.println("sink " + h);
    }
}
