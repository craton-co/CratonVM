// Interpreter round i1, wave 24, lane L5 — a `NoClassDefFoundError` for a
// class that is simply missing carries a `ClassNotFoundException` cause.
//
// HotSpot's `SystemDictionary::resolve_or_fail` wraps whatever the initiating
// loader's lookup threw. The application loader throws
// `ClassNotFoundException` (dotted name of the class it was asked for), so the
// error names the class the constant-pool entry names -- `[LGone;` for an
// array -- and its cause names the ELEMENT class the loader was asked for.
// Only a bootstrap-initiated miss has no cause (the boot loader answers null).
// Before wave 24 CratonVM built every one of these rows with `cause=null`
// (`raise_no_class_def_found`); the supertype-missing shape already had the
// cause. See
// docs/internal/fixed-bugs/interpreter-L5-a-missing-class-ncdfe-has-no-class-not-found-cause-FIXED-20260927.md.
//
// Each row runs twice: the second attempt is the JVMS §5.4.3 rethrow of the
// recorded error (same class, message and cause). The rows cover `new`,
// `anewarray`, `checkcast` of an array, `multianewarray` and a `getstatic`
// field owner.
//
// SETUP (both VMs): after `javac -d out L5W24MissingClassCause.java`, delete
// `out/L5W24MissingClassCause$Gone.class`. Then
//   java -cp out L5W24MissingClassCause
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W24MissingClassCause
//
// Expected HotSpot 25 output (compare verbatim):
//   new#0: java.lang.NoClassDefFoundError: L5W24MissingClassCause$Gone | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//   new#1: java.lang.NoClassDefFoundError: L5W24MissingClassCause$Gone | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//   anewarray#0: java.lang.NoClassDefFoundError: L5W24MissingClassCause$Gone | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//   anewarray#1: java.lang.NoClassDefFoundError: L5W24MissingClassCause$Gone | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//   cast#0: java.lang.NoClassDefFoundError: [LL5W24MissingClassCause$Gone; | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//   cast#1: java.lang.NoClassDefFoundError: [LL5W24MissingClassCause$Gone; | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//   multi#0: java.lang.NoClassDefFoundError: [[LL5W24MissingClassCause$Gone; | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//   multi#1: java.lang.NoClassDefFoundError: [[LL5W24MissingClassCause$Gone; | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//   static#0: java.lang.NoClassDefFoundError: L5W24MissingClassCause$Gone | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//   static#1: java.lang.NoClassDefFoundError: L5W24MissingClassCause$Gone | cause=java.lang.ClassNotFoundException: L5W24MissingClassCause$Gone
//
// CratonVM before wave 24 (from the code; not run): every row `cause=null`,
// and the `multi` rows named the ELEMENT class
// (`NoClassDefFoundError: L5W24MissingClassCause$Gone`): `multianewarray_alloc`
// built the error from the leaf name, under a comment claiming HotSpot does.
// HotSpot 25 names the array class the constant-pool entry names.
public class L5W24MissingClassCause {
    static class Gone {
        static int counter;
    }

    static Object makeNew() {
        return new Gone();
    }

    static Object makeArray() {
        return new Gone[1];
    }

    static Object cast(Object o) {
        return (Gone[]) o;
    }

    static Object makeMulti() {
        return new Gone[1][1];
    }

    static Object readStatic() {
        return Gone.counter;
    }

    static void attempt(String label, java.util.function.Supplier<Object> body) {
        for (int i = 0; i < 2; i++) {
            try {
                body.get();
                System.out.println(label + "#" + i + ": resolved");
            } catch (Throwable t) {
                Throwable c = t.getCause();
                System.out.println(label + "#" + i + ": " + t.getClass().getName() + ": " + t.getMessage()
                        + " | cause=" + (c == null ? "null" : c.getClass().getName() + ": " + c.getMessage()));
            }
        }
    }

    public static void main(String[] args) {
        attempt("new", L5W24MissingClassCause::makeNew);
        attempt("anewarray", L5W24MissingClassCause::makeArray);
        attempt("cast", () -> cast(new Object[0]));
        attempt("multi", L5W24MissingClassCause::makeMulti);
        attempt("static", L5W24MissingClassCause::readStatic);
    }
}
