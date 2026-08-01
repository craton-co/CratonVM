import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

// Regression probe: a compiled callee's exception handler must resume on the
// frame the compiled body published, not on one rebuilt from `this` plus the
// declared parameters.
//
// `run_jit_callee_handler` used to rebuild the handler frame that way and never
// consumed the reason-9 exceptional frame, so every NON-parameter local came
// back 0/null. Fixed on `dev` by fix/liquibase-scope-20260801 (063be4f18).
//
// This probe is the shape that exposes it from the caller side: an
// exception-table callee entered from a compiled caller through the virtual-MIC
// helper's cached raw compiled entry (added by b96731855, which is why the
// failure window opens there). Measured 2026-08-01 over 200_000 calls:
//
//     dev, before the fix                             199_424 wrong
//     dev + CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE=1          0
//     binary predating b96731855                            0
//
// It runs correctly for a few hundred calls and then fails permanently, so a
// short run proves nothing — keep the iteration count in the hundreds of
// thousands.
//
// Bytecode-shape model of the method that is actually miscompiled:
//
//   private Object BindConverter.convert(Object source, TypeDescriptor sourceType,
//                                        TypeDescriptor targetType) {
//       ConversionException failure = null;                       // astore 4
//       for (ConversionService delegate : this.delegates) {       // iterator -> astore 5, aload 5
//           try {                                                 // exception table 36..58 -> 62
//               if (delegate.canConvert(sourceType, targetType)) {
//                   return delegate.convert(source, sourceType, targetType);
//               }
//           }
//           catch (ConversionException ex) {                      // astore 7
//               if (failure == null && ex instanceof ConversionFailedException) {
//                   failure = ex;
//               }
//           }
//       }
//       if (source == null) return null;
//       throw (failure != null) ? failure : new ConverterNotFoundException(...);
//   }
//
// Reported as
//   NullPointerException: Cannot invoke "java.util.Iterator.hasNext()"
//                        because "<local5>" is null
// i.e. `this.delegates.iterator()` handed back null. `delegates` is
// `Collections.unmodifiableList(new ArrayList<>())`, which on CratonVM is a
// synthetic `cratonvm/internal/UnmodifiableList` whose `iterator()` is a native
// that reads the backing collection out of field 0 — so a wrong receiver makes
// it answer null rather than throw.
//
// Usage: HandlerCalleeEntryCacheProbe [iterations]
public class HandlerCalleeEntryCacheProbe {

    static class ConvException extends RuntimeException {
        ConvException(String m) { super(m); }
    }

    static final class ConvFailed extends ConvException {
        ConvFailed(String m) { super(m); }
    }

    interface Service {
        boolean canConvert(Object from, Object to);
        Object convert(Object source, Object from, Object to);
    }

    static final class Declines implements Service {
        public boolean canConvert(Object from, Object to) { return false; }
        public Object convert(Object s, Object f, Object t) { throw new ConvFailed("no"); }
    }

    static final class Throws implements Service {
        public boolean canConvert(Object from, Object to) { return true; }
        public Object convert(Object s, Object f, Object t) { throw new ConvFailed("boom"); }
    }

    static final class Accepts implements Service {
        public boolean canConvert(Object from, Object to) { return to == Boolean.TRUE; }
        public Object convert(Object s, Object f, Object t) { return s; }
    }

    private final List<Service> delegates;

    HandlerCalleeEntryCacheProbe() {
        List<Service> d = new ArrayList<>();
        d.add(new Declines());
        d.add(new Throws());
        d.add(new Accepts());
        this.delegates = Collections.unmodifiableList(d);
    }

    // Exactly the shape above: 3 reference params + receiver = 4 incoming slots,
    // an exception table around the loop body, an areturn from inside the loop.
    private Object convert(Object source, Object sourceType, Object targetType) {
        ConvException failure = null;
        for (Service delegate : this.delegates) {
            try {
                if (delegate.canConvert(sourceType, targetType)) {
                    return delegate.convert(source, sourceType, targetType);
                }
            } catch (ConvException ex) {
                if (failure == null && ex instanceof ConvFailed) {
                    failure = ex;
                }
            }
        }
        if (source == null) {
            return null;
        }
        throw (failure != null) ? failure : new ConvException("not found");
    }

    Object convert(Object source, Object targetType) {
        return convert(source, "srcType", targetType);
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;

        HandlerCalleeEntryCacheProbe p = new HandlerCalleeEntryCacheProbe();
        long ok = 0, bad = 0, threw = 0;
        int firstBad = -1;
        for (int i = 0; i < iters; i++) {
            Object src = "s" + i;
            try {
                Object r = p.convert(src, Boolean.TRUE);
                if (r != src) {
                    if (bad++ == 0) {
                        firstBad = i;
                        System.out.println("WRONG first at i=" + i + " got=" + r + " want=" + src);
                    }
                } else {
                    ok++;
                }
            } catch (Throwable t) {
                if (threw++ == 0) {
                    firstBad = i;
                    System.out.println("THREW first at i=" + i + ": " + t);
                    StackTraceElement[] st = t.getStackTrace();
                    for (int k = 0; k < Math.min(4, st.length); k++) {
                        System.out.println("    at " + st[k]);
                    }
                }
            }
            // A fresh instance now and then, mirroring the per-context binder.
            if ((i & 1023) == 0) {
                p = new HandlerCalleeEntryCacheProbe();
            }
        }
        System.out.println("HandlerCalleeEntryCacheProbe iters=" + iters + " ok=" + ok
                + " bad=" + bad + " threw=" + threw + " firstBad=" + firstBad);
        System.exit((bad + threw) == 0 ? 0 : 1);
    }
}
