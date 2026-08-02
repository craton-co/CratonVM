import java.util.function.Supplier;

/**
 * Drives the dispatch shape behind a signature recorded on 2026-07-31 while
 * `BasicErrorControllerIntegrationTests` was failing under JIT:
 *
 * <pre>
 * NoSuchMethodError method="java/lang/Class.getLog(Ljava/util/function/Supplier;)Lorg/apache/commons/logging/Log;"
 *                   caller="org/springframework/boot/logging/DeferredLogFactory.getLog(Ljava/lang/Class;)... @pc=12"
 * </pre>
 *
 * `DeferredLogFactory` is an interface whose `default Log getLog(Class<?> c)`
 * calls its OWN abstract overload `getLog(Supplier<Log>)`, passing a lambda that
 * captures the `Class` argument. The VM resolved that call against
 * `java/lang/Class` — the class of the ARGUMENT, not of the receiver.
 *
 * This probe reproduces the shape exactly: an interface with a `default` method
 * that forwards to a same-named abstract overload of itself, called hot enough
 * to be compiled, with a captured `Class` argument and a `Class`-typed
 * parameter. Every call checks that the receiver the callee actually saw is the
 * implementation object and not the argument.
 *
 * Prints `SELF_OVERLOAD_RECEIVER_PROBE_PASS` / `..._FAIL n`. Exit code 0/1.
 *
 * Usage: SelfOverloadReceiverProbe [iterations]
 */
public final class SelfOverloadReceiverProbe {

    interface Factory {
        /** The overload the `default` below forwards to. */
        String make(Supplier<String> source);

        /** Same shape as `DeferredLogFactory.getLog(Class)`. */
        default String make(Class<?> destination) {
            return make(() -> destination.getName());
        }

        /** A second `Class`-argument overload, to keep the by-name pick honest. */
        default String makeTwice(Class<?> destination) {
            return make(destination) + "|" + make(destination);
        }
    }

    static final class RealFactory implements Factory {
        final String tag;
        int calls;

        RealFactory(String tag) {
            this.tag = tag;
        }

        @Override
        public String make(Supplier<String> source) {
            // `this.tag` is the whole point: reached through the wrong
            // receiver, this either throws or reads a foreign object.
            this.calls++;
            return this.tag + ":" + source.get();
        }
    }

    /** A second implementation so the call site is not monomorphic. */
    static final class OtherFactory implements Factory {
        @Override
        public String make(Supplier<String> source) {
            return "other:" + source.get();
        }
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        Factory real = new RealFactory("real");
        Factory other = new OtherFactory();
        Class<?>[] classes = {
            String.class, Integer.class, java.util.List.class, Thread.class, Object.class,
        };

        int bad = 0;
        for (int i = 0; i < iterations; i++) {
            Class<?> c = classes[i % classes.length];
            Factory f = (i % 7 == 0) ? other : real;
            String want = ((i % 7 == 0) ? "other:" : "real:") + c.getName();
            String got;
            try {
                got = f.make(c);
            } catch (Throwable t) {
                if (bad < 5) {
                    System.out.println("throw at i=" + i + ": " + t);
                }
                bad++;
                continue;
            }
            if (!want.equals(got)) {
                if (bad < 5) {
                    System.out.println("mismatch at i=" + i + " want=" + want + " got=" + got);
                }
                bad++;
            }
            String wantTwice = want + "|" + want;
            String gotTwice;
            try {
                gotTwice = f.makeTwice(c);
            } catch (Throwable t) {
                if (bad < 5) {
                    System.out.println("throw (twice) at i=" + i + ": " + t);
                }
                bad++;
                continue;
            }
            if (!wantTwice.equals(gotTwice)) {
                if (bad < 5) {
                    System.out.println("mismatch (twice) at i=" + i);
                }
                bad++;
            }
        }
        if (bad == 0) {
            System.out.println("SELF_OVERLOAD_RECEIVER_PROBE_PASS iterations=" + iterations);
        } else {
            System.out.println("SELF_OVERLOAD_RECEIVER_PROBE_FAIL " + bad);
            System.exit(1);
        }
    }
}
