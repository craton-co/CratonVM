import java.util.*;

/**
 * The six triples `native-builtins/src/deprecated_io_util.rs` actually OWNS in a
 * live boot, five of which shadow real `java.base` bytecode.
 *
 * MEASURED, `--dump-native-registry` unioned over 105 corpus vectors: that file
 * registers 38 triples and wins the slot for exactly six —
 *
 *     java/lang/Class.newInstance()Ljava/lang/Object;   inv=7   SHADOW
 *     java/lang/Number.byteValue()B                     inv=0   SHADOW
 *     java/lang/Number.shortValue()S                    inv=0   SHADOW
 *     java/util/Date.toGMTString()Ljava/lang/String;    inv=0   SHADOW
 *     java/util/Date.toLocaleString()Ljava/lang/String; inv=0   SHADOW
 *     java/io/LineNumberInputStream.mark(I)V            inv=0   (absent from the image)
 *
 * — and NONE of them is a `java.io` row, in a file named `deprecated_io_util`.
 *
 * Every case below has an answer fixed by the javadoc, so the oracle and the VM
 * must agree exactly. `Number.byteValue`/`shortValue` are driven through a USER
 * SUBCLASS as well as through the boxed types, because that is the shape the
 * superclass walk serves and the one a `java.lang.Number` row exists for.
 */
public class W4Deprecated {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    interface Thunk { Object call() throws Exception; }

    /** Identity hashes differ per VM by design; report the CLASS, not the object. */
    static String describe(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static void ckThrows(String tag, Thunk t) {
        try {
            System.out.println("CK " + tag + " ok:" + describe(t.call()));
        } catch (Throwable e) {
            String m = e.getMessage();
            System.out.println("CK " + tag + " threw:" + e.getClass().getName()
                    + (m == null ? "" : ":" + m.replace('\n', ' ')));
        }
    }

    public static class Plain { public Plain() {} public String toString() { return "plain"; } }
    public static abstract class Abstract { public Abstract() {} }
    public interface Iface {}
    public static class PrivateCtor { private PrivateCtor() {} }
    public static class NoNoArg { public NoNoArg(int x) {} }
    public static class Boom { public Boom() { throw new IllegalStateException("boom"); } }
    public static class CheckedBoom { public CheckedBoom() throws Exception { throw new Exception("checked"); } }

    /** The smallest legal Number: only the four abstract primitives. */
    static final class MyNum extends Number {
        private final int v;
        MyNum(int v) { this.v = v; }
        @Override public int intValue() { return v; }
        @Override public long longValue() { return v; }
        @Override public float floatValue() { return v; }
        @Override public double doubleValue() { return v; }
    }

    public static void main(String[] args) throws Exception {
        // ---- Class.newInstance() ----------------------------------------
        ckThrows("cls.newInstance.plain", () -> Plain.class.newInstance());
        ckThrows("cls.newInstance.abstract", () -> Abstract.class.newInstance());
        ckThrows("cls.newInstance.iface", () -> Iface.class.newInstance());
        ckThrows("cls.newInstance.privateCtor", () -> PrivateCtor.class.newInstance());
        ckThrows("cls.newInstance.noNoArg", () -> NoNoArg.class.newInstance());
        // `Class.newInstance` propagates the constructor's exception UNWRAPPED,
        // which is the whole reason it is deprecated.
        ckThrows("cls.newInstance.throwing", () -> Boom.class.newInstance());
        ckThrows("cls.newInstance.checkedThrowing", () -> CheckedBoom.class.newInstance());
        ckThrows("cls.newInstance.primitive", () -> int.class.newInstance());
        ckThrows("cls.newInstance.array", () -> int[].class.newInstance());

        // ---- Number.byteValue() / shortValue() ---------------------------
        // The narrowing is a plain `(byte) intValue()` / `(short) intValue()`,
        // so the interesting inputs are the ones that TRUNCATE and the ones
        // that go NEGATIVE.
        for (int v : new int[] {0, 1, 127, 128, 255, 256, -1, -128, -129, 32767, 32768, 65535, 65536, -32768, -32769}) {
            ck("num.user.byte." + v, new MyNum(v).byteValue());
            ck("num.user.short." + v, new MyNum(v).shortValue());
            ck("num.integer.byte." + v, Integer.valueOf(v).byteValue());
            ck("num.integer.short." + v, Integer.valueOf(v).shortValue());
            ck("num.long.byte." + v, Long.valueOf(v).byteValue());
            ck("num.long.short." + v, Long.valueOf(v).shortValue());
        }
        ck("num.double.byte", Double.valueOf(3.9).byteValue());
        ck("num.double.short", Double.valueOf(-3.9).shortValue());
        ck("num.double.byte.big", Double.valueOf(1e30).byteValue());
        ck("num.float.short", Float.valueOf(258.7f).shortValue());
        ck("num.bigint.byte", new java.math.BigInteger("300").byteValue());
        ck("num.bigdec.short", new java.math.BigDecimal("70000.5").shortValue());
        // `byteValue`/`shortValue` through a Number-typed reference, so the
        // call site's constant-pool class is `java.lang.Number` itself.
        Number n = new MyNum(300);
        ck("num.viaNumberRef.byte", n.byteValue());
        ck("num.viaNumberRef.short", n.shortValue());

        // ---- Date.toGMTString() / toLocaleString() -----------------------
        // Fixed instants, so the answers do not depend on when this runs.
        // `toGMTString` is specified as "d mon yyyy hh:mm:ss GMT" and is
        // TIMEZONE-INDEPENDENT; `toLocaleString` is not, so it is reported
        // rather than asserted.
        long[] instants = {0L, 1L, 946684800000L, 1234567890123L, -1L, -86400000L};
        for (long t : instants) {
            ck("date.toGMTString." + t, new Date(t).toGMTString());
        }
        ck("date.toLocaleString", new Date(946684800000L).toLocaleString());
        // Is the JDK's OWN implementation of `toLocaleString` reachable on this
        // VM? Its body is exactly this expression, so if this line agrees with
        // the oracle then retiring the native is a fix and not a trade.
        ck("date.toLocaleString.jdkPath", java.text.DateFormat
                .getDateTimeInstance(java.text.DateFormat.DEFAULT, java.text.DateFormat.DEFAULT)
                .format(new Date(946684800000L)));
        // The same question for `toGMTString`, whose body builds the string by
        // hand from a `GregorianCalendar` in UTC.
        java.text.SimpleDateFormat gmt = new java.text.SimpleDateFormat("d MMM yyyy HH:mm:ss 'GMT'", Locale.US);
        gmt.setTimeZone(TimeZone.getTimeZone("GMT"));
        ck("date.toGMTString.jdkPath", gmt.format(new Date(946684800000L)));

        System.out.println("PASS W4Deprecated");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
