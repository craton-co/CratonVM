/**
 * Regression probe for the shape that made
 * `BatchObservabilityBeanPostProcessor.postProcessAfterInitialization` return
 * `this` instead of the bean it was handed
 * (fixed-suite-bugs/springboot/springboot-3gc-fails-and-hangs-20260821-RETIRED.md).
 *
 * When a compiled callee lets an exception escape, the caller's site services
 * the `i64::MIN` sentinel through `jit_service_callee_deopt`, which rebuilds
 * the callee's frame from `num_args` CONSECUTIVE slots at the pointer it is
 * handed and resumes the callee's own catch block. The shared hashed/vtable
 * MEGAMORPHIC stub was handing it `arg_offsets[0]` — correct for the
 * single-pass backend, whose arguments are one descending block, and wrong for
 * the IR lowerer, whose `arg_offsets` are each argument's own
 * register-allocated home slot. The helper then read neighbouring frame words
 * as arguments 1..n, and the resumed handler saw `this` in local 1.
 *
 * `HandlerLocalAcrossProtectedInvokeProbe` does not cover this: it drives at
 * most two receiver types at a site, so it never gets past the monomorphic
 * cache and the four-way PIC into the megamorphic stub this defect lives in.
 * **Eight receiver classes per site is the load-bearing part of this probe** —
 * with fewer, it passes on the broken build.
 *
 * The callee shape mirrors the original: an INSTANCE method whose first
 * argument is the value it must hand back, a `try` region whose only call
 * always throws, a handler that stores the throwable into a local the try body
 * used for something else, and a `return` of argument 1 after the join.
 */
public final class MegamorphicCalleeDeoptArgsProbe {

    static final class Missing extends RuntimeException {
        Missing(String m) { super(m); }
    }

    /** Stands in for the registry lookup that always misses. */
    interface Registry { Object lookup(Class<?> type); }

    static final class EmptyRegistry implements Registry {
        private final String id;
        EmptyRegistry(String id) { this.id = id; }
        @Override public Object lookup(Class<?> type) {
            throw new Missing("no " + type.getName() + " in " + this.id);
        }
    }

    /** The pass-through post-processor. Every path returns `value`. */
    interface PostProcessor { Object process(Object value, String name); }

    abstract static class AbstractPost implements PostProcessor {
        Registry registry;
        static Object sink;

        @Override public Object process(Object value, String name) {
            if (this.registry == null) {
                sink = name;
                return value;
            }
            try {
                if (value instanceof Tagged || value instanceof Other || value instanceof Third) {
                    Object found = this.registry.lookup(Marker.class);
                    if (value instanceof Tagged) { ((Tagged) value).mark(found); }
                    if (value instanceof Other) { ((Other) value).mark(found); }
                    if (value instanceof Third) { ((Third) value).mark(found); }
                }
            }
            catch (Missing e) {
                sink = e;
            }
            return value;
        }
    }

    // Eight distinct receiver classes for the one `process` call site below.
    // Under six, the site never leaves the MIC + four-way PIC and the defect
    // this probe exists for is not reachable.
    static final class P0 extends AbstractPost { }
    static final class P1 extends AbstractPost { }
    static final class P2 extends AbstractPost { }
    static final class P3 extends AbstractPost { }
    static final class P4 extends AbstractPost { }
    static final class P5 extends AbstractPost { }
    static final class P6 extends AbstractPost { }
    static final class P7 extends AbstractPost { }

    static final class Marker { }
    static final class Tagged { Object m; void mark(Object o) { this.m = o; } }
    static final class Other  { Object m; void mark(Object o) { this.m = o; } }
    static final class Third  { Object m; void mark(Object o) { this.m = o; } }
    static final class Plain  { }

    /** The megamorphic call site. */
    private static Object drive(PostProcessor pp, Object bean, String name) {
        return pp.process(bean, name);
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;

        PostProcessor[] posts = {
            new P0(), new P1(), new P2(), new P3(), new P4(), new P5(), new P6(), new P7(),
        };
        for (int i = 0; i < posts.length; i++) {
            ((AbstractPost) posts[i]).registry = new EmptyRegistry("r" + i);
        }

        // A mix of beans that enter the protected region and beans that skip
        // it, so both the throwing and non-throwing paths stay live.
        Object[] beans = {
            new Tagged(), new Plain(), new Other(), new Plain(),
            new Third(), new Plain(), new Tagged(), new Other(),
        };

        long wrong = 0, wrongIsProcessor = 0, wrongIsNull = 0;
        String first = null;

        for (int i = 0; i < iterations; i++) {
            PostProcessor pp = posts[i % posts.length];
            Object bean = beans[i % beans.length];
            Object out = drive(pp, bean, "bean" + (i % beans.length));
            if (out != bean) {
                wrong++;
                if (out == pp) { wrongIsProcessor++; }
                if (out == null) { wrongIsNull++; }
                if (first == null) {
                    first = "iter=" + i
                          + " in=" + bean.getClass().getSimpleName() + "@" + System.identityHashCode(bean)
                          + " out=" + (out == null ? "null"
                                : out.getClass().getSimpleName() + "@" + System.identityHashCode(out))
                          + " outIsProcessor=" + (out == pp);
                }
            }
        }

        System.out.println("MEGAMORPHIC_CALLEE_DEOPT_ARGS wrong=" + wrong
                + " wrongIsProcessor=" + wrongIsProcessor
                + " wrongIsNull=" + wrongIsNull
                + (first == null ? "" : " first=[" + first + "]"));
        System.exit(wrong == 0 ? 0 : 1);
    }
}
