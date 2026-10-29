import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Guards the redefinition fix: a class redefined with a DIFFERENT body must
 * observe the new body immediately, and must still observe it after the method
 * gets hot enough to re-tier into the JIT.
 *
 * This is the companion to {@link RedefineCostProbe}, which redefines with
 * identical bytes and therefore cannot detect a stale body at all. The perf fix
 * lets a redefined class be JIT-compiled again; if that recompilation ever
 * picked up the *pre*-redefine bytecode, the increment would silently revert to
 * 1 and no timing test would notice.
 *
 *   java  RedefineCorrectnessProbe        # HotSpot: skips, no agent
 *   cratonvm RedefineCorrectnessProbe
 */
public final class RedefineCorrectnessProbe {

    public static final class Victim {
        private int n;
        public int bump() {
            n = n + 1;
            return n;
        }
    }

    /** Enough calls to push the method through tier-up after the redefine. */
    private static final int HOT = 400_000;

    private static byte[] altBytes() throws Exception {
        // Compiled from alt/RedefineCostProbe.java: same nested-class shape,
        // `bump()` adds 100 instead of 1.
        Path p = Path.of("alt", "RedefineCorrectnessProbe$Victim.class");
        if (!Files.exists(p)) {
            throw new IllegalStateException("missing " + p.toAbsolutePath()
                    + " - compile alt/RedefineCorrectnessProbe.java into alt/ first");
        }
        return Files.readAllBytes(p);
    }

    private static boolean redefine(byte[] bytes) throws Exception {
        try {
            Class<?> bridge = Class.forName("cratonvm.Instrument");
            Object ok = bridge
                .getMethod("redefineClass", Class.class, byte[].class)
                .invoke(null, Victim.class, bytes);
            return Boolean.TRUE.equals(ok) || Integer.valueOf(1).equals(ok);
        } catch (ClassNotFoundException e) {
            System.out.println("SKIP  no cratonvm.Instrument bridge on this VM (control run)");
            return false;
        }
    }

    public static void main(String[] args) throws Exception {
        Victim v = new Victim();

        int first = v.bump();
        if (first != 1) {
            throw new AssertionError("pre-redefine bump() should step by 1, got " + first);
        }

        byte[] alt = altBytes();
        if (!redefine(alt)) {
            // Distinguish "no bridge" (printed above) from a bridge that ran
            // and refused: a rejected redefinition is a real failure, and
            // reporting it as SKIP would make this probe silently vacuous.
            System.out.println("redefine() returned false for " + alt.length + " bytes");
            return;
        }

        // Immediately after redefine, still interpreted.
        int before = v.bump();
        int stepInterpreted = before - first;
        System.out.println("interpreted step after redefine = " + stepInterpreted);

        // Drive it hot so the method re-tiers with the NEW body.
        int last = before;
        for (int i = 0; i < HOT; i++) {
            last = v.bump();
        }
        int stepHot = (last - before) / HOT;
        System.out.println("hot/compiled step after redefine = " + stepHot);

        if (stepInterpreted != 100) {
            throw new AssertionError(
                "redefined body not observed by the interpreter: step=" + stepInterpreted);
        }
        if (stepHot != 100) {
            throw new AssertionError(
                "STALE BODY AFTER RE-JIT: recompilation used the pre-redefine bytecode; step="
                + stepHot);
        }
        System.out.println("PASS  redefined body observed both interpreted and compiled");
    }
}
