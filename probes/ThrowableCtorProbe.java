import java.io.IOException;
import java.io.UncheckedIOException;
import java.text.ParseException;
import java.util.MissingResourceException;

/**
 * The constructors the blanket four-descriptor registration did not cover.
 * Same output on both VMs when the gap is closed; on a VM without the fix the
 * AssertionError rows are NoSuchMethodError.
 *
 * `assertViaKeyword` is the reachability point: `assert cond : msg` compiles to
 * AssertionError.<init>(Ljava/lang/Object;)V, so every -ea assertion failure
 * went through the missing constructor.
 */
public class ThrowableCtorProbe {
    static void row(String label, Callable c) {
        String out;
        try {
            out = "OK   " + c.call();
        } catch (Throwable t) {
            String m = t.getMessage();
            if (m != null && m.length() > 70) m = m.substring(0, 70);
            out = "FAIL " + t.getClass().getName() + ": " + m;
        }
        System.out.println(String.format("%-40s %s", label, out));
    }

    interface Callable { Object call() throws Throwable; }

    static String show(Throwable t) {
        // `msg=null` is ambiguous between a null reference and the four-character
        // string "null" — which is exactly what `AssertionError((Object) null)`
        // stores, since it goes through `String.valueOf`. Spell it out.
        String m = t.getMessage();
        String rendered = (m == null) ? "<null-ref>" : "\"" + m + "\"";
        return "msg=" + rendered
                + " cause=" + (t.getCause() == null ? "null" : t.getCause().getClass().getName());
    }

    @SuppressWarnings("all")
    static String assertViaKeyword() {
        try {
            assert false : "from the assert keyword";
            return "assertions disabled (-ea not set)";
        } catch (AssertionError e) {
            return show(e);
        }
    }

    public static void main(String[] args) {
        row("AssertionError(Object:String)", () -> show(new AssertionError((Object) "boom")));
        row("AssertionError(Object:null)", () -> show(new AssertionError((Object) null)));
        row("AssertionError(Object:Integer)", () -> show(new AssertionError((Object) 7)));
        row("AssertionError(Object:Throwable)",
                () -> show(new AssertionError((Object) new IllegalStateException("inner"))));
        row("AssertionError(boolean)", () -> show(new AssertionError(true)));
        row("AssertionError(char)", () -> show(new AssertionError('x')));
        row("AssertionError(int)", () -> show(new AssertionError(42)));
        row("AssertionError(long)", () -> show(new AssertionError(42L)));
        row("AssertionError(float)", () -> show(new AssertionError(1.5f)));
        row("AssertionError(double)", () -> show(new AssertionError(1.5d)));
        row("AssertionError()", () -> show(new AssertionError()));
        row("AssertionError(String,Throwable)",
                () -> show(new AssertionError("m", new IllegalStateException("i"))));
        row("assert cond : msg", ThrowableCtorProbe::assertViaKeyword);

        row("IndexOutOfBounds(int)", () -> show(new IndexOutOfBoundsException(3)));
        row("IndexOutOfBounds(long)", () -> show(new IndexOutOfBoundsException(3L)));
        row("ArrayIndexOutOfBounds(int)", () -> show(new ArrayIndexOutOfBoundsException(3)));
        row("StringIndexOutOfBounds(int)", () -> show(new StringIndexOutOfBoundsException(3)));

        row("UncheckedIOException(IOE)",
                () -> show(new UncheckedIOException(new IOException("io"))));
        row("UncheckedIOException(String,IOE)",
                () -> show(new UncheckedIOException("wrapped", new IOException("io"))));
        row("UncheckedIOException(null) NPEs", () -> {
            try {
                new UncheckedIOException(null);
                return "NO NPE (wrong)";
            } catch (NullPointerException e) {
                return "NullPointerException (right)";
            }
        });
        row("ParseException(String,int)", () -> show(new ParseException("bad", 5)));
        row("MissingResourceException(S,S,S)",
                () -> show(new MissingResourceException("no bundle", "cn", "k")));

        // The four that stay: still the common shapes on classes that have them.
        row("IllegalStateException(String,Throwable)",
                () -> show(new IllegalStateException("m", new IOException("c"))));
        row("RuntimeException(Throwable)",
                () -> show(new RuntimeException(new IOException("c"))));
        row("IOException()", () -> show(new IOException()));
        row("NoClassDefFoundError(String)", () -> show(new NoClassDefFoundError("pkg/Sup")));

        System.out.println("CTOR-END");
    }
}
