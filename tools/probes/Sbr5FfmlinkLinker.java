import java.lang.foreign.*;
import java.lang.invoke.*;
import java.util.*;
public class Sbr5FfmlinkLinker {
  static int cmp(MemorySegment a, MemorySegment b) {
    return Integer.compare(a.reinterpret(4).get(ValueLayout.JAVA_INT, 0), b.reinterpret(4).get(ValueLayout.JAVA_INT, 0));
  }
  interface F { Object get() throws Throwable; }
  static String[] SKIP = new String[0];
  static void p(String t, F f) { for (String k : SKIP) if (k.equals(t)) return; String v; try { v = String.valueOf(f.get()); } catch (Throwable e) { v = "THREW " + e.getClass().getName() + ": " + e.getMessage(); } System.out.println(t + " |" + v + "|"); }
  public static void main(String[] a) throws Throwable {
    if (a.length > 0) SKIP = a[0].split(","); Linker l = Linker.nativeLinker(); SymbolLookup d = l.defaultLookup();
    p("abs", () -> (int) l.downcallHandle(d.find("abs").get(), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT)).invokeExact(-7));
    p("strlen", () -> { try (Arena z = Arena.ofConfined()) { return (long) l.downcallHandle(d.find("strlen").get(), FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS)).invokeExact(z.allocateFrom("hello world")); } });
    p("labs.long", () -> (long) l.downcallHandle(d.find("labs").get(), FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG)).invokeExact(-1234567890123L));
    p("sqrt.double", () -> (double) l.downcallHandle(d.find("sqrt").get(), FunctionDescriptor.of(ValueLayout.JAVA_DOUBLE, ValueLayout.JAVA_DOUBLE)).invokeExact(2.0));
    p("floorf.float", () -> (float) l.downcallHandle(d.find("floorf").get(), FunctionDescriptor.of(ValueLayout.JAVA_FLOAT, ValueLayout.JAVA_FLOAT)).invokeExact(2.5f));
    p("getpid>0", () -> (int) l.downcallHandle(d.find("getpid").get(), FunctionDescriptor.of(ValueLayout.JAVA_INT)).invokeExact() > 0);
    StructLayout divt = MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("q"), ValueLayout.JAVA_INT.withName("r"));
    p("div.struct", () -> { try (Arena z = Arena.ofConfined()) { MemorySegment r = (MemorySegment) l.downcallHandle(d.find("div").get(), FunctionDescriptor.of(divt, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT)).invokeExact((SegmentAllocator) z, 17, 5); return r.get(ValueLayout.JAVA_INT, 0) + "," + r.get(ValueLayout.JAVA_INT, 4); } });
    p("snprintf.variadic", () -> { try (Arena z = Arena.ofConfined()) { MemorySegment buf = z.allocate(64); MethodHandle h = l.downcallHandle(d.find("snprintf").get(), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.JAVA_INT, ValueLayout.JAVA_DOUBLE), Linker.Option.firstVariadicArg(3)); int n = (int) h.invokeExact(buf, 64L, z.allocateFrom("%d-%.2f"), 42, 3.14159); return n + " " + buf.getString(0); } });
    p("errno.capture", () -> { try (Arena z = Arena.ofConfined()) { StructLayout cl = Linker.Option.captureStateLayout(); VarHandle ev = cl.varHandle(MemoryLayout.PathElement.groupElement("errno")); MemorySegment st = z.allocate(cl); MethodHandle h = l.downcallHandle(d.find("open").get(), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_INT), Linker.Option.captureCallState("errno")); int fd = (int) h.invokeExact(st, z.allocateFrom("/no/such/file"), 0); return fd + " " + (int) ev.get(st, 0L); } });
    p("qsort.upcall", () -> { try (Arena z = Arena.ofConfined()) {
      MethodHandle q = l.downcallHandle(d.find("qsort").get(), FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
      MethodHandle c = MethodHandles.lookup().findStatic(Sbr5FfmlinkLinker.class, "cmp", MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
      MemorySegment stub = l.upcallStub(c, FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT), ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT)), z);
      MemorySegment arr = z.allocateFrom(ValueLayout.JAVA_INT, 5, 3, 9, 1, 7, 2, 8);
      q.invokeExact(arr, 7L, 4L, stub);
      return Arrays.toString(arr.toArray(ValueLayout.JAVA_INT)); } });
    p("canonical", () -> new TreeMap<>(l.canonicalLayouts()).toString());
    p("variadic.badlayout", () -> l.downcallHandle(d.find("abs").get(), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_BYTE), Linker.Option.firstVariadicArg(0)));
    p("upcall.badtype", () -> { try (Arena z = Arena.ofConfined()) { MethodHandle c = MethodHandles.lookup().findStatic(Sbr5FfmlinkLinker.class, "cmp", MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class)); return l.upcallStub(c, FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.ADDRESS), z); } });
    p("upcall.nullarena", () -> { MethodHandle c = MethodHandles.lookup().findStatic(Sbr5FfmlinkLinker.class, "cmp", MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class)); return l.upcallStub(c, FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS), null); });
    p("downcall.seqlayout", () -> l.downcallHandle(MemorySegment.ofAddress(8), FunctionDescriptor.of(ValueLayout.JAVA_INT, MemoryLayout.sequenceLayout(2, ValueLayout.JAVA_INT))));
    p("downcall.nullfd", () -> l.downcallHandle(MemorySegment.ofAddress(8), null));
    p("downcall.type.struct", () -> l.downcallHandle(FunctionDescriptor.of(divt, ValueLayout.JAVA_LONG)).type());
    p("defaultLookup", () -> l.defaultLookup().getClass().getName());
    p("lookup.strlen", () -> d.find("strlen").isPresent());
    p("downcall.null", () -> l.downcallHandle(null, FunctionDescriptor.ofVoid()));
    p("downcall.nulladdr", () -> l.downcallHandle(MemorySegment.NULL, FunctionDescriptor.ofVoid()));
    p("variadic.bad", () -> l.downcallHandle(MemorySegment.ofAddress(8), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT), Linker.Option.firstVariadicArg(5)));
    p("native.same", () -> Linker.nativeLinker() == Linker.nativeLinker());
    p("native.iface", () -> Linker.nativeLinker().getClass().isInterface());
  }
}
