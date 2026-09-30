// Interpreter round i1, wave 37, lane L5 -- `defineClass` with bytes whose
// `this_class` is not the requested name: JVMS §5.3.5 makes it a
// `NoClassDefFoundError`, and HotSpot's message (ClassFileParser) is the
// requested name in internal form, then the class file's:
// `p/A (wrong name: p/B)`. Frameworks match that text (a loader that probes a
// case-insensitive file system, a class scanner reporting the mismatch).
//
// CratonVM before wave 37 (both modes, from the code): the same
// `NoClassDefFoundError` with the message
// `L5W37DefineWrongName$A (defineClass requested name L5W37DefineWrongName$A
// but class file declares L5W37DefineWrongName$B)`
// (`ClassManager::define_class_shared_with_options`). The two `IllegalName`
// rows are the JDK's own `ClassLoader.preDefineClass` (Java), printed for
// reference; the `duplicate` row is the class name only.
//
// Run (no setup):
//   javac -d out L5W37DefineWrongName.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W37DefineWrongName
//
// Expected HotSpot 25.0.3 output (compare verbatim; the two wrong-name rows
// are the fix and apply to both CratonVM modes):
//   wrong name=java.lang.NoClassDefFoundError: L5W37DefineWrongName$A (wrong name: L5W37DefineWrongName$B)
//   wrong dotted=java.lang.NoClassDefFoundError: L5W37DefineWrongName/A (wrong name: L5W37DefineWrongName$B)
//   slash name=java.lang.NoClassDefFoundError: IllegalName: L5W37DefineWrongName/B
//   array name=java.lang.NoClassDefFoundError: IllegalName: [LL5W37DefineWrongName$B;
//   duplicate=java.lang.LinkageError

import java.io.IOException;
import java.io.InputStream;

public class L5W37DefineWrongName {
    public static class B {
    }

    static final class L extends ClassLoader {
        L() {
            super(L5W37DefineWrongName.class.getClassLoader());
        }

        Class<?> def(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] bytes(String n) throws IOException {
        try (InputStream in =
                ClassLoader.getSystemResourceAsStream(n.replace('.', '/') + ".class")) {
            return in.readAllBytes();
        }
    }

    static void row(String label, Throwable t) {
        System.out.println(label + "=" + t.getClass().getName() + ": " + t.getMessage());
    }

    public static void main(String[] a) throws Exception {
        byte[] b = bytes("L5W37DefineWrongName$B");
        try {
            new L().def("L5W37DefineWrongName$A", b);
            System.out.println("wrong name=defined");
        } catch (Throwable t) {
            row("wrong name", t);
        }
        try {
            new L().def("L5W37DefineWrongName.A", b);
            System.out.println("wrong dotted=defined");
        } catch (Throwable t) {
            row("wrong dotted", t);
        }
        try {
            new L().def("L5W37DefineWrongName/B", b);
            System.out.println("slash name=defined");
        } catch (Throwable t) {
            row("slash name", t);
        }
        try {
            new L().def("[LL5W37DefineWrongName$B;", b);
            System.out.println("array name=defined");
        } catch (Throwable t) {
            row("array name", t);
        }
        L l = new L();
        l.def("L5W37DefineWrongName$B", b);
        try {
            l.def("L5W37DefineWrongName$B", b);
            System.out.println("duplicate=defined");
        } catch (Throwable t) {
            System.out.println("duplicate=" + t.getClass().getName());
        }
    }
}
