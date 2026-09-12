import java.lang.foreign.*;
import java.lang.foreign.MemoryLayout.PathElement;
import java.nio.ByteOrder;

/**
 * Lane 4 wave 2, the layout half of `jdk/internal/foreign`.
 *
 * Every line is printed through {@link #t}, so a throw is a LINE rather than an
 * abort: a section that stops at the first throw hides every silent defect
 * behind it, which is how the `java.io.File` carrier defect of wave 1 survived
 * thirteen probes.
 *
 * The three `withName` / `withByteAlignment` and two `withOrder` descriptors
 * per carrier are covariant-return bridges. javac picks the descriptor from the
 * STATIC type of the receiver, so each is reached by widening the receiver --
 * `OfInt`, then `ValueLayout`, then `MemoryLayout` -- and not by casting the
 * result.
 */
public class L4FfmLayoutSweep {
    static void p(String tag, Object v) { System.out.println(tag + " |" + v + "|"); }
    static void t(String tag, java.util.concurrent.Callable<Object> c) {
        try { p(tag, c.call()); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    static String cls(Object o) { return o == null ? "null" : o.getClass().getName(); }

    static void carrier(String tag, ValueLayout v) {
        p(tag + ".class", cls(v));
        t(tag + ".toString", () -> v.toString());
        t(tag + ".byteSize", () -> v.byteSize());
        t(tag + ".byteAlignment", () -> v.byteAlignment());
        t(tag + ".order", () -> v.order());
        t(tag + ".name", () -> v.name());

        // withName at the two WIDER static types; the narrow one is done by the
        // caller, which knows the precise carrier type.
        ValueLayout asV = v;
        MemoryLayout asM = v;
        t(tag + ".withName/V.class", () -> cls(asV.withName("nv")));
        t(tag + ".withName/V.name", () -> asV.withName("nv").name());
        t(tag + ".withName/M.class", () -> cls(asM.withName("nm")));
        t(tag + ".withName/M.name", () -> asM.withName("nm").name());
        t(tag + ".withName/V.toString", () -> asV.withName("nv").toString());

        t(tag + ".withOrder/V.class", () -> cls(asV.withOrder(ByteOrder.BIG_ENDIAN)));
        t(tag + ".withOrder/V.order", () -> asV.withOrder(ByteOrder.BIG_ENDIAN).order());
        t(tag + ".withOrder/V.toString", () -> asV.withOrder(ByteOrder.BIG_ENDIAN).toString());
        t(tag + ".withOrder/V.LE", () -> asV.withOrder(ByteOrder.LITTLE_ENDIAN).order());

        t(tag + ".withByteAlignment/V.class", () -> cls(asV.withByteAlignment(1)));
        t(tag + ".withByteAlignment/V.n", () -> asV.withByteAlignment(1).byteAlignment());
        t(tag + ".withByteAlignment/M.class", () -> cls(asM.withByteAlignment(1)));
        t(tag + ".withByteAlignment/M.n", () -> asM.withByteAlignment(1).byteAlignment());
        t(tag + ".withByteAlignment/V.toString", () -> asV.withByteAlignment(1).toString());
        t(tag + ".withByteAlignment/V.bad", () -> asV.withByteAlignment(3).byteAlignment());

        // name() survives a second transform, and the two orders of the same
        // pair of transforms agree.
        t(tag + ".chain.name", () -> asV.withName("k").withByteAlignment(1).name());
        t(tag + ".chain.align", () -> asV.withByteAlignment(1).withName("k").byteAlignment());
        t(tag + ".chain.toString", () -> asV.withName("k").withByteAlignment(1).toString());
        t(tag + ".equals.self", () -> asV.withName("k").equals(asV.withName("k")));
        t(tag + ".hash.self", () -> asV.withName("k").hashCode() == asV.withName("k").hashCode());

        // `carrier()` and `varHandle()` are bucket-B rows -- inherited from
        // `ValueLayouts$AbstractValueLayout` rather than declared -- and nine
        // of each are registered. A VarHandle is read back through a HEAP
        // segment so no address is printed and nothing depends on the native
        // allocator.
        t(tag + ".carrier", () -> asV.carrier().getName());
        t(tag + ".varHandle.varType", () -> asV.varHandle().varType().getName());
        t(tag + ".varHandle.coords", () -> asV.varHandle().coordinateTypes().size());
        // `byteOffset` is registered on the value layouts too -- nine rows of
        // it -- and the empty path is the only one a value layout accepts.
        t(tag + ".byteOffset.empty", () -> asM.byteOffset());
        t(tag + ".byteOffset.bad", () -> asM.byteOffset(PathElement.groupElement("x")));
    }

    public static void main(String[] a) {
        carrier("bool", ValueLayout.JAVA_BOOLEAN);
        t("bool.narrow.withName", () -> cls(ValueLayout.JAVA_BOOLEAN.withName("x")));
        t("bool.narrow.withAlign", () -> cls(ValueLayout.JAVA_BOOLEAN.withByteAlignment(1)));
        t("bool.narrow.withOrder", () -> cls(ValueLayout.JAVA_BOOLEAN.withOrder(ByteOrder.BIG_ENDIAN)));

        carrier("byte", ValueLayout.JAVA_BYTE);
        t("byte.narrow.withName", () -> cls(ValueLayout.JAVA_BYTE.withName("x")));
        t("byte.narrow.withAlign", () -> cls(ValueLayout.JAVA_BYTE.withByteAlignment(1)));
        t("byte.narrow.withOrder", () -> cls(ValueLayout.JAVA_BYTE.withOrder(ByteOrder.BIG_ENDIAN)));

        carrier("char", ValueLayout.JAVA_CHAR);
        t("char.narrow.withName", () -> cls(ValueLayout.JAVA_CHAR.withName("x")));
        t("char.narrow.withAlign", () -> cls(ValueLayout.JAVA_CHAR.withByteAlignment(1)));
        t("char.narrow.withOrder", () -> cls(ValueLayout.JAVA_CHAR.withOrder(ByteOrder.BIG_ENDIAN)));

        carrier("short", ValueLayout.JAVA_SHORT);
        t("short.narrow.withName", () -> cls(ValueLayout.JAVA_SHORT.withName("x")));
        t("short.narrow.withAlign", () -> cls(ValueLayout.JAVA_SHORT.withByteAlignment(1)));
        t("short.narrow.withOrder", () -> cls(ValueLayout.JAVA_SHORT.withOrder(ByteOrder.BIG_ENDIAN)));

        carrier("int", ValueLayout.JAVA_INT);
        t("int.narrow.withName", () -> cls(ValueLayout.JAVA_INT.withName("x")));
        t("int.narrow.withAlign", () -> cls(ValueLayout.JAVA_INT.withByteAlignment(1)));
        t("int.narrow.withOrder", () -> cls(ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN)));

        carrier("long", ValueLayout.JAVA_LONG);
        t("long.narrow.withName", () -> cls(ValueLayout.JAVA_LONG.withName("x")));
        t("long.narrow.withAlign", () -> cls(ValueLayout.JAVA_LONG.withByteAlignment(1)));
        t("long.narrow.withOrder", () -> cls(ValueLayout.JAVA_LONG.withOrder(ByteOrder.BIG_ENDIAN)));

        carrier("float", ValueLayout.JAVA_FLOAT);
        t("float.narrow.withName", () -> cls(ValueLayout.JAVA_FLOAT.withName("x")));
        t("float.narrow.withAlign", () -> cls(ValueLayout.JAVA_FLOAT.withByteAlignment(1)));
        t("float.narrow.withOrder", () -> cls(ValueLayout.JAVA_FLOAT.withOrder(ByteOrder.BIG_ENDIAN)));

        carrier("double", ValueLayout.JAVA_DOUBLE);
        t("double.narrow.withName", () -> cls(ValueLayout.JAVA_DOUBLE.withName("x")));
        t("double.narrow.withAlign", () -> cls(ValueLayout.JAVA_DOUBLE.withByteAlignment(1)));
        t("double.narrow.withOrder", () -> cls(ValueLayout.JAVA_DOUBLE.withOrder(ByteOrder.BIG_ENDIAN)));

        carrier("addr", ValueLayout.ADDRESS);
        t("addr.narrow.withName", () -> cls(ValueLayout.ADDRESS.withName("x")));
        t("addr.narrow.withAlign", () -> cls(ValueLayout.ADDRESS.withByteAlignment(1)));
        t("addr.narrow.withOrder", () -> cls(ValueLayout.ADDRESS.withOrder(ByteOrder.BIG_ENDIAN)));
        t("addr.targetLayout.empty", () -> ValueLayout.ADDRESS.targetLayout());
        t("addr.withTargetLayout.class", () -> cls(ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT)));
        t("addr.withTargetLayout.get", () -> ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT).targetLayout());
        t("addr.withTargetLayout.toString", () -> ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT).toString());
        t("addr.withTargetLayout.size", () -> ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT).byteSize());

        // ---- the non-value layouts -------------------------------------
        SequenceLayout sq = MemoryLayout.sequenceLayout(3, ValueLayout.JAVA_INT);
        p("seq.class", cls(sq));
        t("seq.count", () -> sq.elementCount());
        t("seq.elem.class", () -> cls(sq.elementLayout()));
        t("seq.elem.size", () -> sq.elementLayout().byteSize());
        t("seq.byteSize", () -> sq.byteSize());
        t("seq.toString", () -> sq.toString());
        t("seq.withName.class", () -> cls(sq.withName("s")));
        t("seq.withName.name", () -> sq.withName("s").name());
        t("seq.withName.toString", () -> sq.withName("s").toString());
        t("seq.reshape", () -> MemoryLayout.sequenceLayout(2, sq).byteSize());

        MemoryLayout pad = MemoryLayout.paddingLayout(4);
        p("pad.class", cls(pad));
        t("pad.byteSize", () -> pad.byteSize());
        t("pad.toString", () -> pad.toString());
        t("pad.withName.class", () -> cls(pad.withName("p")));
        t("pad.withName.name", () -> pad.withName("p").name());
        t("pad.withName.toString", () -> pad.withName("p").toString());

        StructLayout st = MemoryLayout.structLayout(
                ValueLayout.JAVA_INT.withName("b"), ValueLayout.JAVA_INT.withName("c"));
        p("struct.class", cls(st));
        t("struct.byteSize", () -> st.byteSize());
        t("struct.members", () -> st.memberLayouts().size());
        t("struct.m0.name", () -> st.memberLayouts().get(0).name());
        t("struct.m1.name", () -> st.memberLayouts().get(1).name());
        t("struct.byteOffset.c", () -> st.byteOffset(PathElement.groupElement("c")));
        t("struct.toString", () -> st.toString());
        t("struct.withName.class", () -> cls(st.withName("st")));
        t("struct.withName.name", () -> st.withName("st").name());
        t("struct.withName.toString", () -> st.withName("st").toString());
        t("struct.withName.members", () -> st.withName("st").memberLayouts().size());

        UnionLayout un = MemoryLayout.unionLayout(ValueLayout.JAVA_INT.withName("u0"), ValueLayout.JAVA_LONG);
        p("union.class", cls(un));
        t("union.byteSize", () -> un.byteSize());
        t("union.members", () -> un.memberLayouts().size());
        t("union.toString", () -> un.toString());
        t("union.withName.class", () -> cls(un.withName("un")));
        t("union.withName.name", () -> un.withName("un").name());
        t("union.withName.toString", () -> un.withName("un").toString());
        t("union.withName.members", () -> un.withName("un").memberLayouts().size());

        // a named member must survive into a struct and be findable by path
        StructLayout nested = MemoryLayout.structLayout(
                ValueLayout.JAVA_INT.withName("h"), MemoryLayout.paddingLayout(4),
                sq.withName("tail"));
        t("nested.byteSize", () -> nested.byteSize());
        t("nested.offset.tail", () -> nested.byteOffset(PathElement.groupElement("tail")));
        t("nested.select", () -> cls(nested.select(PathElement.groupElement("tail"))));
        t("nested.toString", () -> nested.toString());

        // ---- the VarHandle rows, read back through a HEAP segment ------
        // `varHandle()` (9 rows) and `varHandle(PathElement...)` (3) are the
        // only registered layout methods that hand out something the JDK then
        // EXECUTES, so a class name is not enough: each one is used.
        t("vh.int.class.isVarHandle", () -> java.lang.invoke.VarHandle.class
                .isInstance(ValueLayout.JAVA_INT.varHandle()));
        t("vh.int.roundtrip", () -> {
            MemorySegment h = MemorySegment.ofArray(new int[4]);
            java.lang.invoke.VarHandle vh = ValueLayout.JAVA_INT.varHandle();
            vh.set(h, 0L, 0x21222324);
            return Integer.toHexString((int) vh.get(h, 0L));
        });
        t("vh.long.roundtrip", () -> {
            MemorySegment h = MemorySegment.ofArray(new long[2]);
            java.lang.invoke.VarHandle vh = ValueLayout.JAVA_LONG.varHandle();
            vh.set(h, 0L, 0x3132333435363738L);
            return Long.toHexString((long) vh.get(h, 0L));
        });
        t("vh.struct.path.roundtrip", () -> {
            StructLayout s2 = MemoryLayout.structLayout(
                    ValueLayout.JAVA_INT.withName("lo"), ValueLayout.JAVA_INT.withName("hi"));
            java.lang.invoke.VarHandle vh = s2.varHandle(PathElement.groupElement("hi"));
            MemorySegment h = MemorySegment.ofArray(new int[2]);
            vh.set(h, 0L, 0x41424344);
            return Integer.toHexString(h.get(ValueLayout.JAVA_INT_UNALIGNED, 4));
        });
        t("vh.seq.path.roundtrip", () -> {
            SequenceLayout s3 = MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT);
            java.lang.invoke.VarHandle vh = s3.varHandle(PathElement.sequenceElement());
            MemorySegment h = MemorySegment.ofArray(new int[4]);
            vh.set(h, 0L, 2L, 0x51525354);
            return Integer.toHexString(h.get(ValueLayout.JAVA_INT_UNALIGNED, 8));
        });
        t("vh.struct.byteOffset.lo", () -> MemoryLayout.structLayout(
                ValueLayout.JAVA_INT.withName("lo"), ValueLayout.JAVA_INT.withName("hi"))
                .byteOffset(PathElement.groupElement("lo")));
        t("vh.seq.byteOffset.2", () -> MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT)
                .byteOffset(PathElement.sequenceElement(2)));
        t("vh.pad.byteOffset.throws", () -> MemoryLayout.paddingLayout(4)
                .byteOffset(PathElement.groupElement("nope")));

        System.out.println("L4FfmLayoutSweep DONE");
    }
}
