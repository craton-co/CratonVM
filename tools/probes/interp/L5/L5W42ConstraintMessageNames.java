// Interpreter round i1, wave 42, lane L5 -- the JVMS §5.3.4 violation
// messages for a type in a named package (`q.Shared`, the probe's `$Shared`
// renamed in its class file). Earlier probes used nested classes of the
// unnamed package, whose internal and external names agree, so one
// difference never showed: HotSpot prints the method-resolution message's
// type in INTERNAL form ("for the type q/Shared used in the signature"),
// while the field, override and itable messages print it dotted.
//
// Every row: loaders `b` (child-first for the declaring class and
// `q.Shared`) and `a` (its child; child-first for the using class and
// `q.Shared`); both load their own `q.Shared` first, so the resolution or
// the link itself fails.
//
// CratonVM before wave 42 (`--jdk-only`, from the code:
// `vm/src/runtime/resolve/loader_constraints.rs` `resolution_message`): the
// `method` row printed `... for the type q.Shared used in the signature ...`;
// the other rows as HotSpot.
// `--compatible` (by design: a violation is only counted): `method=1`,
// `field=1`, `array-field=1`, `override=linked`, `itable=linked`.
//
// Run (no setup):
//   javac -d out L5W42ConstraintMessageNames.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W42ConstraintMessageNames
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   method=java.lang.LinkageError
//   method msg=loader constraint violation: when resolving method 'int L5W42ConstraintMessageNames$Api.take(q.Shared)' the class loader 'a' @H of the current class, L5W42ConstraintMessageNames$User, and the class loader 'b' @H for the method's defining class, L5W42ConstraintMessageNames$Api, have different Class objects for the type q/Shared used in the signature (L5W42ConstraintMessageNames$User is in unnamed module of loader 'a' @H, parent loader 'b' @H; L5W42ConstraintMessageNames$Api is in unnamed module of loader 'b' @H, parent loader 'app')
//   field=java.lang.LinkageError
//   field msg=loader constraint violation: when resolving field "held" of type q.Shared, the class loader 'a' @H of the current class, L5W42ConstraintMessageNames$User, and the class loader 'b' @H for the field's defining class, L5W42ConstraintMessageNames$Api, have different Class objects for type q.Shared (L5W42ConstraintMessageNames$User is in unnamed module of loader 'a' @H, parent loader 'b' @H; L5W42ConstraintMessageNames$Api is in unnamed module of loader 'b' @H, parent loader 'app')
//   array-field=java.lang.LinkageError
//   array-field msg=loader constraint violation: when resolving field "arr" of type [Lq.Shared;, the class loader 'a' @H of the current class, L5W42ConstraintMessageNames$User, and the class loader 'b' @H for the field's defining class, L5W42ConstraintMessageNames$Api, have different Class objects for type [Lq.Shared; (L5W42ConstraintMessageNames$User is in unnamed module of loader 'a' @H, parent loader 'b' @H; L5W42ConstraintMessageNames$Api is in unnamed module of loader 'b' @H, parent loader 'app')
//   override=java.lang.LinkageError
//   override msg=loader constraint violation for class L5W42ConstraintMessageNames$Sub: when selecting overriding method 'int L5W42ConstraintMessageNames$Sub.m(q.Shared)' the class loader 'a' @H of the selected method's type L5W42ConstraintMessageNames$Sub, and the class loader 'b' @H for its super type L5W42ConstraintMessageNames$Base have different Class objects for the type q.Shared used in the signature (L5W42ConstraintMessageNames$Sub is in unnamed module of loader 'a' @H, parent loader 'b' @H; L5W42ConstraintMessageNames$Base is in unnamed module of loader 'b' @H, parent loader 'app')
//   itable=java.lang.LinkageError
//   itable msg=loader constraint violation in interface itable initialization for class L5W42ConstraintMessageNames$Impl: when selecting method 'int L5W42ConstraintMessageNames$Iface.m(q.Shared)' the class loader 'b' @H for super interface L5W42ConstraintMessageNames$Iface, and the class loader 'a' @H of the selected method's class, L5W42ConstraintMessageNames$Impl have different Class objects for the type q.Shared used in the signature (L5W42ConstraintMessageNames$Iface is in unnamed module of loader 'b' @H, parent loader 'app'; L5W42ConstraintMessageNames$Impl is in unnamed module of loader 'a' @H, parent loader 'b' @H)

import java.io.*;
import java.util.*;

public class L5W42ConstraintMessageNames {
    static final String P = "L5W42ConstraintMessageNames$";
    static final Map<String, String> RENAMES = Map.of("L5W42ConstraintMessageNames$Shared", "q/Shared");

    static byte[] rename(byte[] bytes, Map<String, String> renames) throws IOException {
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(bytes));
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
                    for (Map.Entry<String, String> e : renames.entrySet()) s = s.replace(e.getKey(), e.getValue());
                    out.writeUTF(s);
                }
                case 3, 4 -> out.writeInt(in.readInt());
                case 5, 6 -> { out.writeLong(in.readLong()); i++; }
                case 7, 8, 16, 19, 20 -> out.writeShort(in.readUnsignedShort());
                case 9, 10, 11, 12, 17, 18 -> out.writeInt(in.readInt());
                case 15 -> { out.writeByte(in.readUnsignedByte()); out.writeShort(in.readUnsignedShort()); }
                default -> throw new IOException("tag " + tag);
            }
        }
        in.transferTo(out);
        out.flush();
        return buf.toByteArray();
    }

    static byte[] own(String n) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(n + ".class")) { return in.readAllBytes(); }
    }

    static final class CF extends ClassLoader {
        final Set<String> own;
        CF(String name, ClassLoader parent, Set<String> own) { super(name, parent); this.own = own; }
        @Override protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) return c;
                if (!own.contains(name)) return super.loadClass(name, resolve);
                try {
                    String src = name.equals("q.Shared") ? P + "Shared" : name.replace('.', '/');
                    byte[] b = rename(own(src), RENAMES);
                    return defineClass(name, b, 0, b.length);
                } catch (IOException e) { throw new ClassNotFoundException(name, e); }
            }
        }
    }

    static String norm(String s) { return s == null ? "null" : s.replaceAll("@[0-9a-f]+", "@H"); }

    static void row(String label, Set<String> bOwn, Set<String> aOwn, String cls, String method) throws Exception {
        CF b = new CF("b", L5W42ConstraintMessageNames.class.getClassLoader(), bOwn);
        CF a = new CF("a", b, aOwn);
        b.loadClass("q.Shared");
        a.loadClass("q.Shared");
        try {
            Class<?> u = a.loadClass(cls);
            Object r = method == null ? "linked" : u.getMethod(method).invoke(null);
            if (method == null) { u.getMethods(); u.getDeclaredConstructor().newInstance(); }
            System.out.println(label + "=" + r);
        } catch (Throwable t) {
            if (t instanceof java.lang.reflect.InvocationTargetException) t = t.getCause();
            System.out.println(label + "=" + t.getClass().getName());
            System.out.println(label + " msg=" + norm(t.getMessage()));
        }
    }

    public static void main(String[] args) throws Exception {
        row("method", Set.of(P + "Api", "q.Shared"), Set.of(P + "User", "q.Shared"), P + "User", "take");
        row("field", Set.of(P + "Api", "q.Shared"), Set.of(P + "User", "q.Shared"), P + "User", "field");
        row("array-field", Set.of(P + "Api", "q.Shared"), Set.of(P + "User", "q.Shared"), P + "User", "arrayField");
        row("override", Set.of(P + "Base", "q.Shared"), Set.of(P + "Sub", "q.Shared"), P + "Sub", null);
        row("itable", Set.of(P + "Iface", "q.Shared"), Set.of(P + "Impl", "q.Shared"), P + "Impl", null);
    }

    public static class Shared { public int v = 7; public Shared() {} }
    public static class Api {
        public static Shared held;
        public static Shared[] arr;
        public static int take(Shared s) { return 1; }
    }
    public static class User {
        public static int take() { return Api.take(null); }
        public static int field() { return Api.held == null ? 1 : 2; }
        public static int arrayField() { return Api.arr == null ? 1 : 2; }
    }
    public static class Base { public int m(Shared s) { return 1; } }
    public static class Sub extends Base { public Sub() {} public int m(Shared s) { return 2; } }
    public interface Iface { int m(Shared s); }
    public static class Impl implements Iface { public Impl() {} public int m(Shared s) { return 2; } }
}
