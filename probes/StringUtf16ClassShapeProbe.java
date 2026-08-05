import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;

/**
 * Is `java.lang.StringUTF16` loaded WHOLE?
 *
 * `String.hashCode()` on a UTF-16 string folds the first `length()` bytes of
 * the backing array, sign-extended -- `(char) value[i]` where it needs
 * `getChar(value, i)`. The object is provably fine (`coder`, `value.length`
 * and the bytes all read back correct, and `charAt` agrees with HotSpot), the
 * defect is identical under `--nojit` so it is not a codegen bug, and no
 * native is registered for `StringUTF16.hashCode` or `StringUTF16.getChar`.
 * That leaves the class itself.
 *
 * RKC16N.7 guessed exactly this shape for `java.lang.String` in April 2026 --
 * "loaded as a *partial* class (some methods absent -- possibly due to JMOD
 * parser skipping certain attributes)" -- and was right; the cause was a jimage
 * header decode bug fixed as RKC16N.9. This probe asks the same question of
 * `StringUTF16`, which is the class actually on the path here.
 *
 * It prints the declared methods and the two endianness statics, because those
 * are the three ways `getChar` can go wrong:
 *
 *   * `getChar` / `hashCode` MISSING  -> a partial load; whatever answers is
 *     a fabricated stand-in, and its shape explains the wrong fold;
 *   * `HI_BYTE_SHIFT` / `LO_BYTE_SHIFT` absent or not {0,8} -> `<clinit>`
 *     did not run, or `isBigEndian()` disagrees with the VM's own layout;
 *   * everything present and correct -> the fault is in executing the
 *     bytecode, and this rules the class out rather than guessing.
 *
 * Needs `--add-opens java.base/java.lang=ALL-UNNAMED` on HotSpot. Run it there
 * first: the point is the DIFFERENCE, and a list of methods is meaningless
 * without the reference list beside it.
 */
public class StringUtf16ClassShapeProbe {

    static void reportClass(String binaryName, String[] methodsOfInterest,
            String[] fieldsOfInterest) {
        System.out.println("=== " + binaryName);
        Class<?> c;
        try {
            c = Class.forName(binaryName);
        } catch (Throwable t) {
            System.out.println("  NOT LOADABLE: " + t.getClass().getName());
            return;
        }

        List<String> declared = new ArrayList<>();
        try {
            for (Method m : c.getDeclaredMethods()) {
                declared.add(m.getName());
            }
        } catch (Throwable t) {
            System.out.println("  getDeclaredMethods failed: " + t.getClass().getName());
        }
        System.out.println("  declaredMethodCount=" + declared.size());
        for (String want : methodsOfInterest) {
            int n = 0;
            for (String d : declared) {
                if (d.equals(want)) {
                    n++;
                }
            }
            System.out.println("  method " + want + " declared=" + (n > 0) + " overloads=" + n);
        }

        for (String want : fieldsOfInterest) {
            try {
                Field f = c.getDeclaredField(want);
                f.setAccessible(true);
                System.out.println("  field " + want + " = " + f.get(null));
            } catch (Throwable t) {
                System.out.println("  field " + want + " UNAVAILABLE: "
                        + t.getClass().getName());
            }
        }
    }

    public static void main(String[] args) {
        reportClass("java.lang.StringUTF16",
                new String[] { "getChar", "hashCode", "charAt", "putChar", "compress",
                        "toBytes", "isBigEndian", "length" },
                new String[] { "HI_BYTE_SHIFT", "LO_BYTE_SHIFT", "MAX_LENGTH" });
        reportClass("java.lang.StringLatin1",
                new String[] { "getChar", "hashCode", "charAt", "inflate", "compareTo" },
                new String[] {});
        reportClass("java.lang.String",
                new String[] { "hashCode", "charAt", "length", "isLatin1", "substring" },
                new String[] { "COMPACT_STRINGS", "LATIN1", "UTF16" });
        System.out.println("SHAPE-PROBE-DONE");
    }
}
