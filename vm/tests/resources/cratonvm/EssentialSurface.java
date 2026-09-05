package cratonvm;

import java.sql.Driver;
import java.util.ServiceLoader;

/**
 * The smallest Java a VM built by {@code Vm::new(VmConfig::new())} must be able
 * to run: four {@code java.lang.String} calls and one {@code ServiceLoader.load}.
 *
 * <p>Every method here raised {@code java.lang.NoSuchMethodError} in a DEFAULT
 * (no {@code synthetic-jdk} feature) build until 2026-09-05. That build has no
 * synthetic class library compiled in, so there is no bytecode behind these
 * names and the essential Rust natives are the whole implementation — and
 * {@code vm_init} was applying the real-JDK "drop the native, the bytecode will
 * answer it" policy over them anyway. Recorded in
 * {@code docs/known-issues/jdk-only/F30-1-the-registrar-call-graph-and-the-drifted-arm-20260813.md}
 * &sect;9, and in the internal tree at
 * {@code fixed-bugs/string-length-nosuchmethoderror-in-a-default-build-synthetic-vm-FIXED-20260905.md}.
 *
 * <p>Deliberately trivial, and deliberately NOT a String conformance fixture:
 * it asks only whether these names RESOLVE. Conformance is a question for a
 * build that has a real class library to be conformant to.
 *
 * <p>{@code String.equals} is absent on purpose. It was dropped by the same
 * rule, but a call to it resolves either way — to {@code Object.equals}, which
 * interning then makes answer correctly for literals. A method that cannot see
 * the defect does not belong in the fixture that pins it.
 */
public class EssentialSurface {

    /** 6. The exact call that raised {@code NoSuchMethodError}. */
    public static int literalLength() {
        return "abcdef".length();
    }

    /** 1. Registered beside {@code length()} and dropped by the same rule. */
    public static int literalIsEmpty() {
        return "".isEmpty() ? 1 : 0;
    }

    /** 99, i.e. {@code 'c'}. */
    public static int literalCharAt() {
        return "abcdef".charAt(2);
    }

    /** 3. */
    public static int literalSubstringLength() {
        return "abcdef".substring(3).length();
    }

    /** 1. The WP1.8 half: no {@code ServiceLoader} bytecode exists here either. */
    public static int serviceLoaderLoads() {
        return ServiceLoader.load(Driver.class) != null ? 1 : 0;
    }
}
