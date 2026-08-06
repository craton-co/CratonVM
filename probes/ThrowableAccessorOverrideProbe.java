import java.io.InvalidClassException;

/**
 * The two classes the blanket-list sweep flagged after
 * `PatternSyntaxException`: do their OVERRIDDEN accessors actually run?
 *
 * `native-builtins` registers `getMessage` / `getLocalizedMessage` / `toString`
 * bridges over ~68 throwable classes. A bridge in front of a class that
 * OVERRIDES the method returns `Throwable.detailMessage` instead of whatever
 * the override computes -- the defect fixed for `PatternSyntaxException` on
 * 2026-08-05. Checking `javap -p` against the whole list turned up exactly two
 * more classes that declare their own:
 *
 *   * `java.io.InvalidClassException.getMessage()` prepends the offending
 *     class name to the detail message.
 *   * `java.lang.NullPointerException.getMessage()` computes the helpful
 *     "Cannot invoke ... because ... is null" text lazily, when and only when
 *     `detailMessage` is null. That is the same text three rows of
 *     `StringPolicyMatrixProbe` still differ on.
 *
 * The NPE half needs VM support beyond removing a bridge (the JDK override
 * calls a native that reads the failing bytecode), so this probe exists to say
 * WHICH half is which rather than to assert a single fix.
 */
public class ThrowableAccessorOverrideProbe {

    static void show(String label, Throwable t) {
        System.out.println("  " + label);
        System.out.println("      getMessage()          = " + t.getMessage());
        System.out.println("      getLocalizedMessage() = " + t.getLocalizedMessage());
        System.out.println("      toString()            = " + t);
    }

    public static void main(String[] args) {
        System.out.println("InvalidClassException(classname, message)");
        show("both set", new InvalidClassException("com.example.Foo", "bad serialVersionUID"));
        show("message only", new InvalidClassException("just a message"));

        System.out.println("NullPointerException with an EXPLICIT message");
        show("explicit", new NullPointerException("explicit text"));

        System.out.println("NullPointerException with NO message (helpful-NPE territory)");
        show("no message", new NullPointerException());

        System.out.println("NullPointerException raised by a real null dereference");
        try {
            String s = null;
            s.isEmpty();
            System.out.println("  NO-THROW (unexpected)");
        } catch (NullPointerException e) {
            show("thrown by s.isEmpty() on null", e);
        }

        try {
            int[] a = null;
            int n = a.length;
            System.out.println("  NO-THROW (unexpected) " + n);
        } catch (NullPointerException e) {
            show("thrown by a.length on null", e);
        }

        System.out.println("THROWABLE-ACCESSOR-PROBE-DONE");
    }
}
