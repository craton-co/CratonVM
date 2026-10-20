// SBR2 wave 9, lane MODLOAD: Class.getPackage() of classes defined from a JAR that carries a manifest
// (Implementation-/Specification- attributes, Sealed), plus identity / getDefinedPackage agreement.
// The JDK's own Class.getPackage body reads the version info the loader recorded when it defined the
// class from the jar; this VM serves such classes from its class table, so the manifest has to come
// from the package producer. Diff against HotSpot 25 verbatim.
//
//   mkdir pkgsrc/com/sbr2w9pkg && write A.java, B.java (package com.sbr2w9pkg)
//   javac -d pkgout pkgsrc/com/sbr2w9pkg/*.java ; jar --create --file pkg.jar --manifest mf.txt -C pkgout .
//   javac -d out Sbr2W9ModloadPkg.java
//   java|cratonvm [--jdk-only|--compatible] -cp out:pkg.jar Sbr2W9ModloadPkg
public class Sbr2W9ModloadPkg {
    static void p(String k, Object v) { System.out.println(k + ": " + v); }

    interface Th { Object get() throws Throwable; }

    static void t(String k, Th th) {
        try { p(k, th.get()); } catch (Throwable x) { p(k, "THROWS " + x.getClass().getName() + ": " + x.getMessage()); }
    }

    public static void main(String[] a) throws Exception {
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        Class<?> ca = Class.forName("com.sbr2w9pkg.A");
        Class<?> cb = Class.forName("com.sbr2w9pkg.B");
        Package pa = ca.getPackage();
        t("pkg", () -> pa);
        t("pkg name", () -> pa.getName());
        t("pkg ==", () -> pa == cb.getPackage());
        t("pkg twice ==", () -> ca.getPackage() == ca.getPackage());
        t("pkg == defined", () -> pa == sys.getDefinedPackage("com.sbr2w9pkg"));
        t("pkg == Package.getPackage", () -> pa == Package.getPackage("com.sbr2w9pkg"));
        t("impl title", () -> pa.getImplementationTitle());
        t("impl version", () -> pa.getImplementationVersion());
        t("impl vendor", () -> pa.getImplementationVendor());
        t("spec title", () -> pa.getSpecificationTitle());
        t("spec version", () -> pa.getSpecificationVersion());
        t("spec vendor", () -> pa.getSpecificationVendor());
        t("sealed", () -> pa.isSealed());
        t("compatible 1.0", () -> pa.isCompatibleWith("1.0"));
        t("toString", () -> pa.toString());
        t("module", () -> ca.getModule().isNamed());
        t("annotations", () -> pa.getAnnotations().length);
        // a class loaded by an explicit loadClass first, package asked from the class
        Class<?> cc = sys.loadClass("com.sbr2w9pkg.C");
        t("C pkg ==", () -> cc.getPackage() == pa);
        // the boot and platform side
        t("String pkg spec title", () -> String.class.getPackage().getSpecificationTitle());
        t("String pkg == twice", () -> String.class.getPackage() == String.class.getPackage());
        t("String pkg == Package.getPackage", () -> String.class.getPackage() == Package.getPackage("java.lang"));
        t("sql pkg == defined", () -> java.sql.Connection.class.getPackage() == ClassLoader.getPlatformClassLoader().getDefinedPackage("java.sql"));
        t("self pkg ==", () -> Sbr2W9ModloadPkg.class.getPackage() == Sbr2W9ModloadPkg.class.getPackage());
        t("self pkg impl version", () -> Sbr2W9ModloadPkg.class.getPackage().getImplementationVersion());
        t("array pkg", () -> ca.arrayType().getPackage());
        t("int pkg", () -> int.class.getPackage());
        // a user loader defining nothing: its own package object
        ClassLoader cl = new java.net.URLClassLoader("pk", new java.net.URL[] {
                new java.io.File(a.length > 0 ? a[0] : "pkg.jar").toURI().toURL() }, null);
        t("custom loader pkg impl version", () -> cl.loadClass("com.sbr2w9pkg.A").getPackage().getImplementationVersion());
        t("custom loader pkg != app", () -> cl.loadClass("com.sbr2w9pkg.A").getPackage() != pa);
        t("custom loader pkg == defined", () -> cl.loadClass("com.sbr2w9pkg.A").getPackage() == cl.getDefinedPackage("com.sbr2w9pkg"));
    }
}
