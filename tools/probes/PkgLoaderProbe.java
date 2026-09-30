import java.util.Arrays;

/**
 * The full getDefinedPackage / getDefinedPackages / getPackages surface for the
 * three BUILT-IN loaders, printed as a value table so a cross-VM diff names the
 * row that moved. No assertions: this is an oracle-diffed probe.
 */
public class PkgLoaderProbe {
    static String s(Package p) { return p == null ? "null" : p.getName(); }

    static void row(String what, Object got) {
        System.out.println("ROW " + what + " = " + got);
    }

    public static void main(String[] a) throws Exception {
        ClassLoader app = PkgLoaderProbe.class.getClassLoader();
        ClassLoader plat = ClassLoader.getPlatformClassLoader();

        // Force the classes so their packages are certainly defined.
        Class.forName("java.sql.Connection");
        Class.forName("javax.sql.DataSource");
        Class.forName("java.util.zip.ZipFile");

        row("app.class", app == null ? "null" : app.getClass().getName());
        row("plat.class", plat == null ? "null" : plat.getClass().getName());
        row("app.parent", app == null ? "null" : String.valueOf(app.getParent()));
        row("plat.parent", plat == null ? "null" : String.valueOf(plat.getParent()));

        for (String p : new String[] {
                "java.lang", "java.util", "java.io", "java.sql", "javax.sql",
                "java.util.zip", "com.example.app", "no.such.package" }) {
            row("app.getDefinedPackage(" + p + ")", s(app.getDefinedPackage(p)));
            row("plat.getDefinedPackage(" + p + ")", s(plat.getDefinedPackage(p)));
        }

        // Which loader DEFINES these classes, per the JDK.
        row("String.class.getClassLoader", String.valueOf(String.class.getClassLoader()));
        row("Connection.class.getClassLoader",
                String.valueOf(Class.forName("java.sql.Connection").getClassLoader()));
        row("DataSource.class.getClassLoader",
                String.valueOf(Class.forName("javax.sql.DataSource").getClassLoader()));
        row("Probe.class.getClassLoader", String.valueOf(PkgLoaderProbe.class.getClassLoader()));

        // Module names, so the mapping the VM would need is visible.
        row("Connection.module", Class.forName("java.sql.Connection").getModule().getName());
        row("String.module", String.class.getModule().getName());
        row("ZipFile.module", Class.forName("java.util.zip.ZipFile").getModule().getName());

        // The delegating accessor must still walk.
        row("Package.getPackage(java.lang)", s(Package.getPackage("java.lang")));

        Package[] appDefined = app.getDefinedPackages();
        row("app.getDefinedPackages.componentType",
                appDefined.getClass().getComponentType().getName());
        row("app.getDefinedPackages.hasJavaLang",
                Arrays.stream(appDefined).anyMatch(p -> p.getName().equals("java.lang")));
        Package[] platDefined = plat.getDefinedPackages();
        row("plat.getDefinedPackages.componentType",
                platDefined.getClass().getComponentType().getName());
        row("plat.getDefinedPackages.hasJavaSql",
                Arrays.stream(platDefined).anyMatch(p -> p.getName().equals("java.sql")));

        Package[] all = Package.getPackages();
        row("Package.getPackages.componentType", all.getClass().getComponentType().getName());

        // The strong control: a genuinely app-classpath-defined package.
        Class.forName("com.example.app.Marker");
        row("app.getDefinedPackage(com.example.app) after load",
                s(app.getDefinedPackage("com.example.app")));
        row("Marker.getPackage", s(Class.forName("com.example.app.Marker").getPackage()));

        System.out.println("DONE PkgLoaderProbe");
    }
}
