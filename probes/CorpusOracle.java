import java.io.BufferedReader;
import java.io.FileReader;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

/**
 * Ask a REAL JDK what every listed corpus fixture method does — the oracle the
 * interpreter corpus is measured against.
 *
 * Reads a TSV of `test`, `class`, `method` columns and prints one result line
 * per pair: `RETURNED v` / `THREW cls: msg` / `TIMEOUT` / `RESOLVE-FAILED ...`.
 * Each call runs on its own daemon thread with a watchdog so a fixture that
 * blocks does not stall the sweep.
 *
 * This exists because comparing CratonVM-synthetic against CratonVM-real
 * answers the wrong question. It says which of the two CratonVM modes differ,
 * not which one is right — and a fixture whose *expectation* is wrong looks
 * like a VM bug in both. Run against a real `java` it separates "the VM is
 * missing something" from "the test asserts something the JDK never promised".
 * Of 214 corpus failures triaged this way on 2026-08-02, 191 had correct
 * expectations, 20 could not be judged by HotSpot at all (they need CratonVM's
 * own natives — TckJdbc's SQLite driver, VirtualThreadTest's fixture-declared
 * helpers), and 3 expectations were simply wrong.
 *
 * Compile the fixtures with a real javac first, then:
 *
 *   javac -d classes vm/tests/resources/cratonvm/(all).java probes/CorpusOracle.java
 *   java -cp classes CorpusOracle pairs.tsv 8000
 */
public final class CorpusOracle {
    public static void main(String[] args) throws Exception {
        long timeoutMs = args.length > 1 ? Long.parseLong(args[1]) : 15000;
        try (BufferedReader r = new BufferedReader(new FileReader(args[0]))) {
            String line;
            while ((line = r.readLine()) != null) {
                if (line.isBlank()) continue;
                String[] parts = line.split("\t");
                String test = parts[0], cls = parts[1].replace('/', '.'), meth = parts[2];
                System.out.println(test + "\t" + cls + "\t" + meth + "\t" + run(cls, meth, timeoutMs));
                System.out.flush();
            }
        }
    }

    private static String run(String cls, String meth, long timeoutMs) {
        final String[] out = {"NO-RESULT"};
        Runnable body = () -> {
            Method m;
            try {
                Class<?> t = Class.forName(cls);
                m = t.getDeclaredMethod(meth);
            } catch (Throwable resolution) {
                out[0] = "RESOLVE-FAILED " + resolution;
                return;
            }
            try {
                m.setAccessible(true);
                Object recv = null;
                if (!Modifier.isStatic(m.getModifiers())) {
                    try {
                        recv = m.getDeclaringClass().getDeclaredConstructor().newInstance();
                    } catch (Throwable ctor) {
                        out[0] = "NO-INSTANCE " + ctor;
                        return;
                    }
                }
                Object result = m.invoke(recv);
                out[0] = "RETURNED " + result;
            } catch (InvocationTargetException wrapped) {
                Throwable cause = wrapped.getCause();
                out[0] = "THREW " + (cause == null ? "<null>"
                        : cause.getClass().getName() + ": " + cause.getMessage());
            } catch (Throwable other) {
                out[0] = "INVOKE-FAILED " + other;
            }
        };
        Thread t = new Thread(body, "oracle");
        t.setDaemon(true);
        t.start();
        try {
            t.join(timeoutMs);
        } catch (InterruptedException ie) {
            return "INTERRUPTED";
        }
        if (t.isAlive()) return "TIMEOUT";
        return out[0];
    }
}
