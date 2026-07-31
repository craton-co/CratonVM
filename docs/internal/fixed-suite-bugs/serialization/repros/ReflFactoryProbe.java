import java.io.*;
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;

/**
 * Differential probe for the `experimental-serialization`-gated
 * `register_reflection_factory_serialization` registrar.
 *
 * Every method it overrides is ordinary java.base / jdk.unsupported *bytecode*
 * in a real JDK, so a default (feature-off) CratonVM build must be able to run
 * it as-is. Anything that diverges from HotSpot here is a real gap the gate is
 * hiding; anything identical proves the overrides are not needed on the
 * real-JDK path.
 *
 * Covers all three groups the registrar touches:
 *   1. the *internal* path — plain ObjectInputStream.readObject(), which goes
 *      through ObjectStreamClass -> ReflectionFactory.newConstructorForSerialization
 *   2. the *direct* path JBoss Marshalling uses — sun.reflect.ReflectionFactory
 *      (jdk.unsupported) called by hand
 *   3. the private-hook accessors — readObjectForSerialization,
 *      writeObjectForSerialization, readResolveForSerialization,
 *      writeReplaceForSerialization, hasStaticInitializerForSerialization,
 *      newConstructorForExternalization
 */
public class ReflFactoryProbe {

    // --- fixtures -------------------------------------------------------

    /** Serializable, and deliberately WITHOUT a no-arg constructor, so
     *  deserialization can only work through newConstructorForSerialization. */
    static class NoNoArg implements Serializable {
        private static final long serialVersionUID = 1L;
        int x;
        String s;
        NoNoArg(int x, String s) { this.x = x; this.s = s; }
        public String toString() { return "NoNoArg(" + x + "," + s + ")"; }
    }

    /** Exercises the private serialization hooks + readResolve/writeReplace. */
    static class WithHooks implements Serializable {
        private static final long serialVersionUID = 2L;
        static { /* forces a static initializer to exist */ }
        int v = 7;
        private void writeObject(ObjectOutputStream out) throws IOException {
            out.defaultWriteObject();
            out.writeInt(99);
        }
        private void readObject(ObjectInputStream in)
                throws IOException, ClassNotFoundException {
            in.defaultReadObject();
            int tag = in.readInt();
            if (tag != 99) throw new IOException("bad tag " + tag);
        }
        private void readObjectNoData() throws ObjectStreamException { }
        private Object readResolve() throws ObjectStreamException { return this; }
        private Object writeReplace() throws ObjectStreamException { return this; }
        public String toString() { return "WithHooks(" + v + ")"; }
    }

    static class Ext implements Externalizable {
        private static final long serialVersionUID = 3L;
        int e;
        public Ext() { }
        public Ext(int e) { this.e = e; }
        public void writeExternal(ObjectOutput out) throws IOException { out.writeInt(e); }
        public void readExternal(ObjectInput in) throws IOException { e = in.readInt(); }
        public String toString() { return "Ext(" + e + ")"; }
    }

    // --- helpers --------------------------------------------------------

    static byte[] ser(Object o) throws Exception {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(b)) { out.writeObject(o); }
        return b.toByteArray();
    }

    static Object deser(byte[] b) throws Exception {
        try (ObjectInputStream in =
                 new ObjectInputStream(new ByteArrayInputStream(b))) {
            return in.readObject();
        }
    }

    static void step(String name, Callable body) {
        try {
            System.out.println(name + " = " + body.call());
        } catch (Throwable t) {
            System.out.println(name + " !! " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    interface Callable { Object call() throws Exception; }

    // --- probes ---------------------------------------------------------

    public static void main(String[] args) throws Exception {

        // 1. internal path: readObject() on a class with no no-arg ctor.
        step("1a.roundtrip.NoNoArg", () -> deser(ser(new NoNoArg(42, "hi"))));
        step("1b.roundtrip.WithHooks", () -> deser(ser(new WithHooks())));
        step("1c.roundtrip.Externalizable", () -> deser(ser(new Ext(5))));

        // 2. direct path (the JBoss Marshalling pattern), via the legacy
        //    jdk.unsupported shim.
        step("2a.sun.getReflectionFactory", () ->
            sun.reflect.ReflectionFactory.getReflectionFactory().getClass().getName());

        step("2b.sun.newConstructorForSerialization", () -> {
            Constructor<?> objCtor = Object.class.getDeclaredConstructor();
            Constructor<?> c = sun.reflect.ReflectionFactory.getReflectionFactory()
                    .newConstructorForSerialization(NoNoArg.class, objCtor);
            c.setAccessible(true);
            Object o = c.newInstance();
            return o.getClass().getName() + " x=" + ((NoNoArg) o).x
                   + " s=" + ((NoNoArg) o).s;
        });

        step("2c.sun.newConstructorForSerialization.1arg", () -> {
            // The 1-arg overload the JDK added for frameworks that don't want
            // to look up Object's ctor themselves.
            Method m = sun.reflect.ReflectionFactory.class
                    .getMethod("newConstructorForSerialization", Class.class);
            Constructor<?> c = (Constructor<?>) m.invoke(
                    sun.reflect.ReflectionFactory.getReflectionFactory(),
                    NoNoArg.class);
            c.setAccessible(true);
            Object o = c.newInstance();
            return o.getClass().getName() + " x=" + ((NoNoArg) o).x;
        });

        step("2d.sun.newConstructorForExternalization", () -> {
            Constructor<?> c = sun.reflect.ReflectionFactory.getReflectionFactory()
                    .newConstructorForExternalization(Ext.class);
            c.setAccessible(true);
            Object o = c.newInstance();
            return o.getClass().getName() + " e=" + ((Ext) o).e;
        });

        // 3. private-hook accessors. These live on the internal factory in
        //    JDK 9+; reach them reflectively so the probe compiles without
        //    --add-exports.
        Class<?> jdkRF = Class.forName("jdk.internal.reflect.ReflectionFactory");
        Method getRF = jdkRF.getMethod("getReflectionFactory");
        Object rf = getRF.invoke(null);

        step("3a.jdk.getReflectionFactory", () -> rf.getClass().getName());

        for (String hook : new String[] {
                "readObjectForSerialization",
                "readObjectNoDataForSerialization",
                "writeObjectForSerialization",
                "readResolveForSerialization",
                "writeReplaceForSerialization" }) {
            step("3b." + hook, () -> {
                Method m = jdkRF.getMethod(hook, Class.class);
                Object mh = m.invoke(rf, WithHooks.class);
                return mh == null ? "null" : "nonnull";
            });
        }

        step("3c.hasStaticInitializerForSerialization", () -> {
            Method m = jdkRF.getMethod(
                    "hasStaticInitializerForSerialization", Class.class);
            return m.invoke(rf, WithHooks.class);
        });

        // A hook-free class must report null for every hook — proves the
        // accessors actually discriminate rather than always answering.
        step("3d.hooks.on.NoNoArg", () -> {
            Method m = jdkRF.getMethod("readObjectForSerialization", Class.class);
            Object mh = m.invoke(rf, NoNoArg.class);
            return mh == null ? "null" : "nonnull";
        });

        System.out.println("PROBE-END");
    }
}
