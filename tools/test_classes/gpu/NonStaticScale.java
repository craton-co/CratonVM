// Phase 9 #2 fixture: a non-static method whose body uses
// the `aload_0; getfield <primitive-array-field>` pattern to access
// `this.data`. The analyzer accepts this method (the receiver-access
// pattern is the supported shape); the Phase 9 #2 marshaller extracts
// `data` from the receiver and passes it as a kernel arg; the emitter
// maps `aload_0; getfield data` to that arg slot.
//
// Note: the method is NOT @GpuKernel-annotated. The analyzer admits
// non-static methods automatically when the only `aload_0` usage is
// the receiver-access pattern.

public class NonStaticScale {
    public int[] data;

    public NonStaticScale(int[] data) {
        this.data = data;
    }

    /** Multiply each element of `this.data` by `factor`. */
    public void scaleInPlace(int factor) {
        int n = this.data.length;
        for (int i = 0; i < n; i++) {
            this.data[i] = this.data[i] * factor;
        }
    }
}
