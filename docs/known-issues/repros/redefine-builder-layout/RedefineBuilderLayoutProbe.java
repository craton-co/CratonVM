import java.io.InputStream;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Function;

/**
 * After {@code java.lang.AbstractStringBuilder} is redefined, does a REAL
 * {@code StringBuilder} still behave the way it did a moment earlier?
 *
 * <p>This is the Mockito/Spring witness with both removed. Mockito's inline
 * mock maker was only ever a route to {@code Instrumentation.redefineClasses};
 * the corruption under test is the VM's. The redefinition installs the class's
 * OWN bytes, so nothing about the class changes and every difference below is
 * the VM's doing.
 *
 * <p>Why it corrupts: CratonVM's builders are a synthetic two-field
 * {@code char[] value} / {@code int count} object, not the JDK's compact
 * {@code byte[] value} / {@code byte coder} / {@code int count}. Every builder
 * operation is therefore supposed to resolve to a native shim — see
 * {@code is_string_builder_layout_native_override} in
 * {@code vm/src/runtime/interpreter/invoke.rs}. A redefinition evicts the
 * native shadow, and any operation missing from that list falls back to the
 * real JDK body, which then indexes a layout the object does not have.
 *
 * <pre>
 *   java     RedefineBuilderLayoutProbe   # control: the redefine is skipped
 *   cratonvm RedefineBuilderLayoutProbe   # must print PROBE PASS
 * </pre>
 */
public final class RedefineBuilderLayoutProbe {

    private record Case(String name, Function<StringBuilder, String> op) {}

    private static String show(StringBuilder b) {
        return "len=" + b.length() + " str=" + b;
    }

    private static final List<Case> CASES = new ArrayList<>();
    static {
        CASES.add(new Case("append(char)", b -> { b.append('Z'); return show(b); }));
        CASES.add(new Case("append(String)", b -> { b.append("XY"); return show(b); }));
        CASES.add(new Case("length()", b -> "len=" + b.length()));
        CASES.add(new Case("toString()", b -> "str=" + b));
        CASES.add(new Case("charAt(2)", b -> "c=" + b.charAt(2)));
        CASES.add(new Case("setLength(4)", b -> { b.setLength(4); return show(b); }));
        CASES.add(new Case("setLength(0)", b -> { b.setLength(0); return show(b); }));
        CASES.add(new Case("setLength(len-1)", b -> { b.setLength(b.length() - 1); return show(b); }));
        CASES.add(new Case("setCharAt(1)", b -> { b.setCharAt(1, 'Q'); return show(b); }));
        CASES.add(new Case("deleteCharAt(1)", b -> { b.deleteCharAt(1); return show(b); }));
        CASES.add(new Case("delete(1,3)", b -> { b.delete(1, 3); return show(b); }));
        CASES.add(new Case("insert(1,Q)", b -> { b.insert(1, "Q"); return show(b); }));
        CASES.add(new Case("replace(1,3,Q)", b -> { b.replace(1, 3, "Q"); return show(b); }));
        CASES.add(new Case("reverse()", b -> { b.reverse(); return show(b); }));
        CASES.add(new Case("substring(2)", b -> "s=" + b.substring(2)));
        CASES.add(new Case("substring(1,4)", b -> "s=" + b.substring(1, 4)));
        CASES.add(new Case("indexOf(cd)", b -> "i=" + b.indexOf("cd")));
        CASES.add(new Case("lastIndexOf(c)", b -> "i=" + b.lastIndexOf("c")));
        CASES.add(new Case("isEmpty()", b -> "e=" + b.isEmpty()));
        CASES.add(new Case("chars().count()", b -> "n=" + b.chars().count()));
        CASES.add(new Case("getChars", b -> {
            char[] dst = new char[4];
            b.getChars(1, 5, dst, 0);
            return "d=" + new String(dst);
        }));
        CASES.add(new Case("compareTo(equal)", b -> "c=" + b.compareTo(new StringBuilder("abcdefghij"))));
        CASES.add(new Case("ensureCapacity", b -> { b.ensureCapacity(500); b.append('!'); return show(b); }));
        CASES.add(new Case("trimToSize", b -> { b.trimToSize(); b.append('!'); return show(b); }));
        CASES.add(new Case("appendCodePoint", b -> { b.appendCodePoint(0x41); return show(b); }));
        CASES.add(new Case("codePointAt(0)", b -> "cp=" + b.codePointAt(0)));
        CASES.add(new Case("repeat(x,3)", b -> { b.repeat('x', 3); return show(b); }));
        CASES.add(new Case("capacity()>=len", b -> "ok=" + (b.capacity() >= b.length())));
    }

    /** Every case starts from an identical, freshly built builder. */
    private static String[] runAll() {
        String[] out = new String[CASES.size()];
        for (int i = 0; i < CASES.size(); i++) {
            StringBuilder b = new StringBuilder("abcdefghij");
            try {
                out[i] = CASES.get(i).op().apply(b);
            } catch (Throwable t) {
                out[i] = "THREW " + t.getClass().getName() + ": " + t.getMessage();
            }
        }
        return out;
    }

    /** Read a boot class's own bytes back out of the runtime image. */
    private static byte[] bootBytes(String binaryName) throws Exception {
        String resource = "/" + binaryName.replace('.', '/') + ".class";
        try (InputStream in = Object.class.getResourceAsStream(resource)) {
            if (in == null) {
                throw new IllegalStateException("cannot read " + resource + " from the runtime image");
            }
            return in.readAllBytes();
        }
    }

    private static boolean redefine(String binaryName) {
        try {
            Class<?> target = Class.forName(binaryName, false, null);
            Class<?> instrument = Class.forName("cratonvm.Instrument");
            Object applied = instrument.getMethod("redefineClass", Class.class, byte[].class)
                    .invoke(null, target, bootBytes(binaryName));
            return Boolean.TRUE.equals(applied);
        } catch (Throwable t) {
            System.out.println("REDEFINE skipped for " + binaryName + " (" + t + ")");
            return false;
        }
    }

    public static void main(String[] args) {
        String[] before = runAll();

        boolean applied = redefine("java.lang.AbstractStringBuilder");
        applied |= redefine("java.lang.StringBuilder");
        System.out.println("REDEFINE applied=" + applied);

        String[] after = runAll();

        int diffs = 0;
        for (int i = 0; i < CASES.size(); i++) {
            boolean same = before[i].equals(after[i]);
            if (!same) {
                diffs++;
            }
            System.out.printf("%-4s %-18s before[%s]  after[%s]%n",
                    same ? "same" : "DIFF", CASES.get(i).name(), before[i], after[i]);
        }
        System.out.println(diffs == 0
                ? "PROBE PASS"
                : "PROBE FAIL " + diffs + " operation(s) changed behaviour across the redefinition");
        if (diffs != 0) {
            System.exit(1);
        }
    }
}
