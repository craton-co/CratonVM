import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * Shape of `BindConverter.convert`: an enhanced-for over a field-held List with
 * a try/catch INSIDE the loop body. The iterator lives in a compiler-generated
 * local that is defined before the protected range, never redefined, and read
 * again at the loop head — which the handler's fall-through branches back to.
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
 *
 * The real callee is an INTERFACE call whose exception is raised several frames
 * down, so it escapes a compiled callee rather than being inlined into the
 * caller — that is the path that resumes AT the handler from a frame snapshot.
 */
public class LoopHandlerIteratorLocalProbe {

    /** Stand-in for ConversionException — the declared catch type. */
    static final class ConvEx extends RuntimeException {

        ConvEx(String m) {
            super(m);
        }
    }

    /** Stand-in for ConversionService: the delegates are reached by invokeinterface. */
    interface Svc {

        boolean canConvert(String from, String to);

        String convert(String value, String from, String to);
    }

    /** Refuses everything, from a few frames down, with enough body to resist inlining. */
    static final class Refusing implements Svc {

        private final int id;

        private long spin;

        Refusing(int id) {
            this.id = id;
        }

        @Override
        public boolean canConvert(String from, String to) {
            return from.length() + to.length() + this.id >= 0;
        }

        @Override
        public String convert(String value, String from, String to) {
            return level1(value, from, to);
        }

        private String level1(String value, String from, String to) {
            this.spin += value.length();
            return level2(value, from, to);
        }

        private String level2(String value, String from, String to) {
            this.spin += from.length() * 3L + to.length();
            if (this.spin % 7 == 0) {
                this.spin++;
            }
            return level3(value, from, to);
        }

        private String level3(String value, String from, String to) {
            throw new ConvEx("refused " + value + " " + from + "->" + to + " by " + this.id);
        }
    }

    /** Answers. */
    static final class Answering implements Svc {

        @Override
        public boolean canConvert(String from, String to) {
            return true;
        }

        @Override
        public String convert(String value, String from, String to) {
            return "converted";
        }
    }

    static final class Holder {

        private final List<Svc> delegates;

        Holder(List<Svc> ds) {
            this.delegates = Collections.unmodifiableList(new ArrayList<>(ds));
        }

        /** The shape of BindConverter.convert, opcode for opcode. */
        String convert(String value, String from, String to) {
            ConvEx failure = null;
            for (Svc d : this.delegates) {
                try {
                    if (d.canConvert(from, to)) {
                        return d.convert(value, from, to);
                    }
                }
                catch (ConvEx ex) {
                    if (failure == null) {
                        failure = ex;
                    }
                }
            }
            if (value == null) {
                return null;
            }
            if (failure != null) {
                return "failed:" + failure.getMessage();
            }
            return null;
        }
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;

        List<Svc> ds = new ArrayList<>();
        // The first two throw from three frames down, so the handler runs twice
        // per call and the loop head is re-entered from it both times.
        ds.add(new Refusing(1));
        ds.add(new Refusing(2));
        ds.add(new Answering());
        Holder holder = new Holder(ds);

        int wrong = 0;
        int thrown = 0;
        long firstBadAt = -1;
        String firstError = null;
        for (int i = 0; i < iterations; i++) {
            try {
                String r = holder.convert("v" + (i & 7), "java.lang.String", "long");
                if (!"converted".equals(r)) {
                    wrong++;
                    if (firstBadAt < 0) {
                        firstBadAt = i;
                        firstError = "wrong result: " + r;
                    }
                }
            }
            catch (Throwable ex) {
                thrown++;
                if (firstBadAt < 0) {
                    firstBadAt = i;
                    firstError = ex.getClass().getName() + ": " + ex.getMessage();
                }
            }
        }

        System.out.println("iterations   = " + iterations);
        System.out.println("wrong results= " + wrong);
        System.out.println("escaped throw= " + thrown);
        System.out.println("first bad at = " + firstBadAt);
        System.out.println("first error  = " + firstError);
        boolean ok = wrong == 0 && thrown == 0;
        System.out.println(ok ? "PROBE PASS" : "PROBE FAIL");
        System.exit(ok ? 0 : 1);
    }
}
