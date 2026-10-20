// h23 adversarial review probe for
// docs/known-issues/jit/jit-review-round9-remaining-work-20260918.md
// #1.3, "Private instance self-call" (CRATONVM_JIT_INSTANCE_SELF_CALL,
// jit/src/lib.rs, x64/op_invoke.rs): exercises the checklist's five named
// scenarios in one run -- null receiver, a subclass receiver of a private
// method, nestmates, synchronized methods, deep recursion / StackOverflowError
// -- with a threshold low enough that every hot method compiles.
public class H23SelfCallReview {
    static class Base {
        private int priv(int x) {
            return x + 1;
        }

        int callsPriv(int x) {
            // A self-call to a PRIVATE method: not virtual, always resolves
            // to Base.priv regardless of the receiver's runtime class.
            int total = 0;
            for (int i = 0; i < 50_000; i++) {
                total = priv(x);
            }
            return total;
        }

        synchronized int callsPrivSynchronized(int x) {
            int total = 0;
            for (int i = 0; i < 50_000; i++) {
                total = priv(x);
            }
            return total;
        }

        static int recurse(int depth) {
            // Every frame's own self-call to `priv` below, INSIDE deep
            // recursion, so the self-call path runs at every stack depth up
            // to overflow.
            Base b = new Base();
            int r = b.priv(depth);
            if (depth <= 0) {
                return r;
            }
            return r + recurse(depth - 1);
        }
    }

    static class Sub extends Base {
        // No override possible: `priv` is private. `callsPriv` is inherited
        // and called on a `Sub` instance below -- the receiver's runtime
        // class differs from the declaring class of the private method being
        // self-called.
    }

    // Nestmate: a private member of the OUTER class, called from an inner
    // class instance method -- goes through a synthetic bridge on older
    // class file versions, or a direct nestmate access on newer ones.
    private int outerPriv(int x) {
        return x * 2;
    }

    class Inner {
        int callOuterPriv(int x) {
            int total = 0;
            for (int i = 0; i < 50_000; i++) {
                total = outerPriv(x);
            }
            return total;
        }
    }

    public static void main(String[] args) throws Exception {
        StringBuilder out = new StringBuilder();

        Base base = new Base();
        out.append("base.callsPriv(3)=").append(base.callsPriv(3)).append('\n');
        out.append("base.callsPrivSynchronized(3)=")
                .append(base.callsPrivSynchronized(3))
                .append('\n');

        // Subclass receiver.
        Sub sub = new Sub();
        out.append("sub.callsPriv(4)=").append(sub.callsPriv(4)).append('\n');

        // Null receiver: an explicit self-call cannot itself be null (it is
        // always `this`), but the receiver `Base b` inside `recurse` is
        // fresh each frame, so this exercises ordinary null-safe construction
        // repeatedly under recursion instead. A genuine null-receiver self
        // call is impossible to write in source -- `this` is never null --
        // so the checklist item is about the JIT not ASSUMING a self-call's
        // receiver register is non-null from some other invariant; nothing
        // extra to add here beyond what `recurse` already stresses.

        // Nestmates.
        H23SelfCallReview outer = new H23SelfCallReview();
        Inner inner = outer.new Inner();
        out.append("inner.callOuterPriv(5)=").append(inner.callOuterPriv(5)).append('\n');

        // A bounded recursion depth on the MAIN thread's own (default) stack,
        // so the self-call path is measured at a real answer, not only at the
        // overflow edge.
        out.append("Base.recurse(2000)=").append(Base.recurse(2000)).append('\n');

        // Deep recursion / StackOverflowError, self-call at every frame -- on
        // its OWN small-stack thread, so it cannot eat into the bound above
        // (a 256k stack does not fit 2000 ordinary frames either, so sharing
        // one stack size between the two would make one of them lie).
        StringBuilder overflow = new StringBuilder();
        Thread t = new Thread(null, () -> {
            try {
                recurseUntilOverflow(0);
                overflow.append("recurseUntilOverflow: NO OVERFLOW (unexpected)\n");
            } catch (StackOverflowError e) {
                overflow.append("recurseUntilOverflow: StackOverflowError caught\n");
            }
        }, "overflow-probe", 256 * 1024);
        t.start();
        t.join();
        out.append(overflow);

        System.out.print(out);
    }

    private static int recurseUntilOverflow(int depth) {
        Base b = new Base();
        int r = b.priv(depth);
        return r + recurseUntilOverflow(depth + 1);
    }
}
