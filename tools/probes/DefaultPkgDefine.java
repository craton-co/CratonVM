import java.io.InputStream;
public class DefaultPkgDefine {
    static final class Iso extends ClassLoader {
        Iso() { super(null); }
        Class<?> def(String n, byte[] b) { return defineClass(n, b, 0, b.length); }
    }
    static String s(Package p) { return p == null ? "null" : "package[" + p.getName() + "]"; }
    public static void main(String[] a) throws Exception {
        Iso iso = new Iso();
        System.out.println("ROW iso.getDefinedPackage(\"\") BEFORE = " + s(iso.getDefinedPackage("")));
        byte[] b;
        try (InputStream in = DefaultPkgDefine.class.getResourceAsStream("/DefaultPkgDefine.class")) {
            b = in.readAllBytes();
        }
        Class<?> c = iso.def("DefaultPkgDefine", b);
        System.out.println("ROW defined loader is iso = " + (c.getClassLoader() == iso));
        System.out.println("ROW iso.getDefinedPackage(\"\") AFTER = " + s(iso.getDefinedPackage("")));
        System.out.println("ROW defined.getPackage() = " + s(c.getPackage()));
        System.out.println("ROW iso.getDefinedPackages().length = " + iso.getDefinedPackages().length);
        System.out.println("DONE DefaultPkgDefine");
    }
}
