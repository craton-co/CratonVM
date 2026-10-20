// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// The verifier's loads at link time
// (docs/internal/fixed-bugs/i47-verifier-assignability-checks-initiate-no-loads-FIXED-20261007.md).
// HotSpot's split verifier loads both sides of a reference assignability
// check, and each typed catch type with java.lang.Throwable, through the
// verified class's loader (`VerificationType::resolve_and_check_assignability`)
// while LINKING the class; a missing one fails the link with its
// NoClassDefFoundError although the method naming it never runs.
//
// Rows:
//   asked   -- the application names a logging loader's loadClass is asked
//              while a class it defined is linked (Class.forName(.., true,
//              ..) after a load-only forName), sorted. The loader defines
//              only the `Holder` / `Broken` / `Sibling` classes and delegates
//              every other name; no method of `Holder` runs
//   asked-jdk -- the `java.*` names it is asked in the same window. The
//              follow-up Java-name resolution repair now matches HotSpot
//              for this row too
//              (docs/internal/fixed-bugs/i47-user-loaders-are-never-asked-for-java-names-20261007.md)
//   missing -- linking a class whose never-run method returns a `Gone` as a
//              `Sup` (Gone.class deleted): the throwable, twice (a failed
//              link is retried)
//   later   -- an untouched sibling class of the same loader still links
//
// HotSpot 25.0.3 (default flags and -Xint; measured):
//   asked=[I47VerifierLoadsAtLink$Iface, I47VerifierLoadsAtLink$Impl, I47VerifierLoadsAtLink$MyExc, I47VerifierLoadsAtLink$Sup]
//   asked-jdk=[java.lang.Throwable]
//   missing=java.lang.NoClassDefFoundError: I47VerifierLoadsAtLink$Gone
//   missing=java.lang.NoClassDefFoundError: I47VerifierLoadsAtLink$Gone
//   later=ok
// CratonVM before the fix (`--jdk-only`, `--compatible`, `--nojit`):
//   asked=[I47VerifierLoadsAtLink$MyExc] (the catch-type link step only),
//   missing=linked twice; the first verifier repair left asked-jdk=[].
//   After the Java-name follow-up: all HotSpot lines, asked-jdk included.
//
// SETUP: compile, delete I47VerifierLoadsAtLink$Gone.class, then
//     java|cratonvm [--compatible] [--nojit] -cp <dir> I47VerifierLoadsAtLink <dir>
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

public class I47VerifierLoadsAtLink {
    public interface Iface {
    }

    public static class Sup {
    }

    public static class Impl extends Sup implements Iface {
    }

    public static class Gone extends Sup {
    }

    public static class MyExc extends Exception {
    }

    static void thrower() throws MyExc {
    }

    public static class Holder {
        static int touch() {
            return 1;
        }

        static int guarded() {
            try {
                thrower();
                return 1;
            } catch (MyExc e) {
                return -1;
            }
        }

        static Sup up() {
            return new Impl();
        }

        static Iface viaInterface() {
            return new Impl();
        }

        static Sup[] array() {
            return new Impl[0];
        }

        static Object same() {
            return new Impl();
        }
    }

    public static class Broken {
        static int touch() {
            return 2;
        }

        static Sup never() {
            return new Gone();
        }
    }

    public static class Sibling {
        static int touch() {
            return 3;
        }
    }

    static boolean owned(String name) {
        return name.equals("I47VerifierLoadsAtLink$Holder")
                || name.equals("I47VerifierLoadsAtLink$Broken")
                || name.equals("I47VerifierLoadsAtLink$Sibling");
    }

    static final class Logging extends ClassLoader {
        final Path dir;
        final List<String> asked = Collections.synchronizedList(new ArrayList<>());
        volatile boolean logging;

        Logging(Path dir) {
            super(I47VerifierLoadsAtLink.class.getClassLoader());
            this.dir = dir;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (logging) {
                asked.add(name);
            }
            if (!owned(name)) {
                return getParent().loadClass(name);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try {
                        byte[] b = Files.readAllBytes(dir.resolve(name + ".class"));
                        c = defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Path.of(args.length > 0 ? args[0] : ".");
        Logging loader = new Logging(dir);
        String holder = "I47VerifierLoadsAtLink$Holder";
        Class.forName(holder, false, loader);
        loader.logging = true;
        Class.forName(holder, true, loader);
        loader.logging = false;
        List<String> app = new ArrayList<>();
        List<String> jdk = new ArrayList<>();
        for (String name : loader.asked) {
            if (name.equals(holder)) {
                continue;
            }
            List<String> into = name.startsWith("java.") ? jdk : app;
            if (!into.contains(name)) {
                into.add(name);
            }
        }
        // HotSpot verifies a class's methods in its sorted method array
        // (by name symbol address), not in class-file order: compare sets.
        Collections.sort(app);
        System.out.println("asked=" + app);
        System.out.println("asked-jdk=" + jdk);

        Logging second = new Logging(dir);
        for (int i = 0; i < 2; i++) {
            try {
                Class.forName("I47VerifierLoadsAtLink$Broken", true, second);
                System.out.println("missing=linked");
            } catch (Throwable t) {
                System.out.println("missing=" + t);
            }
        }
        try {
            Class.forName("I47VerifierLoadsAtLink$Sibling", true, second);
            System.out.println("later=ok");
        } catch (Throwable t) {
            System.out.println("later=" + t);
        }
    }
}
