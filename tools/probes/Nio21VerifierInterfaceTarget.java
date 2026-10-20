// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// The verifier's link-time loads for an INTERFACE target whose class was not
// loaded when the verified class was defined
// (docs/internal/fixed-bugs/spring-sample-fails-noclassdeffound-groovyobject-on-dev-19dc529626-FIXED-20261008.md).
//
// HotSpot's `VerificationType::is_reference_assignable_from` loads the target
// of a reference assignability check first and answers an interface target
// WITHOUT loading the source. So a class whose never-run method stores an
// optional-dependency type (here `Opt`, whose superinterface `MissingIface`
// is deleted) into an interface-typed field links on HotSpot. The same store
// into a CLASS-typed field loads the source and fails the link.
// Spring Boot's `BeanDefinitionLoader` stores a `GroovyBeanDefinitionReader`
// (implements `groovy.lang.GroovyObject`) into a `BeanDefinitionReader` field.
//
// Rows (conformance only, no timing):
//   iface -- link a class whose never-run method stores an `Opt` into a
//            `Reader` (interface) static: HotSpot `ok`
//   class -- link a class whose never-run method stores an `OptSub` into a
//            `Base` (class) static: HotSpot fails the link with
//            NoClassDefFoundError naming the missing superinterface
//   again -- the iface class still answers after the failure: `ok`
//
// HotSpot 25.0.3 (Temurin, measured 2026-10-08, with and without CDS):
//   iface=ok
//   class=java.lang.NoClassDefFoundError: Nio21VerifierInterfaceTarget$MissingIface
//   again=ok
// CratonVM dev 19dc529626 (expected from the code, both modes): iface= the
// same NoClassDefFoundError (the source load was recorded unconditionally
// because `Reader` was not loaded when `IfaceHolder` was defined).
//
// SETUP: javac -d <dir> Nio21VerifierInterfaceTarget.java, delete
//     <dir>/Nio21VerifierInterfaceTarget$MissingIface.class, then
//     java|cratonvm [--compatible] -cp <dir> Nio21VerifierInterfaceTarget
public class Nio21VerifierInterfaceTarget {
    public interface MissingIface {
    }

    public interface Reader {
    }

    public static class Opt implements Reader, MissingIface {
    }

    public static class Base {
    }

    public static class OptSub extends Base implements MissingIface {
    }

    public static class IfaceHolder {
        static Reader reader;

        static int touch() {
            return 1;
        }

        static void store() {
            reader = new Opt();
        }
    }

    public static class ClassHolder {
        static Base base;

        static int touch() {
            return 1;
        }

        static void store() {
            base = new OptSub();
        }
    }

    static String link(String simple) {
        try {
            Class<?> c = Class.forName("Nio21VerifierInterfaceTarget$" + simple, true,
                    Nio21VerifierInterfaceTarget.class.getClassLoader());
            Object r = c.getDeclaredMethod("touch").invoke(null);
            return Integer.valueOf(1).equals(r) ? "ok" : "bad";
        } catch (java.lang.reflect.InvocationTargetException e) {
            return e.getCause().getClass().getName() + ": " + e.getCause().getMessage();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) {
        System.out.println("iface=" + link("IfaceHolder"));
        System.out.println("class=" + link("ClassHolder"));
        System.out.println("again=" + link("IfaceHolder"));
    }
}
