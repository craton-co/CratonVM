/**
 * W7-86 — {@code Runtime.exit(int)} is an INSTANCE method whose native shared
 * the body of the STATIC {@code System.exit(int)}.
 *
 * <p>This is a separate file from {@code StaticNativeArityProbe} because the
 * observable is the <b>process exit code</b>: the call does not return, so it
 * cannot sit beside other checks, and nothing this program prints is evidence.
 *
 * <p>{@code javap -p --module java.base java.lang.Runtime} on Adoptium 25.0.3.9
 * gives {@code public void exit(int)} — an instance method, so {@code args[0]}
 * is the {@code Runtime} receiver and {@code args[1]} is the status.
 * {@code System.exit(int)} is static and its {@code args[0]} IS the status.
 * One Rust body served both, so the {@code Runtime} form read the receiver
 * where an {@code Int} was expected and fell to its zero default.
 *
 * <p>Run both arms and compare the shell's exit status, not stdout:
 * <pre>
 *   javac -d out probes/RuntimeExitArityProbe.java
 *   java -cp out RuntimeExitArityProbe runtime ; echo $?   # want 7
 *   java -cp out RuntimeExitArityProbe system  ; echo $?   # want 7
 * </pre>
 *
 * <p>The {@code system} arm is the control and it is what makes this probe able
 * to fail usefully: it went through the same Rust body and was CORRECT before
 * the fix. A change that makes both arms exit 7 by ignoring the argument
 * entirely would also need to leave the {@code zero} arm at 0 — hence three
 * arms, not two.
 *
 * <p>Measured 2026-08-12, Windows 11:
 * <pre>
 *   arm      HotSpot 25.0.3.9   CratonVM BEFORE   CratonVM AFTER (expected)
 *   runtime  7                  0                 7
 *   system   7                  7                 7
 *   zero     0                  0                 0
 * </pre>
 */
public final class RuntimeExitArityProbe {
    public static void main(String[] args) {
        String arm = args.length > 0 ? args[0] : "runtime";
        switch (arm) {
            case "system" -> System.exit(7);
            case "zero" -> Runtime.getRuntime().exit(0);
            default -> Runtime.getRuntime().exit(7);
        }
        // Unreachable on any VM that implements exit at all; a VM that returns
        // from it should not be scored as "exited 0" by accident.
        System.out.println("exit returned — the native did not terminate");
        System.exit(42);
    }
}
