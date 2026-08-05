/**
 * A class with no dependencies, defined by hand through
 * `ClassLoader.defineClass` in {@link L1LoaderIdentityProbe}. It exists only
 * so the probe has real class bytes to hand a custom loader; its own contents
 * are irrelevant beyond being loadable.
 */
public class L1Payload {
    public static int answer() {
        return 42;
    }
}
