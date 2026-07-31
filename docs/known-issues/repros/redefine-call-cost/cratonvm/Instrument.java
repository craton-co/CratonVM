package cratonvm;

/**
 * Declaration-only stand-in for the VM-side instrumentation bridge.
 *
 * CratonVM registers a Rust native for `cratonvm/Instrument.redefineClass`
 * (see `vm/src/runtime/instrument.rs`), but nothing on the classpath declares
 * the class, so `Class.forName("cratonvm.Instrument")` fails and the probe
 * silently degrades to a control run. This supplies the declaration the native
 * binds to; the body is the VM's.
 */
public final class Instrument {
    private Instrument() {}

    /** Redefine {@code target} with {@code classBytes}. Returns true on success. */
    public static native boolean redefineClass(Class<?> target, byte[] classBytes);
}
