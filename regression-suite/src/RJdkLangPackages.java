// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * The PLURAL package methods: `ClassLoader.getPackages()` and the static
 * `Package.getPackages()`.
 *
 * Both answered an empty `Package[]` on this VM until 2026-09-11 -- four
 * registrations in three files agreed to, all citing one stale premise -- where
 * HotSpot 25 answers 91 on this program. The singular methods
 * (`Package.getPackage`, `Class.getPackage`, `ClassLoader.getDefinedPackage`)
 * were right throughout, which is why nothing in the corpus caught it.
 *
 * EVERY published line is a BOOLEAN or a type name, never a count. The suite
 * diffs this vector's `CK` lines against a HotSpot oracle, and the counts
 * legitimately differ: both VMs answer "how many boot packages have a loaded
 * class right now", and CratonVM answers much of its own bootstrap with natives
 * where HotSpot runs JDK bytecode (35 against 91 when this landed). The facts
 * that must agree are the ones the defect moved -- non-empty, no nulls, the
 * component type, `java.lang` present, and that touching a new package moves the
 * answer at all.
 *
 * Nothing here prints outside the `PASS`/`CK` prefixes: a line the oracle prints
 * and `extract()` drops is itself a harness error (G1).
 *
 * Record: docs/internal/jdk-only/package-getpackages-answered-empty-FIXED-20260911.md
 */
public class RJdkLangPackages {
    static int checks = 0;

    static void ck(String key, Object value) {
        System.out.println("CK RJdkLangPackages " + key + "=" + value);
        checks++;
    }

    static void must(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    public static void main(String[] args) throws Exception {
        // The package table holds what a LOADED class put there, on HotSpot as
        // here, so touch a few boot packages before asking.
        Class.forName("java.util.ArrayList");
        Class.forName("java.io.File");
        Class.forName("java.nio.file.Path");

        audit("static", Package.getPackages());

        ClassLoader app = RJdkLangPackages.class.getClassLoader();
        must(app != null, "the application class loader must not be null");
        Package[] viaLoader = new Peek(app).peek();
        audit("loader", viaLoader);

        // `getDefinedPackages()` is a different contract -- only what THIS loader
        // defined -- and may legitimately be empty. What must not happen is the
        // plural boot view collapsing to it.
        ck("loaderCoversDefined", viaLoader.length >= app.getDefinedPackages().length);

        // The answer is DERIVED from the loaded classes, so touching a new
        // package must move it. A frozen table would satisfy every line above.
        int before = Package.getPackages().length;
        Class.forName("java.util.zip.ZipFile");
        Class.forName("javax.crypto.Cipher");
        int after = Package.getPackages().length;
        ck("growsWhenANewPackageIsTouched", after > before);
        ck("hasJavaUtilZipAfterTouch", contains(Package.getPackages(), "java.util.zip"));

        System.out.println("PASS RJdkLangPackages (" + checks + " checks)");
    }

    static void audit(String tag, Package[] ps) {
        must(ps != null, tag + ": getPackages() must not return null");
        ck(tag + ".componentType", ps.getClass().getComponentType().getName());
        ck(tag + ".nonEmpty", ps.length > 0);
        int nulls = 0;
        boolean namesPresent = true;
        for (Package p : ps) {
            if (p == null) {
                nulls++;
            } else if (p.getName() == null) {
                // The UNNAMED package is legal and HotSpot reports it, with
                // `getName()` an empty string -- so only a null name is a defect.
                namesPresent = false;
            }
        }
        ck(tag + ".nullElements", nulls);
        ck(tag + ".everyNameNonNull", namesPresent);
        ck(tag + ".hasJavaLang", contains(ps, "java.lang"));
        ck(tag + ".hasJavaIo", contains(ps, "java.io"));
        must(nulls == 0, tag + ": getPackages() must not contain a null element");
        must(ps.length > 0, tag + ": getPackages() must not be empty");
    }

    static boolean contains(Package[] ps, String name) {
        for (Package p : ps) {
            if (p != null && name.equals(p.getName())) {
                return true;
            }
        }
        return false;
    }

    /** `ClassLoader.getPackages()` is protected, so only a subclass may call it. */
    static final class Peek extends ClassLoader {
        Peek(ClassLoader parent) {
            super(parent);
        }

        Package[] peek() {
            return getPackages();
        }
    }
}
