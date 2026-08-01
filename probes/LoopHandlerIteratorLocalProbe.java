import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * Shape of `BindConverter.convert`: an enhanced-for over a field-held List with
 * a try/catch INSIDE the loop body. The iterator lives in a local that is
 * defined before the protected range, never redefined, and read again at the
 * loop head — which is reached from the handler's fall-through.
 *
 * Under JIT the iterator local came back null after the handler ran, so the
 * loop head NPE'd on `Iterator.hasNext()`:
 *
 *   java.lang.NullPointerException: Cannot invoke "java.util.Iterator.hasNext()"
 *   because "<local5>" is null
 *       at ...bind.BindConverter.convert(BindConverter.java:108)
 *
 * DevToolsPooledDataSourceAutoConfigurationTests.inMemoryDerbyIsShutdown hit it
 * once the class had run enough tests to compile `convert` AND a delegate threw
 * (binding `spring.datasource.hikari.validation-timeout` to long). `--nojit`
 * and `CRATONVM_JIT_BISECT_SKIP=...BindConverter.convert` both pass.
 */
public class LoopHandlerIteratorLocalProbe {

    static final class Delegate {

        final boolean throwing;

        final String answer;

        Delegate(boolean throwing, String answer) {
            this.throwing = throwing;
            this.answer = answer;
        }

        String apply(String in) {
            if (this.throwing) {
                throw new IllegalStateException("delegate refuses " + in);
            }
            return this.answer;
        }
    }

    static final class Holder {

        private final List<Delegate> delegates;

        Holder(List<Delegate> ds) {
            this.delegates = Collections.unmodifiableList(new ArrayList<>(ds));
        }

        /**
         * Byte-for-byte the shape of BindConverter.convert: iterator local
         * defined at the loop head, try/catch inside the body, handler falls
         * through to the back edge.
         */
        String convert(String in) {
            RuntimeException failure = null;
            for (Delegate d : this.delegates) {
                try {
                    String r = d.apply(in);
                    if (r != null) {
                        return r;
                    }
                }
                catch (RuntimeException ex) {
                    if (failure == null) {
                        failure = ex;
                    }
                }
            }
            if (failure != null) {
                return "failed:" + failure.getMessage();
            }
            return null;
        }
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;

        List<Delegate> ds = new ArrayList<>();
        // First two throw, the third answers — so the handler runs twice per
        // call and the loop head is re-entered from the handler both times.
        ds.add(new Delegate(true, null));
        ds.add(new Delegate(true, null));
        ds.add(new Delegate(false, "converted"));
        Holder holder = new Holder(ds);

        int wrong = 0;
        int thrown = 0;
        long firstBadAt = -1;
        String lastError = null;
        for (int i = 0; i < iterations; i++) {
            try {
                String r = holder.convert("v" + (i & 7));
                if (!"converted".equals(r)) {
                    wrong++;
                    if (firstBadAt < 0) {
                        firstBadAt = i;
                        lastError = "wrong result: " + r;
                    }
                }
            }
            catch (Throwable ex) {
                thrown++;
                if (firstBadAt < 0) {
                    firstBadAt = i;
                    lastError = ex.getClass().getName() + ": " + ex.getMessage();
                }
            }
        }

        System.out.println("iterations   = " + iterations);
        System.out.println("wrong results= " + wrong);
        System.out.println("escaped throw= " + thrown);
        System.out.println("first bad at = " + firstBadAt);
        System.out.println("first error  = " + lastError);
        boolean ok = wrong == 0 && thrown == 0;
        System.out.println(ok ? "PROBE PASS" : "PROBE FAIL");
        System.exit(ok ? 0 : 1);
    }
}
