import java.lang.foreign.*;
import java.lang.foreign.MemoryLayout.PathElement;

/** Minimal repro of RJdkForeign's `layouts` step: does a named member survive
 *  into the struct's member list, and can the path walk find it? */
public class FfmLayoutProbe {
    static void p(String tag, Object v) { System.out.println(tag + " |" + v + "|"); }
    static void t(String tag, java.util.concurrent.Callable<Object> c) {
        try { p(tag, c.call()); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    public static void main(String[] a) {
        ValueLayout.OfInt named = ValueLayout.JAVA_INT.withName("c");
        p("member class", named.getClass().getName());
        t("member name", () -> named.name());
        StructLayout st = MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("b"), named);
        p("struct class", st.getClass().getName());
        t("struct byteSize", () -> st.byteSize());
        t("struct members", () -> st.memberLayouts().size());
        t("member 0 name", () -> st.memberLayouts().get(0).name());
        t("member 1 name", () -> st.memberLayouts().get(1).name());
        t("byteOffset(c)", () -> st.byteOffset(PathElement.groupElement("c")));
        SequenceLayout sq = MemoryLayout.sequenceLayout(3, ValueLayout.JAVA_INT);
        p("seq class", sq.getClass().getName());
        t("seq count", () -> sq.elementCount());
        t("seq elem", () -> sq.elementLayout().byteSize());
        t("seq toString", () -> sq.toString());
        t("pad", () -> MemoryLayout.paddingLayout(4).byteSize());
    }
}
