import java.io.File;
import java.util.Locale;
import java.util.Set;

/**
 * j27 lane FASTBOOT2: the state the JDK's System.initPhase1 leaves behind,
 * which the --compatible fast boot (CRATONVM_COMPAT_FAST_BOOT_JIT) must leave
 * too. Run it as the FIRST code of the program: each row is the first use of
 * its subsystem. Expected HotSpot 25 output (Linux and Windows):
 *
 * fact concat a1b
 * fact cldr 1.234.567
 * fact referenceHandler true
 * fact finalizer true
 * fact lineSeparator true
 * fact userDirCached true
 * fact intPrevIsDefault false
 */
public class J27FastBootPhase1 {
    public static void main(String[] args) throws Exception {
        // First, before anything else can initialise StaticProperty:
        // initPhase1 caches user.dir there, so a later setProperty does not
        // move File's notion of the working directory.
        String bootDir = System.getProperty("user.dir");
        System.setProperty("user.dir", bootDir + File.separator + "j27-nowhere");
        boolean userDirCached = new File("x").getAbsolutePath()
                .equals(bootDir + File.separator + "x");
        System.setProperty("user.dir", bootDir);
        int one = args.length + 1;
        // StringConcatFactory.<clinit> reads VM.getSavedProperty.
        System.out.println("fact concat " + "a" + one + "b");
        // CLDR provider lookup: ServiceLoader -> ClassLoaders.<clinit>, which
        // reads VM.getSavedProperty.
        System.out.println("fact cldr " + String.format(Locale.GERMANY, "%,d", 1234567));
        Set<String> names = new java.util.HashSet<>();
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            names.add(t.getName());
        }
        System.out.println("fact referenceHandler " + names.contains("Reference Handler"));
        System.out.println("fact finalizer " + names.contains("Finalizer"));
        System.out.println("fact lineSeparator "
                + System.lineSeparator().equals(System.getProperty("line.separator")));
        System.out.println("fact userDirCached " + userDirCached);
        // Terminator.setup() installed INT, so the previous handler is not SIG_DFL.
        sun.misc.SignalHandler prev = sun.misc.Signal.handle(new sun.misc.Signal("INT"), s -> { });
        System.out.println("fact intPrevIsDefault " + (prev == sun.misc.SignalHandler.SIG_DFL));
    }
}
