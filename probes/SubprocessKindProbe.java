import java.io.BufferedReader;
import java.io.InputStreamReader;

/**
 * What class does `ProcessBuilder.start()` hand back, and does the subprocess
 * surface work, in each mode?
 *
 * `l5-native-io-bridge-residuals.md`'s largest open item is 37 `Bridge`
 * registrations whose receiver class is `cratonvm/synthetic/Process` and friends
 * — classes `--jdk-only` contract §5 forbids fabricating. `Bridge` is the tag
 * that keeps a native alive under `--jdk-only`, so the record's question is what
 * those registrations mean when the receiver cannot legally exist.
 *
 * That is answerable by running it. This prints the concrete class of the object
 * `start()` returns, plus enough of the surface to show whether it works, so the
 * two modes can be compared directly:
 *
 *   cratonvm --real-jdk  --java-home $JDK -cp out SubprocessKindProbe
 *   cratonvm --jdk-only  --java-home $JDK -cp out SubprocessKindProbe
 *
 * A `cratonvm.synthetic.Process` under `--real-jdk` and a refusal (or a real
 * `java.lang.ProcessImpl`) under `--jdk-only` would mean the 37 rows are
 * unreachable in strict mode and the `Bridge` tag is buying nothing there. The
 * same class in both modes would mean strict mode is fabricating a class §5
 * forbids, which is the contradiction the record names.
 */
public class SubprocessKindProbe {
    public static void main(String[] args) {
        try {
            ProcessBuilder pb = new ProcessBuilder("/bin/echo", "hello-from-subprocess");
            pb.redirectErrorStream(true);
            Process p = pb.start();
            System.out.println("processClass=" + p.getClass().getName());
            System.out.println("processSuper=" + p.getClass().getSuperclass().getName());
            try (BufferedReader r = new BufferedReader(
                    new InputStreamReader(p.getInputStream()))) {
                System.out.println("stdout=" + r.readLine());
            }
            int code = p.waitFor();
            System.out.println("exit=" + code);
            System.out.println("isAliveAfter=" + p.isAlive());
            System.out.println("VERDICT=subprocess-worked");
        } catch (Throwable t) {
            // A policy refusal is a result, not a crash — name it exactly.
            System.out.println("VERDICT=threw " + t.getClass().getName()
                    + ": " + t.getMessage());
        }
    }
}
