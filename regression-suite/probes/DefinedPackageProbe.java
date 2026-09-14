// Does `ClassLoader.getDefinedPackage(name)` answer only for packages THIS
// loader defined, or does it answer for any package it can see?
//
// `RLangPackages` (regression-suite, WORKER 3's `caab31d49`) fails its very
// first check on a CratonVM built from the 2026-08-22 integrated tree:
//
//     AssertionError: appLoader.getDefinedPackage(java.lang)==null:
//     the application loader does not DEFINE java.lang, got package java.lang
//
// HotSpot answers `null` and the vector passes with 27 checks. The contract is
// explicit (java.lang.ClassLoader): getDefinedPackage returns a Package "that
// has been defined by this class loader", and `java.lang` is defined by the
// BOOT loader, not the application loader. getPackage()/getPackages() are the
// ones that walk the delegation chain.
//
// This probe separates the three questions the one assertion conflates, so the
// fix can be aimed:
//   1. does getDefinedPackage answer for a package this loader did not define?
//   2. does it do so for the BOOT packages only, or for the parent's too?
//   3. does getPackage (which SHOULD walk) still work?
//
// Run:  cratonvm --jdk-only -cp <dir> DefinedPackageProbe
//       java -cp <dir> DefinedPackageProbe          # the oracle
public class DefinedPackageProbe {

    static void row(String what, Object got) {
        System.out.println("  " + what + " = " + (got == null ? "null" : got.toString()));
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader plat = app.getParent();

        System.out.println("app loader  = " + app);
        System.out.println("platform    = " + plat);
        System.out.println("this class defined by = " + DefinedPackageProbe.class.getClassLoader());

        // (1) A boot package. The app loader did NOT define it, so
        //     getDefinedPackage must be null on every conforming VM.
        System.out.println("getDefinedPackage on the APPLICATION loader:");
        row("java.lang        (boot)", app.getDefinedPackage("java.lang"));
        row("java.util        (boot)", app.getDefinedPackage("java.util"));
        row("java.io          (boot)", app.getDefinedPackage("java.io"));
        row("no.such.package        ", app.getDefinedPackage("no.such.package"));

        // (2) The same question one loader up.
        if (plat != null) {
            System.out.println("getDefinedPackage on the PLATFORM loader:");
            row("java.lang        (boot)", plat.getDefinedPackage("java.lang"));
        }

        // (3) getPackage DOES walk the delegation chain, so a non-null here is
        //     correct and a null would be a different defect. Deprecated, but
        //     it is the method whose semantics getDefinedPackage is contrasted
        //     with, so measuring both is what makes (1) unambiguous.
        System.out.println("getPackage (walks the chain — non-null is CORRECT):");
        row("java.lang        (boot)", Package.getPackage("java.lang"));

        // (4) The unnamed package of the app loader: this class's own. The app
        //     loader DID define it, so a non-null here is correct and is the
        //     positive control that (1) is not simply "always null".
        System.out.println("the loader's OWN package (positive control):");
        Package own = DefinedPackageProbe.class.getPackage();
        row("this class's package   ", own == null ? "null (unnamed package)" : own);

        // A definable, named package the app loader really does define needs a
        // packaged class; the unnamed package above reads `null` on a
        // conforming VM too, so state plainly that (4) is weak rather than
        // implying it proved something.
        System.out.println("NOTE: (4) is a WEAK control — a class in the unnamed");
        System.out.println("      package answers null on a conforming VM as well.");
        System.out.println("      The STRONG control needs a class in a NAMED package on the");
        System.out.println("      classpath, which a single-file probe cannot carry. It was");
        System.out.println("      measured separately and is reproduced in WORKER-5-NOTE-8 §7:");
        System.out.println("      com.example.app resolves non-null on HotSpot and on the fixed");
        System.out.println("      VM, so the fix narrows the probe WITHOUT blinding it.");
    }
}
