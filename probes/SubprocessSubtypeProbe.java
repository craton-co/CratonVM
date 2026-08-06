import java.util.List;

/**
 * Is the object `ProcessBuilder.start()` returns actually a
 * `java.lang.Process`, by every test the platform offers?
 *
 * CratonVM returns `cratonvm.synthetic.Process`, a VM-minted class whose
 * `getSuperclass()` is `java.lang.Object`. Six of the seven questions below
 * still answer exactly as HotSpot does, because the VM's own subtype machinery
 * knows about the relation. The seventh — walking the class's own superclass
 * chain — does not, and that disagreement is the finding: `isAssignableFrom`
 * says yes while the reflective hierarchy says no, in one VM, about one pair of
 * classes.
 *
 * It matters because hand-rolled hierarchy walks are everywhere: serialization
 * frameworks, DI containers, matchers, mock frameworks. Code that asks `Class`
 * gets the right answer; code that walks `getSuperclass()` gets the opposite.
 *
 * See `docs/known-issues/jdk-only/synthetic-process-cluster-and-the-supertype-lie.md`.
 *
 *   javac -d out probes/SubprocessSubtypeProbe.java
 *   java  -cp out SubprocessSubtypeProbe                  # control
 *   cratonvm --real-jdk --java-home $JDK -cp out SubprocessSubtypeProbe
 */
public class SubprocessSubtypeProbe {

    static String safe(java.util.function.Supplier<String> f) {
        try {
            return f.get();
        } catch (Throwable t) {
            return "EXC:" + t.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        Process p = new ProcessBuilder("/bin/echo", "x").start();
        p.waitFor();
        Object o = p;

        // --- the six the VM answers for itself ---------------------------
        System.out.println("instanceof.Process="
                + safe(() -> String.valueOf(o instanceof Process)));
        System.out.println("cast.Process=" + safe(() -> {
            Process q = (Process) o;
            return q == o ? "ok" : "ok-different";
        }));
        System.out.println("Process.class.isInstance="
                + safe(() -> String.valueOf(Process.class.isInstance(o))));
        System.out.println("isAssignableFrom="
                + safe(() -> String.valueOf(Process.class.isAssignableFrom(o.getClass()))));
        System.out.println("listOfProcess=" + safe(() -> {
            List<Process> l = List.of(p);
            return String.valueOf(l.size());
        }));
        System.out.println("arrayStore=" + safe(() -> {
            Process[] arr = new Process[1];
            arr[0] = p;
            return "ok";
        }));

        // --- the seventh: the class's own account of its ancestry ---------
        StringBuilder chain = new StringBuilder();
        for (Class<?> c = p.getClass(); c != null; c = c.getSuperclass()) {
            chain.append(c.getName()).append(" -> ");
        }
        System.out.println("chain=" + chain + "null");

        boolean inChain = false;
        for (Class<?> c = p.getClass(); c != null; c = c.getSuperclass()) {
            if (c == Process.class) {
                inChain = true;
                break;
            }
        }
        System.out.println("ProcessInSuperclassChain=" + inChain);

        // The assertion. These two are asking the same question and must agree.
        boolean assignable = Process.class.isAssignableFrom(p.getClass());
        System.out.println("CONSISTENT=" + (inChain == assignable));
    }
}
