import java.lang.reflect.Field;
import java.net.URL;

/**
 * Paired probe for the {@code --add-opens} **command line flag**, as distinct
 * from the module-open bookkeeping it feeds.
 *
 * {@code SetAccessibleModuleProbe} already covers the encapsulation gate itself.
 * What it cannot answer is whether a launcher flag ever reaches that gate: a
 * flag that is parsed into {@code VmConfig} and then read by nobody looks
 * identical, from inside Java, to a flag that was never passed. That failure
 * mode is on record for its two siblings — {@code --module-path} and
 * {@code --add-modules} were parsed and ignored for months
 * (classloading/src/module.rs, "`--add-opens was parsed then ignored`").
 *
 * The subject is the exact member Spring Boot's
 * {@code DirtiesUrlFactoriesExtension} reaches for, {@code java.net.URL.factory},
 * via the same two steps {@code ReflectionTestUtils.setField} takes:
 * {@code setAccessible(true)} then a write. Both are printed, because the two
 * fail differently and only the pair distinguishes "the gate denied it" from
 * "the field moved".
 *
 * Every line prints an outcome as a value, never a verdict, so the four
 * transcripts (host JDK / CratonVM) x (with / without the flag) diff directly.
 * The flag is load-bearing only if the WITHOUT arm is red and the WITH arm is
 * green — a probe that is green in both arms proves nothing about the flag.
 */
public class AddOpensFlagProbe {

    static String outcome(ThrowingRunnable r) {
        try {
            r.run();
            return "OK";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    interface ThrowingRunnable {
        void run() throws Throwable;
    }

    public static void main(String[] args) throws Exception {
        Module base = URL.class.getModule();
        Module self = AddOpensFlagProbe.class.getModule();

        // The bookkeeping the flag is supposed to move, read back through the
        // public API. `isOpen(pkg)` is the unqualified form; `isOpen(pkg, self)`
        // is what `--add-opens=...=ALL-UNNAMED` actually grants.
        System.out.println("module base.name=" + base.getName());
        System.out.println("module self.named=" + self.isNamed());
        System.out.println("open java.net unqualified=" + base.isOpen("java.net"));
        System.out.println("open java.net toSelf=" + base.isOpen("java.net", self));
        System.out.println("exported java.net toSelf=" + base.isExported("java.net", self));

        // Control: java.base exports java.util and opens it to nobody, and no
        // `--add-opens=java.base/java.net` can move it. A run where this line
        // changes between the two arms is measuring something other than the
        // flag under test.
        System.out.println("open java.util toSelf=" + base.isOpen("java.util", self));

        // The failing operation itself, in the order Spring takes it.
        Field factory = URL.class.getDeclaredField("factory");
        System.out.println("URL.factory setAccessible=" + outcome(() -> factory.setAccessible(true)));
        System.out.println("URL.factory set=" + outcome(() -> factory.set(null, null)));

        // Second member in the same package, non-static, to show the grant is
        // package-wide rather than one memoised field.
        Field handler = URL.class.getDeclaredField("handler");
        System.out.println("URL.handler setAccessible=" + outcome(() -> handler.setAccessible(true)));

        // Control member in a package `--add-opens=java.base/java.net` must not
        // reach: private, in java.util, so denied in both arms on a correct VM.
        Field alu = java.util.ArrayList.class.getDeclaredField("elementData");
        System.out.println("ArrayList.elementData setAccessible=" + outcome(() -> alu.setAccessible(true)));
    }
}
