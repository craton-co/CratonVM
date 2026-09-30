/**
 * Force-load the classes `vm/src/runtime/instrument.rs` registers against, so a
 * `--dump-native-registry` from this run carries their census verdicts.
 *
 * Same pattern as AwtCategoryCensus (G79-1): the dump's
 * `real_declaring_method` fields are only meaningful for classes the run
 * loaded, and nothing in the corpus loads the instrumentation surface — so
 * every one of its 33 registrations reads `class-not-loaded`, which is "no
 * question asked", not "clean".
 */
public class InstrCensus {
    static final String[] CLASSES = {
        "com.sun.tools.attach.VirtualMachine",
        "sun.instrument.InstrumentationImpl",
    };

    public static void main(String[] a) {
        int ok = 0, missing = 0;
        ClassLoader cl = InstrCensus.class.getClassLoader();
        for (String c : CLASSES) {
            try {
                Class.forName(c, false, cl);
                ok++;
            } catch (Throwable t) {
                missing++;
                System.out.println("MISSING " + c + " : " + t.getClass().getName());
            }
        }
        System.out.println("CENSUS loaded=" + ok + " missing=" + missing);
    }
}
