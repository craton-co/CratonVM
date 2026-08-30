// The same question `InvokeCastSweep` asks of `MethodHandle`, asked of the
// OTHER reflective doors: when the argument does not match the declared
// parameter, does this VM refuse, and does it refuse with the type the JDK
// specifies?
//
// The `MethodHandle` half found that a wrong reference was passed through
// untouched and surfaced as `NoSuchMethodError` from inside the callee -- an
// `Error`, three frames from the call that was actually wrong. These doors have
// their own spelling of the same rule and there is no reason to assume they
// share an implementation:
//
//   MethodHandle.invoke / bindTo        ClassCastException
//   Method.invoke                       IllegalArgumentException
//   Field.set / Field.get               IllegalArgumentException
//   Constructor.newInstance             IllegalArgumentException
//   Array.set                           IllegalArgumentException / ArrayStoreException
//   VarHandle.set                       ClassCastException / WrongMethodTypeException
//
// Four different exception types for one concept, which is exactly the shape a
// VM gets wrong in one place and right in another.
//
// Rows print an exception CLASS NAME or a value -- never a message.
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Array;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

public class ReflectArgTypeSweep {
    interface Body { Object run() throws Throwable; }

    static void t(String tag, Body b) {
        String v;
        try { v = String.valueOf(b.run()); }
        catch (Throwable e) { v = "throws " + e.getClass().getName(); }
        System.out.println(tag + " = " + v);
    }

    public interface Named { String name(); }

    public static class Box implements Named {
        public String s = "s";
        public int i = 7;
        public Object o = "o";
        public static String stat = "st";
        public final String fin = "f";

        public Box() {}
        public Box(String s) { this.s = s; }
        public Box(int i) { this.i = i; }

        public String name() { return "box"; }
        public String take(String x) { return "S:" + x; }
        public String takeInt(int x) { return "I:" + x; }
        public String takeNamed(Named x) { return "N:" + x.name(); }
        public String takeObject(Object x) { return "O:" + x; }
        public static String takeStatic(String x) { return "T:" + x; }
    }

    public static class SubBox extends Box {
        @Override public String name() { return "sub"; }
    }

    public static void main(String[] a) throws Throwable {
        methods();
        vmMintedCarriers();
        fields();
        constructors();
        arrays();
        varHandles();
        System.out.println("DONE");
    }

    // ---- reflective calls whose ARGUMENT is a VM-minted stand-in -----------
    //
    // The population an assignability check gets wrong in the dangerous
    // direction. A `cratonvm.internal.foreign.MemorySegmentImpl` really is a
    // `java.lang.foreign.MemorySegment` -- the relationship lives in the
    // interpreter's `synthetic_implements` table, because the VM mints the
    // class -- but it DECLARES no interfaces, so every hierarchy walk and every
    // by-name walk answers false about it.
    //
    // These rows exist because adding an interface judgement to
    // `Method.invoke` without asking that table refused
    // `Linker.downcallHandle(MemorySegment, FunctionDescriptor, Option[])` with
    // `argument type mismatch`, and the only thing that caught it was re-running
    // `probes/P1RemainingSweep.java`. A refusal of working code is worse than
    // the wrong answer it replaces, so the guard belongs in the same probe as
    // the defect.

    static void vmMintedCarriers() {
        t("k.downcallHandle", () -> {
            Class<?> linkerC = Class.forName("java.lang.foreign.Linker");
            Class<?> fdC = Class.forName("java.lang.foreign.FunctionDescriptor");
            Class<?> vlC = Class.forName("java.lang.foreign.ValueLayout");
            Class<?> memL = Class.forName("java.lang.foreign.MemoryLayout");
            Class<?> lookupC = Class.forName("java.lang.foreign.SymbolLookup");
            Object l = linkerC.getMethod("nativeLinker").invoke(null);
            Object lookup = linkerC.getMethod("defaultLookup").invoke(l);
            Object found = lookupC.getMethod("find", String.class).invoke(lookup, "strlen");
            Object addr = ((java.util.Optional<?>) found).orElse(null);
            if (addr == null) return "NO-SYMBOL";
            Object jlong = vlC.getField("JAVA_LONG").get(null);
            Object jaddr = vlC.getField("ADDRESS").get(null);
            Object argLayouts = Array.newInstance(memL, 1);
            Array.set(argLayouts, 0, jaddr);
            Object fd = fdC.getMethod("of", memL, argLayouts.getClass())
                .invoke(null, jlong, argLayouts);
            Object mh = linkerC.getMethod("downcallHandle",
                    Class.forName("java.lang.foreign.MemorySegment"), fdC,
                    Class.forName("[Ljava.lang.foreign.Linker$Option;"))
                .invoke(l, addr, fd, Array.newInstance(
                    Class.forName("java.lang.foreign.Linker$Option"), 0));
            // The TYPE, not the implementation class: a MethodHandle's own
            // class is unspecified and HotSpot answers a Species name.
            return ((java.lang.invoke.MethodHandle) mh).type().toString();
        });
        // An interface formal reached with an ordinary arena-allocated segment,
        // via a second door.
        t("k.arenaSegmentToInterfaceFormal", () -> {
            Class<?> arenaC = Class.forName("java.lang.foreign.Arena");
            Class<?> segC = Class.forName("java.lang.foreign.MemorySegment");
            Object arena = arenaC.getMethod("ofConfined").invoke(null);
            try {
                Object seg = arenaC.getMethod("allocate", long.class).invoke(arena, 16L);
                Object copy = segC.getMethod("asSlice", long.class, long.class)
                    .invoke(seg, 0L, 8L);
                return segC.getMethod("byteSize").invoke(copy);
            } finally {
                arenaC.getMethod("close").invoke(arena);
            }
        });
    }

    // ---- Method.invoke -----------------------------------------------------

    static void methods() throws Throwable {
        Box b = new Box();
        Method take = Box.class.getMethod("take", String.class);
        Method takeInt = Box.class.getMethod("takeInt", int.class);
        Method takeNamed = Box.class.getMethod("takeNamed", Named.class);
        Method takeObject = Box.class.getMethod("takeObject", Object.class);
        Method stat = Box.class.getMethod("takeStatic", String.class);

        t("m.ok", () -> take.invoke(b, "x"));
        t("m.wrongRef", () -> take.invoke(b, Integer.valueOf(3)));
        t("m.nullRef", () -> take.invoke(b, (Object) null));
        t("m.wrongReceiver", () -> take.invoke("not-a-box", "x"));
        t("m.nullReceiver", () -> take.invoke(null, "x"));
        t("m.subclassReceiver", () -> take.invoke(new SubBox(), "x"));
        t("m.tooFewArgs", () -> take.invoke(b));
        t("m.tooManyArgs", () -> take.invoke(b, "x", "y"));
        t("m.primBoxed", () -> takeInt.invoke(b, Integer.valueOf(3)));
        t("m.primWidened", () -> takeInt.invoke(b, Short.valueOf((short) 3)));
        t("m.primNarrowed", () -> takeInt.invoke(b, Long.valueOf(3L)));
        t("m.primNull", () -> takeInt.invoke(b, (Object) null));
        t("m.primWrongRef", () -> takeInt.invoke(b, "three"));
        t("m.interfaceOk", () -> takeNamed.invoke(b, new SubBox()));
        t("m.interfaceWrong", () -> takeNamed.invoke(b, Integer.valueOf(3)));
        t("m.objectParamAnything", () -> takeObject.invoke(b, Integer.valueOf(3)));
        t("m.staticIgnoresReceiver", () -> stat.invoke(null, "x"));
        t("m.staticWrongRef", () -> stat.invoke(null, Integer.valueOf(3)));
    }

    // ---- Field.set / get ---------------------------------------------------

    static void fields() throws Throwable {
        Box b = new Box();
        Field fs = Box.class.getField("s");
        Field fi = Box.class.getField("i");
        Field fo = Box.class.getField("o");
        Field ffin = Box.class.getField("fin");

        t("f.ok", () -> { fs.set(b, "y"); return b.s; });
        t("f.wrongRef", () -> { fs.set(b, Integer.valueOf(3)); return b.s; });
        t("f.nullRef", () -> { fs.set(b, null); return String.valueOf(b.s); });
        t("f.objectAnything", () -> { fo.set(b, Integer.valueOf(3)); return b.o; });
        t("f.primOk", () -> { fi.set(b, Integer.valueOf(9)); return b.i; });
        t("f.primWidened", () -> { fi.set(b, Short.valueOf((short) 9)); return b.i; });
        t("f.primNarrowed", () -> { fi.set(b, Long.valueOf(9L)); return b.i; });
        t("f.primNull", () -> { fi.set(b, null); return b.i; });
        t("f.primWrongRef", () -> { fi.set(b, "nine"); return b.i; });
        t("f.wrongReceiver", () -> { fs.set("not-a-box", "y"); return "no-throw"; });
        t("f.nullReceiver", () -> { fs.set(null, "y"); return "no-throw"; });
        t("f.finalNoAccessible", () -> { ffin.set(b, "z"); return "no-throw"; });
        t("f.getWrongReceiver", () -> fs.get("not-a-box"));
        t("f.getNullReceiver", () -> fs.get(null));
        t("f.getIntAsObject", () -> fi.get(b));
        t("f.getRefAsInt", () -> fs.getInt(b));
    }

    // ---- Constructor.newInstance -------------------------------------------

    static void constructors() throws Throwable {
        Constructor<Box> cs = Box.class.getConstructor(String.class);
        Constructor<Box> ci = Box.class.getConstructor(int.class);

        t("c.ok", () -> cs.newInstance("q").s);
        t("c.wrongRef", () -> cs.newInstance(Integer.valueOf(3)).s);
        t("c.nullRef", () -> String.valueOf(cs.newInstance((Object) null).s));
        t("c.tooFewArgs", () -> cs.newInstance().s);
        t("c.tooManyArgs", () -> cs.newInstance("q", "r").s);
        t("c.primOk", () -> ci.newInstance(Integer.valueOf(5)).i);
        t("c.primNarrowed", () -> ci.newInstance(Long.valueOf(5L)).i);
        t("c.primNull", () -> ci.newInstance((Object) null).i);
    }

    // ---- Array.set ---------------------------------------------------------

    static void arrays() {
        t("a.ok", () -> { Object arr = Array.newInstance(String.class, 2);
            Array.set(arr, 0, "x"); return Array.get(arr, 0); });
        t("a.wrongRef", () -> { Object arr = Array.newInstance(String.class, 2);
            Array.set(arr, 0, Integer.valueOf(3)); return Array.get(arr, 0); });
        t("a.nullRef", () -> { Object arr = Array.newInstance(String.class, 2);
            Array.set(arr, 0, null); return String.valueOf(Array.get(arr, 0)); });
        t("a.interfaceComponentOk", () -> { Object arr = Array.newInstance(Named.class, 2);
            Array.set(arr, 0, new Box()); return ((Named) Array.get(arr, 0)).name(); });
        t("a.interfaceComponentWrong", () -> { Object arr = Array.newInstance(Named.class, 2);
            Array.set(arr, 0, Integer.valueOf(3)); return "no-throw"; });
        t("a.objectComponentAnything", () -> { Object arr = Array.newInstance(Object.class, 2);
            Array.set(arr, 0, Integer.valueOf(3)); return Array.get(arr, 0); });
        t("a.primWrongRef", () -> { Object arr = Array.newInstance(int.class, 2);
            Array.set(arr, 0, "three"); return "no-throw"; });
        t("a.primNull", () -> { Object arr = Array.newInstance(int.class, 2);
            Array.set(arr, 0, null); return "no-throw"; });
        t("a.notAnArray", () -> { Array.set("not-an-array", 0, "x"); return "no-throw"; });
        t("a.outOfBounds", () -> { Object arr = Array.newInstance(String.class, 2);
            Array.set(arr, 5, "x"); return "no-throw"; });
    }

    // ---- VarHandle ---------------------------------------------------------

    static void varHandles() throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle vs = l.findVarHandle(Box.class, "s", String.class);
        VarHandle vi = l.findVarHandle(Box.class, "i", int.class);
        Box b = new Box();

        t("v.ok", () -> { vs.set(b, "y"); return b.s; });
        t("v.wrongRef", () -> { vs.set(b, Integer.valueOf(3)); return b.s; });
        t("v.nullRef", () -> { vs.set(b, (Object) null); return String.valueOf(b.s); });
        t("v.wrongReceiver", () -> { vs.set("not-a-box", "y"); return "no-throw"; });
        t("v.primOk", () -> { vi.set(b, 9); return b.i; });
        t("v.primWrongRef", () -> { vi.set(b, "nine"); return "no-throw"; });
        t("v.getOk", () -> vs.get(b));
        t("v.getWrongReceiver", () -> vs.get("not-a-box"));
        t("v.casOk", () -> vs.compareAndSet(b, b.s, "z"));
        t("v.casWrongRef", () -> vs.compareAndSet(b, b.s, Integer.valueOf(3)));
    }
}
