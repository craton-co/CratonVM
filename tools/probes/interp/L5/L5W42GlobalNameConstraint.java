// Interpreter round i1, wave 42, lane L5 -- JVMS §5.3.4 loader constraints on
// a JDK-global-looking name (`javax.l5w42.Shared`) that user loaders define
// themselves. The member resolution records the constraint when one side has
// not loaded the name yet, and the later load by the other loader fails with
// `LinkageError` (HotSpot's `SystemDictionary::check_constraints`), exactly as
// for any other name (`L5W37LoaderConstraintPending`).
//
// Before wave 42 CratonVM recorded no constraint for a `javax/`, `jdk/`,
// `sun/` or `com/sun/` name (`vm/src/runtime/resolve/loader_constraints.rs`
// `constrainable`: the user loader's view of such a name was the global
// route's guess), so every row but `both-loaded` printed a value and the
// later load succeeded: a class of loader `a` could then hand its own
// `Shared` to a method of loader `b` compiled against b's `Shared`.
//
// Loader `b` (child-first for `$Api` and `javax.l5w42.Shared`), loader `a`
// (its child; child-first for `$User`, and for `Shared` unless the row says
// otherwise). Every row builds fresh loaders. `javax.l5w42.Shared` is the
// probe's `$Shared` renamed in its class file; `$Api` and `$User` are renamed
// to match.
//
// Rows:
//  accessor-first   `a` defines its own `Shared` (the `new`), `b` has none;
//                   `a` resolves `Api.take2(Shared)`; then `b` loads
//                   `Shared` itself -> LinkageError at b's define.
//  declaring-first  `b` has its own `Shared`, `a` none; `a` resolves
//                   `Api.take2(Shared)` with a null; then `a` loads its own
//                   -> LinkageError at a's define.
//  initiating       `b` has its own `Shared`; `a` resolves `Api.take2`, then
//                   `new Shared()` makes the VM ask `a`, which delegates to a
//                   third loader `c` that defines its own -> LinkageError at
//                   a's initiating load.
//  agree            `a` delegates `Shared` to `b`: one class, no error.
//  both-loaded      both already have their own `Shared`: the resolution
//                   itself fails (checked before wave 42 too).
//  transparent      the accessor is a loader `t` that overrides only
//                   `findClass` (parent: the application loader), so the
//                   VM's global route answers `Shared` for it: the
//                   application loader's copy from the class path (setup).
//                   `b` has its own `Shared`; `t` resolves `b`'s
//                   `Api.take2(Shared)` (its `findClass` hands `l5w42b.Api`
//                   over to `b`), then `new Shared()` -> LinkageError at t's
//                   initiating load.
//
// Setup (once, before the run; writes `javax/l5w42/Shared.class` next to the
// probe's classes, so the application class path has it):
//   javac -d out L5W42GlobalNameConstraint.java
//   java -cp out L5W42GlobalNameConstraint setup
// Run:
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W42GlobalNameConstraint
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] LOADER-CONSTRAINT RECORD javax/l5w42/Shared: ...` (accessor-
// first, declaring-first, initiating, agree, transparent),
// `[ACCESS-DBG] LOADER-CONSTRAINT DENY at define:` (accessor-first,
// declaring-first) and `[ACCESS-DBG] LOADER-CONSTRAINT DENY #<n> at initiating
// load:` (initiating, transparent).
//
// `--compatible` (by design, AGENTS.md: nothing is recorded, a violation is
// only counted), and `dev` before wave 42 under `--jdk-only` (from the code):
//   accessor-first=2 / accessor-first load=loaded
//   declaring-first=1 / declaring-first load=loaded
//   initiating=17, agree=17, transparent=17
//   both-loaded=java.lang.LinkageError under `--jdk-only` (as below, but
//   the type dotted before wave 42: `L5W42ConstraintMessageNames`);
//   both-loaded=2 under `--compatible`.
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   accessor-first=2
//   accessor-first load=java.lang.LinkageError
//   accessor-first msg=loader constraint violation: loader 'b' @H wants to load class javax.l5w42.Shared. A different class with the same name was previously loaded by 'a' @H. (javax.l5w42.Shared is in unnamed module of loader 'a' @H, parent loader 'b' @H)
//   declaring-first=1
//   declaring-first load=java.lang.LinkageError
//   declaring-first msg=loader constraint violation: loader 'a' @H wants to load class javax.l5w42.Shared. A different class with the same name was previously loaded by 'b' @H. (javax.l5w42.Shared is in unnamed module of loader 'b' @H, parent loader 'app')
//   initiating=java.lang.LinkageError
//   initiating msg=loader constraint violation: loader 'a' @H wants to load class javax.l5w42.Shared. A different class with the same name was previously loaded by 'b' @H. (javax.l5w42.Shared is in unnamed module of loader 'b' @H, parent loader 'app')
//   agree=17
//   both-loaded=java.lang.LinkageError
//   both-loaded msg=loader constraint violation: when resolving method 'int L5W42GlobalNameConstraint$Api.take2(javax.l5w42.Shared)' the class loader 'a' @H of the current class, L5W42GlobalNameConstraint$User, and the class loader 'b' @H for the method's defining class, L5W42GlobalNameConstraint$Api, have different Class objects for the type javax/l5w42/Shared used in the signature (L5W42GlobalNameConstraint$User is in unnamed module of loader 'a' @H, parent loader 'b' @H; L5W42GlobalNameConstraint$Api is in unnamed module of loader 'b' @H, parent loader 'app')
//   transparent=java.lang.LinkageError
//   transparent msg=loader constraint violation: loader 't' @H wants to load class javax.l5w42.Shared. A different class with the same name was previously loaded by 'b' @H. (javax.l5w42.Shared is in unnamed module of loader 'b' @H, parent loader 'app')

import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.Set;

public class L5W42GlobalNameConstraint {
    static final String P = "L5W42GlobalNameConstraint$";
    static final String SHARED = "javax.l5w42.Shared";

    /** Internal nested name -> internal renamed name, applied inside every CONSTANT_Utf8. */
    static final Map<String, String> RENAMES = Map.of(
            "L5W42GlobalNameConstraint$Shared", "javax/l5w42/Shared");
    static final Map<String, String> RENAMES_T = Map.of(
            "L5W42GlobalNameConstraint$Shared", "javax/l5w42/Shared",
            "L5W42GlobalNameConstraint$Api", "l5w42b/Api",
            "L5W42GlobalNameConstraint$User", "l5w42t/User");

    /** `bytes` with every substring `from` of every CONSTANT_Utf8 replaced by `to`. */
    static byte[] rename(byte[] bytes, Map<String, String> renames) throws IOException {
        DataInputStream in = new DataInputStream(new java.io.ByteArrayInputStream(bytes));
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buf);
        out.writeInt(in.readInt());
        out.writeShort(in.readUnsignedShort());
        out.writeShort(in.readUnsignedShort());
        int count = in.readUnsignedShort();
        out.writeShort(count);
        for (int i = 1; i < count; i++) {
            int tag = in.readUnsignedByte();
            out.writeByte(tag);
            switch (tag) {
                case 1 -> {
                    String s = in.readUTF();
                    for (Map.Entry<String, String> e : renames.entrySet()) {
                        s = s.replace(e.getKey(), e.getValue());
                    }
                    out.writeUTF(s);
                }
                case 3, 4 -> out.writeInt(in.readInt());
                case 5, 6 -> {
                    out.writeLong(in.readLong());
                    i++;
                }
                case 7, 8, 16, 19, 20 -> out.writeShort(in.readUnsignedShort());
                case 9, 10, 11, 12, 17, 18 -> out.writeInt(in.readInt());
                case 15 -> {
                    out.writeByte(in.readUnsignedByte());
                    out.writeShort(in.readUnsignedShort());
                }
                default -> throw new IOException("constant tag " + tag);
            }
        }
        in.transferTo(out);
        out.flush();
        return buf.toByteArray();
    }

    static byte[] own(String nested) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(nested + ".class")) {
            return in.readAllBytes();
        }
    }

    static byte[] sharedBytes() throws IOException {
        return rename(own(P + "Shared"), RENAMES);
    }

    static final class ChildFirst extends ClassLoader {
        private final Set<String> own;
        private final ClassLoader sharedFrom;

        ChildFirst(String name, ClassLoader parent, Set<String> own, ClassLoader sharedFrom) {
            super(name, parent);
            this.own = own;
            this.sharedFrom = sharedFrom;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (sharedFrom != null && name.equals(SHARED)) {
                    return sharedFrom.loadClass(name);
                }
                if (!own.contains(name)) {
                    return super.loadClass(name, resolve);
                }
                try {
                    byte[] b;
                    if (name.equals(SHARED)) {
                        b = sharedBytes();
                    } else if (name.equals("l5w42b.Api")) {
                        b = rename(own(P + "Api"), RENAMES_T);
                    } else {
                        b = rename(own(name.replace('.', '/')), RENAMES);
                    }
                    return defineClass(name, b, 0, b.length);
                } catch (IOException e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
        }
    }

    /** Overrides `findClass` only: `l5w42t.User` is its own, `l5w42b.Api` is `b`'s. */
    static final class FindOnly extends ClassLoader {
        private final ClassLoader apiFrom;

        FindOnly(String name, ClassLoader parent, ClassLoader apiFrom) {
            super(name, parent);
            this.apiFrom = apiFrom;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.equals("l5w42b.Api")) {
                return apiFrom.loadClass(name);
            }
            if (name.equals("l5w42t.User")) {
                try {
                    byte[] b = rename(own(P + "User"), RENAMES_T);
                    return defineClass(name, b, 0, b.length);
                } catch (IOException e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
            throw new ClassNotFoundException(name);
        }
    }

    static String norm(String s) {
        return s == null ? "null" : s.replaceAll("@[0-9a-f]+", "@H");
    }

    static void run(String label, ClassLoader a, String user, String method) throws Exception {
        Class<?> u = a.loadClass(user);
        try {
            Object r = u.getMethod(method).invoke(null);
            System.out.println(label + "=" + r);
        } catch (java.lang.reflect.InvocationTargetException e) {
            Throwable t = e.getCause();
            System.out.println(label + "=" + t.getClass().getName());
            System.out.println(label + " msg=" + norm(t.getMessage()));
        }
    }

    static void load(String label, ClassLoader l) {
        try {
            l.loadClass(SHARED);
            System.out.println(label + " load=loaded");
        } catch (Throwable t) {
            System.out.println(label + " load=" + t.getClass().getName());
            System.out.println(label + " msg=" + norm(t.getMessage()));
        }
    }

    static ClassLoader app() {
        return L5W42GlobalNameConstraint.class.getClassLoader();
    }

    static ChildFirst b() {
        return new ChildFirst("b", app(), Set.of(P + "Api", "l5w42b.Api", SHARED), null);
    }

    static ChildFirst a(ClassLoader b, boolean ownShared, ClassLoader sharedFrom) {
        return new ChildFirst("a", b,
                ownShared ? Set.of(P + "User", SHARED) : Set.of(P + "User"), sharedFrom);
    }

    public static void main(String[] args) throws Exception {
        Path root = Path.of(L5W42GlobalNameConstraint.class.getProtectionDomain()
                .getCodeSource().getLocation().toURI());
        Path file = root.resolve("javax/l5w42/Shared.class");
        if (args.length > 0 && args[0].equals("setup")) {
            Files.createDirectories(file.getParent());
            Files.write(file, sharedBytes());
            System.out.println("wrote " + file);
            return;
        }
        {
            ChildFirst b = b();
            run("accessor-first", a(b, true, null), P + "User", "takeOwn");
            load("accessor-first", b);
        }
        {
            ChildFirst b = b();
            b.loadClass(SHARED);
            ChildFirst a = a(b, true, null);
            run("declaring-first", a, P + "User", "take2Null");
            load("declaring-first", a);
        }
        {
            ChildFirst b = b();
            b.loadClass(SHARED);
            ChildFirst c = new ChildFirst("c", app(), Set.of(SHARED), null);
            run("initiating", a(b, false, c), P + "User", "take2ThenNew");
        }
        {
            ChildFirst b = b();
            run("agree", a(b, false, b), P + "User", "take2ThenNew");
        }
        {
            ChildFirst b = b();
            ChildFirst a = a(b, true, null);
            b.loadClass(SHARED);
            a.loadClass(SHARED);
            run("both-loaded", a, P + "User", "takeOwn");
        }
        if (!Files.exists(file)) {
            System.out.println("transparent=setup missing");
            return;
        }
        {
            ChildFirst b = b();
            b.loadClass(SHARED);
            FindOnly t = new FindOnly("t", app(), b);
            run("transparent", t, "l5w42t.User", "take2ThenNew");
        }
    }

    public static class Shared {
        public int v = 7;

        public Shared() {
        }
    }

    public static class Api {
        public static int take2(Shared s) {
            return s == null ? 1 : 2;
        }
    }

    public static class User {
        public static int takeOwn() {
            return Api.take2(new Shared());
        }

        public static int take2Null() {
            return Api.take2(null);
        }

        public static int take2ThenNew() {
            int r = Api.take2(null);
            return r * 10 + new Shared().v;
        }
    }
}
