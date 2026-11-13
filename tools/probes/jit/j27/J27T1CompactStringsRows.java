// JIT round j27 wave 7, lane T1EA: the -XX:-CompactStrings arm against HotSpot (r18 backlog row
// STR17-6; page docs/feature-designs/real-jdk-mode.md).
//
// Run each arm on HotSpot 25 and on cratonvm (--real-jdk and --compatible):
//   javac -d tools/probes/out tools/probes/jit/j27/J27T1CompactStringsRows.java
//   java --add-opens java.base/java.lang=ALL-UNNAMED -cp tools/probes/out J27T1CompactStringsRows
//   java --add-opens java.base/java.lang=ALL-UNNAMED -XX:-CompactStrings -cp tools/probes/out J27T1CompactStringsRows
// Every `fact inv-*` line is an invariant (identical in both arms, on every VM). The `fact flag-*`
// and `fact coder-*` lines are what the flag changes.
//
// Expected HotSpot 25, default arm:
//   fact flag-vmoption true DEFAULT
//   fact flag-static true
//   fact coder-literal 0
//   fact coder-built 0
//   fact coder-sb 0
//   fact coder-int 0
//   fact coder-concat 0
//   fact coder-euro 1
// Expected HotSpot 25, -XX:-CompactStrings:
//   fact flag-vmoption false VM_CREATION
//   fact flag-static false
//   fact coder-literal 1
//   fact coder-built 1
//   fact coder-sb 1
//   fact coder-int 1
//   fact coder-concat 1
//   fact coder-euro 1
// Both arms:
//   fact inv-equals true true true
//   fact inv-hash 96354 96354
//   fact inv-compare -1 1 0
//   fact inv-bytes 3 6 3
//   fact inv-case abc ABC
//   fact inv-index 1 -1 2
//   fact inv-substring bc true
//   fact inv-intern true

import java.lang.management.ManagementFactory;
import java.lang.reflect.Field;
import java.nio.charset.StandardCharsets;

public class J27T1CompactStringsRows {
    static Field coderField;

    static String coder(String s) {
        try {
            if (coderField == null) {
                coderField = String.class.getDeclaredField("coder");
                coderField.setAccessible(true);
            }
            return String.valueOf(coderField.getByte(s));
        } catch (ReflectiveOperationException | RuntimeException e) {
            return "inaccessible:" + e.getClass().getSimpleName();
        }
    }

    static String compactStatic() {
        try {
            Field f = String.class.getDeclaredField("COMPACT_STRINGS");
            f.setAccessible(true);
            return String.valueOf(f.getBoolean(null));
        } catch (ReflectiveOperationException | RuntimeException e) {
            return "inaccessible:" + e.getClass().getSimpleName();
        }
    }

    static String vmOption() {
        try {
            com.sun.management.HotSpotDiagnosticMXBean b =
                    ManagementFactory.getPlatformMXBean(com.sun.management.HotSpotDiagnosticMXBean.class);
            com.sun.management.VMOption o = b.getVMOption("CompactStrings");
            return o.getValue() + " " + o.getOrigin();
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }

    public static void main(String[] args) {
        int five = args.length + 5;
        String literal = "abc";
        String built = new String(new char[] {'a', 'b', 'c'});
        String sb = new StringBuilder("ab").append('c').toString();
        String num = Integer.toString(five);
        String concat = "ab" + five;
        String euro = "a€c";

        System.out.println("fact flag-vmoption " + vmOption());
        System.out.println("fact flag-static " + compactStatic());
        System.out.println("fact coder-literal " + coder(literal));
        System.out.println("fact coder-built " + coder(built));
        System.out.println("fact coder-sb " + coder(sb));
        System.out.println("fact coder-int " + coder(num));
        System.out.println("fact coder-concat " + coder(concat));
        System.out.println("fact coder-euro " + coder(euro));

        System.out.println("fact inv-equals " + literal.equals(built) + " " + built.equals(sb)
                + " " + concat.equals("ab5"));
        System.out.println("fact inv-hash " + literal.hashCode() + " " + built.hashCode());
        System.out.println("fact inv-compare " + literal.compareTo("abd") + " " + "abd".compareTo(sb)
                + " " + built.compareTo(literal));
        System.out.println("fact inv-bytes " + built.getBytes(StandardCharsets.ISO_8859_1).length + " "
                + built.getBytes(StandardCharsets.UTF_16LE).length + " "
                + sb.getBytes(StandardCharsets.UTF_8).length);
        System.out.println("fact inv-case " + "ABC".toLowerCase() + " " + built.toUpperCase());
        System.out.println("fact inv-index " + built.indexOf('b') + " " + sb.indexOf("x") + " "
                + concat.indexOf(num));
        System.out.println("fact inv-substring " + built.substring(1) + " "
                + (built.substring(1).hashCode() == "bc".hashCode()));
        System.out.println("fact inv-intern " + (built.intern() == literal));
    }
}
