package cratonvm;

/**
 * {@code Runtime.getRuntime().halt(status)} must terminate the process
 * immediately with that status, and must NOT run shutdown hooks.
 *
 * `Runtime.halt` calls two `java.lang.Shutdown` natives — `beforeHalt()` and
 * (via `Shutdown.halt`) `halt0(int)`. With `beforeHalt` unregistered the whole
 * call threw `UnsatisfiedLinkError` out of a method that cannot legally
 * return, so the process carried on running instead of dying.
 *
 * Run with one argument: the exit status to request. Every line this prints
 * before the halt is proof it got that far; nothing may print after.
 */
public final class RuntimeHalt {

    private RuntimeHalt() {
    }

    public static void main(String[] args) {
        int status = args.length > 0 ? Integer.parseInt(args[0]) : 0;

        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            // halt() must skip this. If it runs, the marker below appears and
            // the test fails — that would mean halt() was quietly routed
            // through the ordinary exit path.
            System.out.println("SHUTDOWN-HOOK-RAN");
            System.out.flush();
        }));

        System.out.println("BEFORE-HALT");
        System.out.flush();

        Runtime.getRuntime().halt(status);

        // Unreachable. Printing it means halt() returned, which it must never
        // do (short of the CRATONVM_SOFT_EXIT opt-in).
        System.out.println("HALT-RETURNED");
        System.out.flush();
    }
}
