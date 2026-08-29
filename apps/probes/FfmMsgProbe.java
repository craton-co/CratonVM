import java.lang.foreign.*;
/** The exact MESSAGE of every FFM refusal this lane is about to add. */
public class FfmMsgProbe {
    static void m(String tag, Run r) {
        try { r.run(); System.out.println(tag + " |no-throw|"); }
        catch (Throwable e) {
            System.out.println(tag + " |" + e.getClass().getName() + "| msg |" + e.getMessage() + "|");
        }
    }
    interface Run { void run() throws Throwable; }
    public static void main(String[] a) {
        m("global close", () -> Arena.global().close());
        m("ofAuto close", () -> Arena.ofAuto().close());
        m("ofArray(null)", () -> MemorySegment.ofArray((byte[]) null));
        m("ofBuffer(null)", () -> MemorySegment.ofBuffer(null));
        try (Arena ar = Arena.ofConfined()) {
            m("allocate(-1)", () -> ar.allocate(-1));
            m("allocate(8,0)", () -> ar.allocate(8, 0));
            m("allocate(8,3)", () -> ar.allocate(8, 3));
            m("allocate(8,-4)", () -> ar.allocate(8, -4));
            m("allocate(-1,4)", () -> ar.allocate(-1, 4));
            MemorySegment s = ar.allocate(16);
            System.out.println("slice(0,16).equals(whole) |" + s.asSlice(0, 16).equals(s) + "|");
            System.out.println("slice(0,16)==whole |" + (s.asSlice(0, 16) == s) + "|");
            System.out.println("slice hashCode equal |" + (s.asSlice(0, 16).hashCode() == s.hashCode()) + "|");
            System.out.println("slice(4,8).equals(itself) |" + s.asSlice(4, 8).equals(s.asSlice(4, 8)) + "|");
        }
        System.out.println("DONE FfmMsgProbe");
    }
}
