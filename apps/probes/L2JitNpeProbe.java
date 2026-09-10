/** Does a JIT-compiled method still produce JEP 358's helpful NPE message?
 *
 *  Retiring `BigInteger`'s shadows moved six rows from a native (whose message
 *  was a hand-installed constant) to real bytecode, and with the JIT on those
 *  six lost their message: `NullPointerException: null` where HotSpot and this
 *  VM's own interpreter both name the field. `--nojit` on the same binary and
 *  the same probe is 0-diff on all six, so the message is lost in COMPILED code
 *  and the finding has nothing to do with `BigInteger`.
 *
 *  This asks it with no JDK class involved: the same call, cold and then hot.
 *  Each shape prints its cold answer, warms the method past the compile
 *  threshold with non-null arguments, and asks again -- so a difference between
 *  the two lines of a pair is the compiler and cannot be anything else.
 */
public class L2JitNpeProbe {
    static int rows = 0;
    static final int WARM = 200_000;

    static class Holder {
        int value = 7;
        int[] arr = new int[3];
        String s = "x";
        int get() { return value; }
    }

    static void row(String label, Object v) {
        System.out.println(label + " |" + v + "|");
        rows++;
    }

    static String msg(Runnable r) {
        try {
            r.run();
            return "NO THROW";
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    // Each shape is its own method so the JIT compiles it independently.
    static int readField(Holder h) { return h.value; }
    static int readArrayLen(Holder h) { return h.arr.length; }
    static int invokeOn(Holder h) { return h.get(); }
    static int invokeOnString(Holder h) { return h.s.length(); }
    static void writeField(Holder h) { h.value = 1; }

    static void pair(String tag, java.util.function.Consumer<Holder> f) {
        row(tag + " cold", msg(() -> f.accept(null)));
        Holder live = new Holder();
        for (int i = 0; i < WARM; i++) {
            f.accept(live);
        }
        row(tag + " hot", msg(() -> f.accept(null)));
    }

    public static void main(String[] a) {
        pair("readField", h -> readField(h));
        pair("readArrayLen", h -> readArrayLen(h));
        pair("invokeOn", h -> invokeOn(h));
        pair("invokeOnString", h -> invokeOnString(h));
        pair("writeField", h -> writeField(h));
        System.out.println("rows " + rows);
        System.out.println("DONE L2JitNpeProbe");
    }
}
