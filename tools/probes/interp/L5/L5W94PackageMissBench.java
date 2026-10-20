// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 94, lane L5
// (`i93-L5-builtin-loader-package-miss-scans-the-class-store`): a built-in
// loader's `getDefinedPackage` of a package none of whose classes is loaded.
// HotSpot answers it with one `ConcurrentHashMap.get`; the VM first asks
// whether it defined a class of the package for that loader, which walked the
// whole class store on every miss (twice for a platform-module package).
// `CRATONVM_LOADER_PACKAGE_INDEX=1` makes that question one probe.
//
// Rows (ns per call, median of R rounds of N calls each, after a warm-up):
//   platform-miss   platform loader, `javax.sql.rowset` (java.sql.rowset
//                   module, platform-defined; no class of it is loaded here)
//   app-miss        application loader, a package nobody has
//   app-hit         application loader, this class's own (unnamed) package
//   platform-hit    platform loader, `java.sql` after `java.sql.Date` loads
//
// Output: "row <name> med <ns> ns/op" on stderr (timings), and
// "checksum <name> <answer>" on stdout (deterministic).
// Args: [n] (default 20000).
//
// Run: javac -d . L5W94PackageMissBench.java
//      java -cp . L5W94PackageMissBench
//      cratonvm --jdk-only   -cp . L5W94PackageMissBench
//      cratonvm --compatible -cp . L5W94PackageMissBench
//      CRATONVM_LOADER_PACKAGE_INDEX=1 cratonvm --jdk-only   -cp . L5W94PackageMissBench
//      CRATONVM_LOADER_PACKAGE_INDEX=1 cratonvm --compatible -cp . L5W94PackageMissBench
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 sources: a built-in
// loader's `getDefinedPackage` is its `packages` map, filled when a class of
// the package is defined; `java.sql.Date` is loaded before the `platform-hit`
// row; the main class's package is the unnamed one, named "")
// checksum platform-miss null
// checksum app-miss null
// checksum app-hit []
// checksum platform-hit [java.sql]
// HOTSPOT_EXPECTED_END
import java.util.Arrays;
import java.util.Locale;

public class L5W94PackageMissBench {
    static volatile Object sink;

    interface Ask {
        Package run();
    }

    static String answer(Package p) {
        return p == null ? "null" : "[" + p.getName() + "]";
    }

    static void row(String name, int n, Ask ask) {
        String first = answer(ask.run());
        for (int i = 0; i < n; i++) {
            sink = ask.run(); // warm-up
        }
        long[] t = new long[9];
        for (int r = 0; r < t.length; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink = ask.run();
            }
            t[r] = (System.nanoTime() - t0) / n;
        }
        Arrays.sort(t);
        System.err.printf(Locale.ROOT, "row %s med %d ns/op%n", name, t[t.length / 2]);
        System.out.println("checksum " + name + " " + first);
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        ClassLoader app = ClassLoader.getSystemClassLoader();
        row("platform-miss", n, () -> platform.getDefinedPackage("javax.sql.rowset"));
        row("app-miss", n, () -> app.getDefinedPackage("l5w94.no.such.pkg"));
        String own = L5W94PackageMissBench.class.getPackageName();
        row("app-hit", n, () -> app.getDefinedPackage(own));
        sink = Class.forName("java.sql.Date", false, platform);
        row("platform-hit", n, () -> platform.getDefinedPackage("java.sql"));
    }
}
