/*
 * CalleeHandlerRoutingProbe - does a hot, handler-bearing INSTANCE callee still
 * catch its own implicit exception once the call site tiers up?
 *
 * This is the invariant behind `jit_virtual_tierup`. A direct compiled entry has
 * no interpreter boundary at which the callee's own exception table can be
 * resumed, so if a handler-bearing callee is promoted to one, an implicit
 * NPE/AIOOBE raised inside it bails out through the CALLER's epilogue instead of
 * reaching the callee's `catch`. On an embedded server that surfaces as a
 * request that never completes, which is why `c28bdd687` turned instance
 * tier-up off wholesale.
 *
 * The shape that matters:
 *   - the callee is an INSTANCE method (reached by invokevirtual),
 *   - it DECLARES an exception table,
 *   - it is called often enough to cross the tier-up threshold,
 *   - and only THEN does it take the implicit exception.
 *
 * A correct VM returns the catch's sentinel every time. A VM that promoted the
 * callee to a direct entry either loses the catch (the exception escapes to
 * main) or strands the frame (no output at all).
 *
 * Run it both ways and diff:
 *   CRATONVM_JIT_VIRTUAL_TIERUP=1 cratonvm -cp <dir> CalleeHandlerRoutingProbe
 *   CRATONVM_JIT_VIRTUAL_TIERUP=0 cratonvm -cp <dir> CalleeHandlerRoutingProbe
 * and against HotSpot. All three must agree line for line.
 */
public class CalleeHandlerRoutingProbe {

    /** Handler-bearing instance callee: the thing that must stay resumable. */
    static final class Target {
        int viaArray(int[] a, int i) {
            try {
                return a[i];
            } catch (NullPointerException e) {
                return -1;
            } catch (ArrayIndexOutOfBoundsException e) {
                return -2;
            }
        }

        int viaField(Holder h) {
            try {
                return h.value;
            } catch (NullPointerException e) {
                return -3;
            }
        }

        /** Nested: the implicit throw happens one frame deeper than the catch. */
        int viaNested(int[] a, int i) {
            try {
                return unguarded(a, i);
            } catch (NullPointerException | ArrayIndexOutOfBoundsException e) {
                return -4;
            }
        }

        private int unguarded(int[] a, int i) {
            return a[i];
        }
    }

    static final class Holder {
        int value;

        Holder(int v) {
            this.value = v;
        }
    }

    public static void main(String[] args) {
        int warm = args.length > 0 ? Integer.parseInt(args[0]) : 200000;

        Target t = new Target();
        int[] ok = new int[] { 7, 8, 9 };
        Holder h = new Holder(42);

        // Phase 1: drive every callee well past the tier-up threshold on the
        // NON-throwing path, so the call sites are promoted before any
        // exception is raised. Promotion-then-throw is the ordering that breaks.
        long acc = 0;
        for (int i = 0; i < warm; i++) {
            acc += t.viaArray(ok, i % 3);
            acc += t.viaField(h);
            acc += t.viaNested(ok, i % 3);
        }
        System.out.println("warm acc=" + acc);

        // Phase 2: now take the implicit exceptions. Each must be caught by the
        // CALLEE's own handler and produce its sentinel.
        System.out.println("null array      -> " + t.viaArray(null, 0));
        System.out.println("index oob       -> " + t.viaArray(ok, 99));
        System.out.println("null field      -> " + t.viaField(null));
        System.out.println("nested null     -> " + t.viaNested(null, 0));
        System.out.println("nested oob      -> " + t.viaNested(ok, 99));

        // Phase 3: the callees must still work afterwards — a mishandled
        // exceptional return can leave the call site or frame poisoned.
        System.out.println("after: array    -> " + t.viaArray(ok, 1));
        System.out.println("after: field    -> " + t.viaField(h));
        System.out.println("after: nested   -> " + t.viaNested(ok, 2));
        System.out.println("DONE");
    }
}
