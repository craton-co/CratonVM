// SBR2 wave 8, lane LOADERS: behaviour of the rows the adoption retires
// (Class.getClassLoader0 readers, getPackage, getResourceAsStream, forName(Module,..),
// Module.getPackages, BootLoader resources). Diff against HotSpot 25 verbatim.
// Run: javac -d out Sbr2W8LoadersRows.java
//      java|cratonvm [--jdk-only|--compatible] -cp out Sbr2W8LoadersRows
import java.io.InputStream;

public class Sbr2W8LoadersRows {
    static void p(String k, Object v) { System.out.println(k + ": " + v); }

    public static void main(String[] a) throws Exception {
        Class<?> sql = java.sql.Connection.class;
        Class<?> logm = java.util.logging.Logger.class;
        p("sql loader", sql.getClassLoader() == ClassLoader.getPlatformClassLoader());
        p("logging loader", logm.getClassLoader());
        p("self pkg", Sbr2W8LoadersRows.class.getPackage().getName().isEmpty());
        p("sql pkg", sql.getPackage());
        p("String pkg", String.class.getPackage());
        p("String assertions", String.class.desiredAssertionStatus());
        p("self assertions", Sbr2W8LoadersRows.class.desiredAssertionStatus());
        p("sql assertions", sql.desiredAssertionStatus());
        try (InputStream in = Sbr2W8LoadersRows.class.getResourceAsStream("Sbr2W8LoadersRows.class")) { p("self res", in != null); }
        try (InputStream in = Sbr2W8LoadersRows.class.getResourceAsStream("/Sbr2W8LoadersRows.class")) { p("self res abs", in != null); }
        try (InputStream in = Sbr2W8LoadersRows.class.getResourceAsStream("/nope.txt")) { p("missing res", in); }
        try (InputStream in = String.class.getResourceAsStream("String.class")) { p("String res", in != null); }
        try (InputStream in = sql.getResourceAsStream("Connection.class")) { p("sql res", in != null); }
        p("forName(base,String)", Class.forName(Object.class.getModule(), "java.lang.String"));
        p("forName(base,app)", Class.forName(Object.class.getModule(), "Sbr2W8LoadersRows"));
        p("forName(sql,Connection)", Class.forName(sql.getModule(), "java.sql.Connection"));
        p("forName(base,sql)", Class.forName(Object.class.getModule(), "java.sql.Connection"));
        p("forName(base,missing)", Class.forName(Object.class.getModule(), "no.Such"));
        p("base has java.lang", Object.class.getModule().getPackages().contains("java.lang"));
        p("sql has java.sql", sql.getModule().getPackages().contains("java.sql"));
        p("unnamed has self pkg", Sbr2W8LoadersRows.class.getModule().getPackages());
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        p("sys.loadClass(String)", sys.loadClass("java.lang.String"));
        p("sys.loadClass(sql)", sys.loadClass("java.sql.Connection"));
        p("sys.loadClass([I)", tryLoad(sys, "[I"));
        p("plat.loadClass(app)", tryLoad(ClassLoader.getPlatformClassLoader(), "Sbr2W8LoadersRows"));
        p("sys res", sys.getResource("java/lang/Object.class") != null);
        p("plat res app", ClassLoader.getPlatformClassLoader().getResource("Sbr2W8LoadersRows.class"));
        p("boot res", Object.class.getClassLoader() == null && ClassLoader.getSystemResource("java/lang/String.class") != null);
        p("custom child", new ClassLoader(sys) {}.loadClass("Sbr2W8LoadersRows") == Sbr2W8LoadersRows.class);
        p("array loader", Sbr2W8LoadersRows[].class.getClassLoader() == Sbr2W8LoadersRows.class.getClassLoader());
        p("sys defined pkg", sys.getDefinedPackage("java.sql"));
        p("Package.getPackage sql", Package.getPackage("java.sql"));
    }

    static Object tryLoad(ClassLoader l, String n) {
        try { return l.loadClass(n); } catch (Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    }
}
