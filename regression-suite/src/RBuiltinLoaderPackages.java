/**
 * {@code getDefinedPackage} on the three BUILT-IN loaders: DEFINITION, not
 * visibility, and not module membership either.
 *
 * <h2>What this pins that {@code RLangPackages} cannot</h2>
 *
 * {@code RLangPackages} asserts the NEGATIVE half — that the application and
 * platform loaders must not claim {@code java.lang}. It passes on a VM whose
 * platform loader claims NOTHING, and that is exactly the residual the
 * {@code java.lang} fix knowingly shipped: the class-path segment probe it
 * narrowed to cannot answer for a loader whose classes come out of a jimage, so
 * every genuinely platform-defined package answered {@code null} where HotSpot
 * answers a {@code Package}. A vector that only asserts nulls cannot see the
 * difference between "correctly narrowed" and "blinded".
 *
 * <p>Nor can membership stand in for definition. {@code java.smartcardio} is a
 * platform module on every run; {@code javax.smartcardio} is still {@code null}
 * on HotSpot until a class in it is loaded, and non-null immediately after. The
 * BEFORE/AFTER pair below is the only row in this corpus that separates "the
 * loader COULD define this package" from "the loader HAS defined it", and it is
 * the row a module-table lookup with no loaded-class conjunct fails.
 *
 * <h2>And the walk</h2>
 *
 * {@code Package.getPackage(String)} is the delegating accessor: it walks the
 * parent chain and ends at the boot loader. It is the caller that notices when
 * the boot loader stops answering — deprecated, still used (TestNG's
 * {@code IgnoreListener} walks a test class's package chain through it) — and
 * it went {@code null} for {@code java.lang} on a VM whose only remaining
 * boot-loader probe was a class-file glob over a jimage. Asserting the walk
 * beside the non-delegating pair is what keeps a fix to one from silently
 * breaking the other.
 *
 * <p>Determinism: no timing, no identity hashes, no exception messages.
 */
public class RBuiltinLoaderPackages {
    static int checks;

    static void ck(String what, boolean ok, String detail) {
        checks++;
        if (!ok) {
            throw new AssertionError("RBuiltinLoaderPackages: " + what + ": " + detail);
        }
    }

    static String s(Package p) {
        return p == null ? "null" : p.getName();
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = RBuiltinLoaderPackages.class.getClassLoader();
        ClassLoader plat = ClassLoader.getPlatformClassLoader();

        // ---- 1. membership is not definition ---------------------------------
        // `javax.smartcardio` is in the platform module `java.smartcardio` on
        // every JDK image here, and is NOT defined until a class is loaded.
        ck("platform has not yet defined javax.smartcardio",
                plat.getDefinedPackage("javax.smartcardio") == null,
                "got " + s(plat.getDefinedPackage("javax.smartcardio")));
        ck("platform has not yet defined com.sun.net.httpserver",
                plat.getDefinedPackage("com.sun.net.httpserver") == null,
                "got " + s(plat.getDefinedPackage("com.sun.net.httpserver")));

        Class.forName("javax.smartcardio.CardTerminal");
        Class.forName("com.sun.net.httpserver.HttpServer");

        Package smartcardio = plat.getDefinedPackage("javax.smartcardio");
        ck("platform defines javax.smartcardio once a class is loaded",
                smartcardio != null, "still null after Class.forName");
        ck("javax.smartcardio names itself", "javax.smartcardio".equals(s(smartcardio)),
                "got " + s(smartcardio));
        Package httpserver = plat.getDefinedPackage("com.sun.net.httpserver");
        ck("platform defines com.sun.net.httpserver once a class is loaded",
                httpserver != null, "still null after Class.forName");
        ck("com.sun.net.httpserver names itself",
                "com.sun.net.httpserver".equals(s(httpserver)), "got " + s(httpserver));

        // ---- 2. java.sql, the package the residual was named for -------------
        Class.forName("java.sql.Connection");
        Class.forName("javax.sql.DataSource");
        Package sql = plat.getDefinedPackage("java.sql");
        ck("platform defines java.sql", sql != null, "the platform loader defines java.sql");
        ck("java.sql names itself", "java.sql".equals(s(sql)), "got " + s(sql));
        ck("platform defines javax.sql", plat.getDefinedPackage("javax.sql") != null,
                "javax.sql is in the java.sql module");
        // and the APPLICATION loader must not claim it, which is the half the
        // narrowing fix already had right.
        ck("app does not define java.sql", app.getDefinedPackage("java.sql") == null,
                "got " + s(app.getDefinedPackage("java.sql")));
        ck("app does not define javax.sql", app.getDefinedPackage("javax.sql") == null,
                "got " + s(app.getDefinedPackage("javax.sql")));

        // ---- 3. the boot half stays negative on both built-ins ---------------
        ck("app does not define java.lang", app.getDefinedPackage("java.lang") == null,
                "got " + s(app.getDefinedPackage("java.lang")));
        ck("platform does not define java.lang", plat.getDefinedPackage("java.lang") == null,
                "got " + s(plat.getDefinedPackage("java.lang")));
        ck("platform does not define java.util", plat.getDefinedPackage("java.util") == null,
                "got " + s(plat.getDefinedPackage("java.util")));
        ck("platform does not define this vector's own package",
                plat.getDefinedPackage("") == null,
                "got " + s(plat.getDefinedPackage("")));
        ck("neither loader fabricates an absent package",
                app.getDefinedPackage("no.such.package.here") == null
                        && plat.getDefinedPackage("no.such.package.here") == null,
                "a package nobody defined was fabricated");

        // ---- 4. the DELEGATING accessor must still walk ----------------------
        // Ends at the boot loader, which is where `java.lang` lives.
        Package walked = Package.getPackage("java.lang");
        ck("Package.getPackage walks to the boot loader", walked != null,
                "the deprecated delegating accessor returned null for java.lang");
        ck("the walk names java.lang", "java.lang".equals(s(walked)), "got " + s(walked));
        ck("the walk finds java.sql too", Package.getPackage("java.sql") != null,
                "getPackage must delegate to the platform loader");
        ck("the walk does not fabricate", Package.getPackage("no.such.package.here") == null,
                "getPackage answered for a package nobody defined");

        // ---- 5. the application loader's own packages still resolve ----------
        // The regression that would matter: narrowing the built-ins must not
        // blind the app loader, which is the Spring `BeanDefinitionLoader` case.
        Package self = RBuiltinLoaderPackages.class.getPackage();
        ck("this class has a Package", self != null, "the unnamed package has a Package object");
        ck("its name is the empty string", "".equals(s(self)), "got " + s(self));

        System.out.println("PASS RBuiltinLoaderPackages (" + checks + " checks)");
    }
}
