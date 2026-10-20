// Interpreter round i1, wave 41, lane L5 -- `Class.forName(name, init,
// loader)` with a user-defined loader asks the VM's record of that loader
// FIRST: HotSpot's `SystemDictionary::resolve_instance_class_or_null` finds a
// class the loader defined OR initiated (a previous VM-initiated request that
// it answered, JVMS §5.3) in the loader's dictionary and returns it without
// calling `loadClass` again. A direct `loader.loadClass(...)` call from Java
// is not a VM-initiated load and records nothing.
//
// Rows (a fresh counting loader each; `calls` is how often its `loadClass`
// saw the name):
//   own          the loader defines `$T` itself; `forName` three times
//   delegated    the loader delegates `$T2` to its parent; `forName` twice
//   array        `forName("[L...$T2;")` twice (the element's record)
//   resolved     a class of the loader resolves `new $T2` (the VM's request),
//                then `forName` once
//   direct       `loader.loadClass("...$T2")` from Java, then `forName` once
//
// Before wave 41 CratonVM (`--jdk-only`, from the code: `lang_class.rs`
// `native_class_for_name` read only the loader's own definitions before
// calling `loadClass`) printed `delegated calls=2`, `array calls=2`,
// `resolved calls=2`; `own` and `direct` matched. `--compatible` keeps that by
// design (it records no initiating loads).
//
// Run (no setup):
//   javac -d out L5W41ForNameInitiatedRecord.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W41ForNameInitiatedRecord
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   own calls=1
//   delegated calls=1
//   array calls=1
//   resolved calls=1
//   direct calls=2

import java.io.IOException;
import java.io.InputStream;

public class L5W41ForNameInitiatedRecord {
    static final String P = "L5W41ForNameInitiatedRecord$";

    static final class L extends ClassLoader {
        final String own;
        int calls;

        L(String own) {
            super(L5W41ForNameInitiatedRecord.class.getClassLoader());
            this.own = own;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(P + "T") || name.equals(P + "T2")) {
                calls++;
            }
            if (!name.equals(own) && !name.equals(P + "Ref")) {
                return super.loadClass(name, resolve);
            }
            Class<?> c = findLoadedClass(name);
            if (c != null) {
                return c;
            }
            try (InputStream in =
                    ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            } catch (IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        L own = new L(P + "T");
        for (int i = 0; i < 3; i++) {
            Class.forName(P + "T", false, own);
        }
        System.out.println("own calls=" + own.calls);

        L delegated = new L("none");
        for (int i = 0; i < 2; i++) {
            Class.forName(P + "T2", false, delegated);
        }
        System.out.println("delegated calls=" + delegated.calls);

        L array = new L("none");
        for (int i = 0; i < 2; i++) {
            Class.forName("[L" + P + "T2;", false, array);
        }
        System.out.println("array calls=" + array.calls);

        L resolved = new L("none");
        resolved.loadClass(P + "Ref").getMethod("make").invoke(null);
        Class.forName(P + "T2", false, resolved);
        System.out.println("resolved calls=" + resolved.calls);

        L direct = new L("none");
        direct.loadClass(P + "T2");
        Class.forName(P + "T2", false, direct);
        System.out.println("direct calls=" + direct.calls);
    }

    public static class T {
    }

    public static class T2 {
    }

    public static class Ref {
        public static Object make() {
            return new T2();
        }
    }
}
