import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * Does `String.hashCode()` actually reach `StringUTF16.hashCode(byte[])`?
 *
 * Everything else has been ruled out by measurement: the `String` object is
 * correct (`coder`, `value.length`, the bytes, and `charAt` all agree with
 * HotSpot), `java.lang.StringUTF16` loads with the same 85 declared methods
 * and the same `HI_BYTE_SHIFT=0` / `LO_BYTE_SHIFT=8` as HotSpot, the defect is
 * identical under `--nojit`, and no native is registered for either
 * `StringUTF16.hashCode` or `StringUTF16.getChar`.
 *
 * So the remaining question is not "is the callee broken" but "is the callee
 * being called". This probe invokes the two helpers DIRECTLY by reflection and
 * prints their answers beside `String.hashCode()`:
 *
 *   * helpers right, `String.hashCode()` wrong -> the dispatch from
 *     `String.hashCode` is not landing where its bytecode says, and the
 *     helpers are innocent;
 *   * helpers wrong the same way -> the fault is inside `StringUTF16`, and
 *     `getChar` vs `hashCode` separates which.
 *
 * Needs `--add-opens java.base/java.lang=ALL-UNNAMED` on HotSpot.
 */
public class StringUtf16DispatchProbe {

    public static void main(String[] args) throws Exception {
        String s = new String(new char[] { '\u03A3', '\u039F', '\u03A3' });

        Field vf = String.class.getDeclaredField("value");
        vf.setAccessible(true);
        byte[] value = (byte[]) vf.get(s);

        int jls = 0;
        for (int i = 0; i < s.length(); i++) {
            jls = 31 * jls + s.charAt(i);
        }
        System.out.println("String.hashCode()      = " + s.hashCode());
        System.out.println("JLS over charAt        = " + jls);

        Class<?> utf16 = Class.forName("java.lang.StringUTF16");

        Method hash = utf16.getDeclaredMethod("hashCode", byte[].class);
        hash.setAccessible(true);
        System.out.println("StringUTF16.hashCode() = " + hash.invoke(null, (Object) value));

        Method getChar = utf16.getDeclaredMethod("getChar", byte[].class, int.class);
        getChar.setAccessible(true);
        StringBuilder units = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = (Character) getChar.invoke(null, value, i);
            units.append(String.format("%04X ", (int) c));
        }
        System.out.println("StringUTF16.getChar*   = " + units.toString().trim());

        Method len = utf16.getDeclaredMethod("length", byte[].class);
        len.setAccessible(true);
        System.out.println("StringUTF16.length()   = " + len.invoke(null, (Object) value));
        System.out.println("value.length           = " + value.length);
        System.out.println("DISPATCH-PROBE-DONE");
    }
}
