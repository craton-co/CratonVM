/**
 * Paired probe for field-initializer ordering across a constructor chain,
 * diffed against the host JDK.
 *
 * JLS 12.5: a class's instance field initializers run **after** its
 * `super(...)` call and **before** its own constructor body. So a base class
 * that both initializes a field inline and assigns it in a constructor —
 *
 *     private int port = 8080;
 *     Base(int port) { this.port = port; }
 *
 * — must end up with the constructor's value, no matter how many subclasses sit
 * above it. Running the initializers late (or twice) silently restores the
 * inline default and looks like "the argument was ignored".
 *
 * This is not hypothetical. Spring Boot's
 * `AbstractConfigurableWebServerFactory` has exactly that shape
 * (`private int port = 8080` + `AbstractConfigurableWebServerFactory(int)`),
 * and `new TomcatServletWebServerFactory(0)` — two subclasses up, port 0
 * meaning "pick an ephemeral port" — produced a server on **8080** under
 * CratonVM while HotSpot picked ephemeral ports. 99 connectors on the same
 * fixed port instead of 99 different ephemeral ones.
 *
 * Every line prints a value. Anything other than the stated expectation is a
 * divergence; the expectations are JLS-mandated, not a HotSpot quirk.
 */
public class FieldInitVsSuperCtorProbe {

    // --- the exact Spring shape: inline default + ctor assignment ---
    static class Base {
        private int port = 8080;
        private String name = "default";
        private final java.util.List<String> pages = new java.util.ArrayList<>();

        Base() {
        }

        Base(int port) {
            this.port = port;
        }

        int getPort() {
            return this.port;
        }

        String getName() {
            return this.name;
        }

        int pageCount() {
            return this.pages.size();
        }
    }

    static class Mid extends Base {
        Mid(int port) {
            super(port);
        }
    }

    static class Leaf extends Mid {
        Leaf(int port) {
            super(port);
        }
    }

    /** Subclass that adds its own initialized field, to show the two levels
     *  do not interfere. */
    static class LeafWithOwnField extends Mid {
        private int extra = 7;

        LeafWithOwnField(int port) {
            super(port);
        }

        int getExtra() {
            return this.extra;
        }
    }

    /** A ctor that reads the field it is about to overwrite — catches an
     *  implementation that zeroes rather than initializes. */
    static class ReadsBeforeWrite extends Base {
        final int observed;

        ReadsBeforeWrite(int port) {
            super();
            this.observed = getPort();
        }
    }

    public static void main(String[] args) {
        System.out.println("expect 0     : Leaf(0).getPort()          = " + new Leaf(0).getPort());
        System.out.println("expect 0     : Mid(0).getPort()           = " + new Mid(0).getPort());
        System.out.println("expect 0     : Base(0).getPort()          = " + new Base(0).getPort());
        System.out.println("expect 8080  : Base().getPort()           = " + new Base().getPort());
        System.out.println("expect 8080  : Leaf(8080).getPort()       = " + new Leaf(8080).getPort());
        System.out.println("expect 1234  : Leaf(1234).getPort()       = " + new Leaf(1234).getPort());
        System.out.println("expect -1    : Leaf(-1).getPort()         = " + new Leaf(-1).getPort());

        // Fields the ctor never touches must still hold their inline defaults.
        Leaf leaf = new Leaf(0);
        System.out.println("expect default: Leaf(0).getName()         = " + leaf.getName());
        System.out.println("expect 0     : Leaf(0).pageCount()        = " + leaf.pageCount());

        LeafWithOwnField own = new LeafWithOwnField(0);
        System.out.println("expect 0     : LeafWithOwnField.getPort() = " + own.getPort());
        System.out.println("expect 7     : LeafWithOwnField.getExtra()= " + own.getExtra());

        System.out.println("expect 8080  : ReadsBeforeWrite.observed  = " + new ReadsBeforeWrite(0).observed);
    }
}
