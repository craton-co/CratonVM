package app;

/**
 * Interpreter round i4 wave 99, lane L5:
 * docs/internal/fixed-bugs/interpreter-i98-orch-module-clauses-omit-version-of-named-application-modules-FIXED-20261016.md.
 * HotSpot's ClassCastException and IllegalAccessError texts name a module as
 * `module <name>@<version>` (Klass::class_in_module_of_loader under
 * ModuleEntry::should_show_version): a module-path module with a version
 * shows it, an automatic module's version comes from its jar's file name
 * (lib-3.4.jar -> lib@3.4), an unversioned module and the JDK's own modules
 * show none. IllegalAccessException texts (Module.toString) never do.
 *
 * Setup and run (from tools/probes/interp/L5/L5W99ModuleVersionClauses):
 *
 *   sh setup.sh                      (builds out/L5W99MV/mp: app.jar, lib-3.4.jar, unv.jar)
 *   java --module-path out/L5W99MV/mp -m app/app.Main
 *   cratonvm --java-home <jdk25> [--nojit] [--compatible] --module-path out/L5W99MV/mp -m app/app.Main
 *
 * app.jar is jarred with `--module-version 1.2`; lib-3.4.jar has no
 * module-info (an automatic module); unv.jar is a modular jar with no version.
 * lib.Hidden is public when app is compiled and package-private in the jar,
 * so `new lib.Hidden()` fails with IllegalAccessError at resolution.
 *
 * Expected HotSpot 25.0.3 output (compare verbatim):
 *   cce-lib=class lib.Thing cannot be cast to class app.Box (lib.Thing is in module lib@3.4 of loader 'app'; app.Box is in module app@1.2 of loader 'app')
 *   cce-unv=class unv.U cannot be cast to class app.Box (unv.U is in module unv of loader 'app'; app.Box is in module app@1.2 of loader 'app')
 *   cce-base=class app.Box cannot be cast to class java.lang.String (app.Box is in module app@1.2 of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
 *   cce-same=class app.Box cannot be cast to class app.Other (app.Box and app.Other are in module app@1.2 of loader 'app')
 *   iae=java.lang.IllegalAccessError: failed to access class lib.Hidden from class app.Main (lib.Hidden is in module lib@3.4 of loader 'app'; app.Main is in module app@1.2 of loader 'app')
 *   dup=java.lang.LinkageError: loader 'app' attempted duplicate class definition for app.Box. (app.Box is in module app@1.2 of loader 'app')
 *   iaex=java.lang.IllegalAccessException: class app.Main (in module app) cannot access class unv.hidden.H (in module unv) because module unv does not export unv.hidden to module app
 */
public class Main {
    static Object hidden() {
        return new lib.Hidden();
    }

    public static void main(String[] args) throws Exception {
        Object thing = new lib.Thing();
        try {
            Box b = (Box) thing;
            System.out.println("cce-lib=no exception " + b);
        } catch (ClassCastException e) {
            System.out.println("cce-lib=" + e.getMessage());
        }
        Object u = new unv.U();
        try {
            Box b = (Box) u;
            System.out.println("cce-unv=no exception " + b);
        } catch (ClassCastException e) {
            System.out.println("cce-unv=" + e.getMessage());
        }
        Object box = new Box();
        try {
            String s = (String) box;
            System.out.println("cce-base=no exception " + s);
        } catch (ClassCastException e) {
            System.out.println("cce-base=" + e.getMessage());
        }
        try {
            Other o = (Other) box;
            System.out.println("cce-same=no exception " + o);
        } catch (ClassCastException e) {
            System.out.println("cce-same=" + e.getMessage());
        }
        try {
            System.out.println("iae=no error " + hidden());
        } catch (IllegalAccessError e) {
            System.out.println("iae=" + e);
        }
        // Wave 99 follow-up: the duplicate-definition LinkageError
        // (SystemDictionary::check_constraints, class_in_module_of_loader).
        try (java.io.InputStream in = Box.class.getResourceAsStream("Box.class")) {
            byte[] bytes = in.readAllBytes();
            Class<?> again = java.lang.invoke.MethodHandles.lookup().defineClass(bytes);
            System.out.println("dup=no error " + again);
        } catch (LinkageError e) {
            System.out.println("dup=" + e);
        }
        try {
            Class<?> h = Class.forName("unv.hidden.H");
            System.out.println("iaex=no exception " + h.getConstructor().newInstance());
        } catch (IllegalAccessException e) {
            System.out.println("iaex=" + e);
        }
    }
}
