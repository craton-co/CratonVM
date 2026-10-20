// Interpreter round i1, wave 23, lane L5 — JVMS §5.4.3: a recorded resolution
// error is rethrown as THE SAME error, cause included.
//
// `new Missing()` (the class itself is gone) and `new Sub()` (Sub is present,
// its superclass Base is gone) each run three times. HotSpot records the
// first failure per constant-pool entry, cause class and message included
// (`SystemDictionary::add_resolution_error`), and every later attempt throws a
// new error of that class and message with a new cause of the recorded
// cause's class and message (`ConstantPool::throw_resolution_error`).
//
// SETUP (both VMs): after `javac -d out`, delete
// `out/L5W23ResolutionErrorCause$Missing.class` and
// `out/L5W23ResolutionErrorCause$Base.class`. Then
//   java -cp out L5W23ResolutionErrorCause
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W23ResolutionErrorCause
//
// Expected HotSpot 25 output (compare verbatim):
//   missing#0: java.lang.NoClassDefFoundError: L5W23ResolutionErrorCause$Missing | cause=java.lang.ClassNotFoundException: L5W23ResolutionErrorCause$Missing
//   missing#1: java.lang.NoClassDefFoundError: L5W23ResolutionErrorCause$Missing | cause=java.lang.ClassNotFoundException: L5W23ResolutionErrorCause$Missing
//   missing#2: java.lang.NoClassDefFoundError: L5W23ResolutionErrorCause$Missing | cause=java.lang.ClassNotFoundException: L5W23ResolutionErrorCause$Missing
//   super#0: java.lang.NoClassDefFoundError: L5W23ResolutionErrorCause$Base | cause=java.lang.ClassNotFoundException: L5W23ResolutionErrorCause$Base
//   super#1: java.lang.NoClassDefFoundError: L5W23ResolutionErrorCause$Base | cause=java.lang.ClassNotFoundException: L5W23ResolutionErrorCause$Base
//   super#2: java.lang.NoClassDefFoundError: L5W23ResolutionErrorCause$Base | cause=java.lang.ClassNotFoundException: L5W23ResolutionErrorCause$Base
//
// CratonVM before wave 23 (from the code; not run): the `super` rows' first
// attempt carries the ClassNotFoundException cause
// (`raise_no_class_def_found_with_cause`), and `super#1` / `super#2`, rethrown
// from the record, printed `cause=null`: the record kept the error's class and
// message only. The `missing` rows print `cause=null` on every attempt in
// CratonVM (`raise_no_class_def_found` builds no cause), a separate
// divergence of the FIRST throw, filed by wave 23 and fixed in wave 24:
// docs/internal/fixed-bugs/interpreter-L5-a-missing-class-ncdfe-has-no-class-not-found-cause-FIXED-20260927.md.
// Since wave 24 all six rows equal HotSpot's with the SETUP above. Without
// the deletion step every row prints `resolved` on both VMs.
public class L5W23ResolutionErrorCause {
    static class Missing { }

    static class Base { }

    static class Sub extends Base { }

    static Object makeMissing() {
        return new Missing();
    }

    static Object makeSub() {
        return new Sub();
    }

    static void attempt(String label, java.util.function.Supplier<Object> body) {
        for (int i = 0; i < 3; i++) {
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
        attempt("missing", L5W23ResolutionErrorCause::makeMissing);
        attempt("super", L5W23ResolutionErrorCause::makeSub);
    }
}
