// Interpreter round i1, wave 38, lane L5 -- the `LinkageError` a loader gets
// for defining one name twice (JVMS §5.3.5), with HotSpot's whole message:
// the loader's `nameAndId`, the kind ("class" / "interface") and the
// `(X is in unnamed module of loader ..., parent loader ...)` clause.
// `docs/internal/fixed-bugs/interpreter-L5-duplicate-class-definition-message-lacks-hotspots-loader-text-FIXED-20261002.md`.
//
// Identity hashes are printed as `@H`.
//
// Run (no setup):
//   javac -d out L5W38DuplicateDefineMessage.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W38DuplicateDefineMessage
//
// Before wave 38 (from the code): the exception class matched, the message
// was `loader L5W38DuplicateDefineMessage$L attempted duplicate class
// definition for L5W38DuplicateDefineMessage$A.` for every row (the loader's
// class name, no quotes, no hash, no clause, always "class"). `--compatible`
// keeps that wording by design (its loaders have no `nameAndId`).
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   named=java.lang.LinkageError: loader 'l1' @H attempted duplicate class definition for L5W38DuplicateDefineMessage$A. (L5W38DuplicateDefineMessage$A is in unnamed module of loader 'l1' @H, parent loader 'app')
//   unnamed=java.lang.LinkageError: loader L5W38DuplicateDefineMessage$L @H attempted duplicate class definition for L5W38DuplicateDefineMessage$A. (L5W38DuplicateDefineMessage$A is in unnamed module of loader L5W38DuplicateDefineMessage$L @H, parent loader 'app')
//   bootstrap-parent=java.lang.LinkageError: loader 'l3' @H attempted duplicate class definition for L5W38DuplicateDefineMessage$A. (L5W38DuplicateDefineMessage$A is in unnamed module of loader 'l3' @H, parent loader 'bootstrap')
//   interface=java.lang.LinkageError: loader 'l4' @H attempted duplicate interface definition for L5W38DuplicateDefineMessage$I. (L5W38DuplicateDefineMessage$I is in unnamed module of loader 'l4' @H, parent loader 'app')
//   user-parent=java.lang.LinkageError: loader 'l6' @H attempted duplicate class definition for L5W38DuplicateDefineMessage$A. (L5W38DuplicateDefineMessage$A is in unnamed module of loader 'l6' @H, parent loader 'l5' @H)

import java.io.InputStream;

public class L5W38DuplicateDefineMessage {
    static final String P = "L5W38DuplicateDefineMessage$";

    static final class L extends ClassLoader {
        L(String name, ClassLoader parent) {
            super(name, parent);
        }

        L(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(String name) throws Exception {
            try (InputStream in =
                    ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static void twice(String label, L loader, String name) {
        try {
            loader.define(name);
            loader.define(name);
            System.out.println(label + "=defined twice");
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName() + ": "
                    + String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H"));
        }
    }

    public static void main(String[] args) {
        ClassLoader app = L5W38DuplicateDefineMessage.class.getClassLoader();
        twice("named", new L("l1", app), P + "A");
        twice("unnamed", new L(app), P + "A");
        twice("bootstrap-parent", new L("l3", null), P + "A");
        twice("interface", new L("l4", app), P + "I");
        twice("user-parent", new L("l6", new L("l5", app)), P + "A");
    }

    public static class A {
    }

    public interface I {
    }
}
