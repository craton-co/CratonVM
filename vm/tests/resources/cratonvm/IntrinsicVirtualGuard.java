package cratonvm;

/**
 * Virtual-dispatch correctness guard for the Object.hashCode intrinsic.
 *
 * The intrinsic table is keyed on (class, name, descriptor). For a virtual
 * call the interpreter must guard on the *resolved declaring class*: a
 * subclass that OVERRIDES hashCode() must dispatch to the override, NOT to
 * the java/lang/Object.hashCode intrinsic (which would return the identity
 * hash). See intrinsic_table_contract.md "Interpreter integration" and the
 * roadmap risk "Virtual-dispatch unsoundness".
 *
 * This program is run with intrinsics ENABLED. If the intrinsic incorrectly
 * shadowed an overriding subclass, the FixedHash.hashCode() call below would
 * return an identity hash instead of the constant 0x1234, and the program
 * would print "FAIL".
 *
 * Plain Java 8 syntax only.
 */
public class IntrinsicVirtualGuard {

    /** Subclass of Object that overrides hashCode() with a constant. */
    static final class FixedHash {
        @Override
        public int hashCode() {
            return 0x1234;
        }
    }

    /** Another subclass with a different constant override. */
    static final class OtherHash {
        @Override
        public int hashCode() {
            return -42;
        }
    }

    /** Subclass that does NOT override hashCode -- intrinsic IS valid here. */
    static final class PlainHash {
    }

    /** A CharSequence implementation overriding length(). */
    static final class FixedLen implements CharSequence {
        @Override public int length() { return 99; }
        @Override public char charAt(int i) { return 'x'; }
        @Override public CharSequence subSequence(int s, int e) { return this; }
        @Override public String toString() { return "FixedLen"; }
    }

    public static void main(String[] args) {
        boolean ok = true;

        // --- overriding subclass: the override must win, every time. -----
        FixedHash fh = new FixedHash();
        for (int i = 0; i < 10000; i++) {
            // Virtual call site. If the intrinsic shadowed the override,
            // this returns an identity hash != 0x1234.
            if (fh.hashCode() != 0x1234) {
                ok = false;
                System.out.println("FAIL: FixedHash.hashCode() returned "
                        + fh.hashCode() + " expected 4660");
                break;
            }
        }

        OtherHash oh = new OtherHash();
        for (int i = 0; i < 10000; i++) {
            if (oh.hashCode() != -42) {
                ok = false;
                System.out.println("FAIL: OtherHash.hashCode() returned "
                        + oh.hashCode() + " expected -42");
                break;
            }
        }

        // Calling hashCode() through an Object-typed reference must STILL
        // pick the override (true virtual dispatch).
        Object asObject = fh;
        if (asObject.hashCode() != 0x1234) {
            ok = false;
            System.out.println("FAIL: (Object)FixedHash.hashCode() returned "
                    + asObject.hashCode());
        }

        // --- non-overriding subclass: identity hash must be consistent. --
        PlainHash ph = new PlainHash();
        int p1 = ph.hashCode();
        int p2 = ph.hashCode();
        if (p1 != p2) {
            ok = false;
            System.out.println("FAIL: PlainHash.hashCode() inconsistent: "
                    + p1 + " vs " + p2);
        }

        // --- overriding length() through a CharSequence reference. -------
        CharSequence cs = new FixedLen();
        for (int i = 0; i < 10000; i++) {
            if (cs.length() != 99) {
                ok = false;
                System.out.println("FAIL: FixedLen.length() returned "
                        + cs.length() + " expected 99");
                break;
            }
        }
        // A real String through the same CharSequence reference must use
        // the String.length intrinsic and return the true length.
        CharSequence realStr = "abcdef";
        if (realStr.length() != 6) {
            ok = false;
            System.out.println("FAIL: String length via CharSequence = "
                    + realStr.length());
        }

        System.out.println(ok ? "VIRTUAL_GUARD_OK" : "VIRTUAL_GUARD_FAIL");
    }
}
