package cratonvm;

// Fixture for PGO-01 (pgo-01-call-site-evidence-gap.md):
// each entry point below has exactly ONE invoke kind in its own loop body, so
// its MethodProfile.call_sites can be asserted on without another call kind's
// evidence polluting the count.
//
// callSpecialLoop's invokespecial source is `new` (constructor invocation),
// NOT a private-method call: modern javac (JEP 181 nestmate access, JDK 11+)
// compiles a same-class private INSTANCE method call as invokevirtual, not
// invokespecial (verified with javap on this fixture before settling on this
// shape — a private-method-call version silently recorded zero evidence).
// `<init>` is the one call javac still always emits as invokespecial.
public class PgoCallSiteEvidence {
    private static final PgoCallSiteEvidence INSTANCE = new PgoCallSiteEvidence();

    static int staticHelper(int x) {
        return x + 1;
    }

    // public, non-static -> javac always emits invokevirtual for a call to
    // this, regardless of receiver expression.
    public int virtualHelper(int x) {
        return x + 3;
    }

    public static int callStaticLoop(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            sum += staticHelper(i);
        }
        return sum;
    }

    // invokespecial call site: `new`'s <init> call, one bci, executed n times.
    // No other invoke instruction in this method's bytecode (verified with
    // javap — see the class doc comment above).
    public static int callSpecialLoop(int n) {
        int count = 0;
        for (int i = 0; i < n; i++) {
            PgoCallSiteEvidence obj = new PgoCallSiteEvidence();
            if (obj != null) {
                count++;
            }
        }
        return count;
    }

    // Negative control: no invokestatic/invokespecial anywhere in this method.
    public static int callVirtualOnlyLoop(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            sum += INSTANCE.virtualHelper(i);
        }
        return sum;
    }
}
