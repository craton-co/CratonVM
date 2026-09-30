package cratonvm;

// Class-hierarchy-analysis fixture (M5, NOTES-cha.md).
//
// The point of this fixture is a call site with NO PROFILE EVIDENCE whose
// callee is nevertheless decidable. `Op` has exactly one concrete implementor
// loaded, so class-hierarchy analysis can name `Doubler.apply` for the
// `invokeinterface` below; a receiver profile cannot, because the test harness
// never enables profiling. That makes `hierarchy_bound_sites` on the compiled
// artifact a clean one-variable read on whether CHA planned anything.
//
// Called REPEATEDLY BY THE HARNESS rather than looped internally in Java, for
// the reason `PgoGuardedVirtualInline.java` gives: the method's own invocation
// count then drives ordinary tiered compilation instead of OSR.
public class ChaUniqueImplementor {
    // Deliberately an INTERFACE, not an abstract class: the residual
    // NOTES-cha.md records is specifically that an `invokeinterface` site on an
    // unprofiled first compile used to refuse with `NoProfileEvidence`.
    public interface Op {
        int apply(int x);
    }

    // The ONE concrete implementor. PUBLIC for the reason the PGO fixture
    // documents: the guarded inliner refuses a package-private method selected
    // from a class other than the constant-pool class.
    public static final class Doubler implements Op {
        public int apply(int x) {
            return x + x;
        }
    }

    // Held at the INTERFACE type, so the call below really is an
    // `invokeinterface` and not a devirtualized static call the verifier could
    // have resolved.
    private static final Op OP = new Doubler();

    public static int run(int x) {
        return OP.apply(x);
    }
}
