// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.Arrays;
import java.util.List;

/**
 * SBR round 5, lane avh: HotSpot-diffed probe of MemoryLayout.arrayElementVarHandle (the
 * IndirectVarHandle the JDK composes over a SegmentVarHandle) and of
 * MethodHandle.invokeWithArguments over the real handles behind it. Every row prints
 * "name -> result | ExceptionClass: message"; diff against HotSpot 25.
 */
public class Sbr5AvhArrayVh {
    interface Row { Object run() throws Throwable; }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            out = v instanceof Object[] a ? Arrays.deepToString(a) : String.valueOf(v);
            if (v != null) out += " (" + v.getClass().getName() + ")";
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + " -> " + out);
    }

    static int add(int a, int b) { return a + b; }
    static void vd(String s) { sink = s; }
    static String sink;
    static String join(String sep, String... parts) { return String.join(sep, parts); }
    static Object boom(int x) throws Exception { throw new java.io.IOException("boom" + x); }

    public static void main(String[] args) throws Throwable {
        Arena arena = Arena.ofConfined();
        MemorySegment seg = arena.allocate(256, 8);
        for (int i = 0; i < 256; i++) seg.set(ValueLayout.JAVA_BYTE, i, (byte) (i * 3 + 1));

        VarHandle vi = ValueLayout.JAVA_INT.arrayElementVarHandle();
        VarHandle vj = ValueLayout.JAVA_LONG.arrayElementVarHandle();
        VarHandle vd = ValueLayout.JAVA_DOUBLE.arrayElementVarHandle();
        VarHandle vs = ValueLayout.JAVA_SHORT.arrayElementVarHandle();
        VarHandle vc = ValueLayout.JAVA_CHAR.arrayElementVarHandle();
        VarHandle vb = ValueLayout.JAVA_BYTE.arrayElementVarHandle();
        VarHandle vf = ValueLayout.JAVA_FLOAT.arrayElementVarHandle();
        VarHandle vz = ValueLayout.JAVA_BOOLEAN.arrayElementVarHandle();
        VarHandle vi2 = MemoryLayout.sequenceLayout(8, ValueLayout.JAVA_INT).arrayElementVarHandle(MemoryLayout.PathElement.sequenceElement());

        row("type.int", () -> vi.varType() + " " + vi.coordinateTypes());
        row("type.int2", () -> vi2.varType() + " " + vi2.coordinateTypes());
        row("get.int", () -> (int) vi.get(seg, 0L, 3L));
        row("get.int.exact", () -> { int x = (int) vi.get(seg, 8L, 5L); return x; });
        row("set.int", () -> { vi.set(seg, 0L, 4L, 12345); return (int) vi.get(seg, 0L, 4L); });
        row("get.long", () -> (long) vj.get(seg, 0L, 2L));
        row("set.long", () -> { vj.set(seg, 0L, 5L, -7L); return (long) vj.get(seg, 0L, 5L); });
        row("get.double", () -> (double) vd.get(seg, 0L, 1L) == (double) vd.get(seg, 0L, 1L));
        row("set.double", () -> { vd.set(seg, 0L, 6L, 2.5); return (double) vd.get(seg, 0L, 6L); });
        row("get.short", () -> (short) vs.get(seg, 0L, 9L));
        row("set.char", () -> { vc.set(seg, 0L, 9L, 'Q'); return (char) vc.get(seg, 0L, 9L); });
        row("set.byte", () -> { vb.set(seg, 0L, 11L, (byte) -3); return (byte) vb.get(seg, 0L, 11L); });
        row("set.float", () -> { vf.set(seg, 0L, 12L, 1.25f); return (float) vf.get(seg, 0L, 12L); });
        row("set.bool", () -> { vz.set(seg, 0L, 13L, true); return (boolean) vz.get(seg, 0L, 13L); });
        row("get.int2.offset", () -> (int) vi2.get(seg, 0L, 1L, 3L));
        row("get.int.offset16", () -> (int) vi.get(seg, 16L, 3L));
        row("getAndAdd.int", () -> { vi.set(seg, 0L, 20L, 10); int a = (int) vi.getAndAdd(seg, 0L, 20L, 5); return a + "," + (int) vi.get(seg, 0L, 20L); });
        row("cas.int", () -> { vi.set(seg, 0L, 21L, 1); boolean a = vi.compareAndSet(seg, 0L, 21L, 1, 2); boolean b = vi.compareAndSet(seg, 0L, 21L, 1, 3); return a + "," + b + "," + (int) vi.get(seg, 0L, 21L); });
        row("getVolatile.long", () -> { vj.setVolatile(seg, 0L, 22L, 99L); return (long) vj.getVolatile(seg, 0L, 22L); });
        row("getAndSet.long", () -> { long o = (long) vj.getAndSet(seg, 0L, 22L, 5L); return o + "," + (long) vj.get(seg, 0L, 22L); });
        row("bitwiseOr.int", () -> { vi.set(seg, 0L, 23L, 5); int o = (int) vi.getAndBitwiseOr(seg, 0L, 23L, 2); return o + "," + (int) vi.get(seg, 0L, 23L); });
        row("oob.hi", () -> (int) vi.get(seg, 0L, 1000L));
        row("oob.neg", () -> (int) vi.get(seg, 0L, -1L));
        row("oob.offset", () -> (int) vi.get(seg, 1000L, 0L));
        row("oob.set", () -> { vi.set(seg, 0L, 1000L, 1); return "no"; });
        row("null.seg", () -> (int) vi.get((MemorySegment) null, 0L, 0L));
        row("wrongidx.int", () -> (int) vi.get(seg, 0L, 3));
        row("wrongidx.str", () -> (int) vi.get(seg, 0L, "x"));
        row("wrongret.long", () -> (long) vi.get(seg, 0L, 3L));
        row("wrongret.obj", () -> (Object) vi.get(seg, 0L, 3L));
        row("nobase", () -> (int) vi.get(seg, 3L));
        row("wrongtype.set", () -> { vi.set(seg, 0L, 0L, "s"); return "no"; });
        row("misaligned", () -> (int) vi.get(seg.asSlice(1), 0L, 0L));
        row("readonly.set", () -> { vi.set(seg.asReadOnly(), 0L, 0L, 1); return "no"; });
        row("readonly.get", () -> (int) vi.get(seg.asReadOnly(), 0L, 7L));
        row("heap.get", () -> { MemorySegment h = MemorySegment.ofArray(new int[8]); vi.set(h, 0L, 2L, 77); return (int) vi.get(h, 0L, 2L); });
        row("closed", () -> { Arena a = Arena.ofConfined(); MemorySegment s = a.allocate(64, 8); a.close(); return (int) vi.get(s, 0L, 0L); });
        row("accessModeType", () -> vi.accessModeType(VarHandle.AccessMode.SET));
        row("accessModeType.cas", () -> vi.accessModeType(VarHandle.AccessMode.COMPARE_AND_SET));
        row("isAccessModeSupported.add", () -> vi.isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD));
        row("isAccessModeSupported.add.double", () -> vd.isAccessModeSupported(VarHandle.AccessMode.GET_AND_BITWISE_OR));
        row("toMethodHandle", () -> vi.toMethodHandle(VarHandle.AccessMode.GET).type());
        row("toMethodHandle.invoke", () -> (int) vi.toMethodHandle(VarHandle.AccessMode.GET).invoke(seg, 0L, 3L));
        row("toMethodHandle.iwa", () -> vi.toMethodHandle(VarHandle.AccessMode.GET).invokeWithArguments(seg, 0L, 3L));
        row("withExact", () -> (int) vi.withInvokeExactBehavior().get(seg, 0L, 3L));
        row("withExact.bad", () -> (int) vi.withInvokeExactBehavior().get(seg, 0L, 3));
        row("sequence.layout", () -> {
            MemoryLayout st = MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("a"), ValueLayout.JAVA_LONG.withName("b")).withName("S");
            VarHandle vb2 = st.arrayElementVarHandle(MemoryLayout.PathElement.groupElement("b"));
            vb2.set(seg, 0L, 2L, 55L);
            return (long) vb2.get(seg, 0L, 2L) + " " + vb2.coordinateTypes();
        });
        row("loop.sum", () -> { long s = 0; for (long i = 0; i < 30; i++) { vi.set(seg, 0L, i, (int) i); } for (long i = 0; i < 30; i++) s += (int) vi.get(seg, 0L, i); return s; });

        // invokeWithArguments over real handles
        MethodHandles.Lookup L = MethodHandles.lookup();
        Class<?> C = Sbr5AvhArrayVh.class;
        MethodHandle ADD = L.findStatic(C, "add", MethodType.methodType(int.class, int.class, int.class));
        MethodHandle JOIN = L.findStatic(C, "join", MethodType.methodType(String.class, String.class, String[].class));
        MethodHandle VD = L.findStatic(C, "vd", MethodType.methodType(void.class, String.class));
        MethodHandle BOOM = L.findStatic(C, "boom", MethodType.methodType(Object.class, int.class));
        MethodHandle LEN = L.findVirtual(String.class, "length", MethodType.methodType(int.class));
        MethodHandle CTOR = L.findConstructor(StringBuilder.class, MethodType.methodType(void.class, String.class));
        MethodHandle GET = L.findStaticGetter(C, "sink", String.class);
        row("iwa.static", () -> ADD.invokeWithArguments(3, 4));
        row("iwa.static.list", () -> ADD.invokeWithArguments(List.of(3, 4)));
        row("iwa.arity.few", () -> ADD.invokeWithArguments(3));
        row("iwa.arity.many", () -> ADD.invokeWithArguments(3, 4, 5));
        row("iwa.badarg", () -> ADD.invokeWithArguments("a", 4));
        row("iwa.null.prim", () -> ADD.invokeWithArguments(null, 4));
        row("iwa.nullarray", () -> VD.invokeWithArguments((Object[]) null));
        row("iwa.nulllist", () -> VD.invokeWithArguments((List<?>) null));
        row("iwa.void", () -> VD.invokeWithArguments("z") + "/" + sink);
        row("iwa.varargs", () -> JOIN.invokeWithArguments("-", "a", "b", "c"));
        row("iwa.varargs.none", () -> JOIN.invokeWithArguments("-"));
        row("iwa.varargs.arr", () -> JOIN.invokeWithArguments("-", new String[] {"x", "y"}));
        row("iwa.virtual", () -> LEN.invokeWithArguments("hello"));
        row("iwa.virtual.null", () -> LEN.invokeWithArguments((Object) null));
        row("iwa.ctor", () -> CTOR.invokeWithArguments("sb").toString());
        row("iwa.getter", () -> GET.invokeWithArguments());
        row("iwa.throws", () -> BOOM.invokeWithArguments(4));
        row("iwa.bound", () -> ADD.bindTo(10).invokeWithArguments(5));
        row("iwa.insert", () -> MethodHandles.insertArguments(ADD, 0, 100).invokeWithArguments(1));
        row("iwa.asType", () -> ADD.asType(MethodType.methodType(Object.class, Object.class, Object.class)).invokeWithArguments(1, 2));
        row("iwa.asType.long", () -> ADD.asType(MethodType.methodType(long.class, int.class, int.class)).invokeWithArguments(1, 2));
        row("iwa.constant", () -> MethodHandles.constant(String.class, "k").invokeWithArguments());
        row("iwa.identity", () -> MethodHandles.identity(int.class).invokeWithArguments(9));
        row("iwa.collector", () -> ADD.asCollector(Object[].class, 0).type());
        row("iwa.spreader", () -> ADD.asSpreader(Object[].class, 2).invokeWithArguments((Object) new Object[] {7, 8}));
        row("iwa.filter", () -> MethodHandles.filterReturnValue(ADD, LEN.asType(MethodType.methodType(int.class, int.class))).type());
        row("iwa.guard", () -> MethodHandles.guardWithTest(
            MethodHandles.constant(boolean.class, true).asType(MethodType.methodType(boolean.class)).asType(MethodType.methodType(boolean.class)),
            MethodHandles.dropArguments(MethodHandles.constant(int.class, 1), 0), MethodHandles.dropArguments(MethodHandles.constant(int.class, 2), 0)).invokeWithArguments());
        row("iwa.array", () -> MethodHandles.arrayElementGetter(int[].class).invokeWithArguments(new int[] {5, 6, 7}, 1));
        row("iwa.array.oob", () -> MethodHandles.arrayElementGetter(int[].class).invokeWithArguments(new int[] {5, 6, 7}, 9));
        row("iwa.modehandle.set", () -> vi.toMethodHandle(VarHandle.AccessMode.SET).invokeWithArguments(seg, 0L, 2L, 9));
        row("iwa.explicit", () -> MethodHandles.explicitCastArguments(ADD, MethodType.methodType(short.class, long.class, long.class)).invokeWithArguments(1L, 2L));
        row("iwa.list.custom", () -> ADD.invokeWithArguments(new java.util.LinkedList<>(List.of(8, 9))));
        arena.close();
    }
}
