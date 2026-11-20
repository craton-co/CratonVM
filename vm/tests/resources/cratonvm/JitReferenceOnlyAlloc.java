package cratonvm;

public final class JitReferenceOnlyAlloc {
    static final class Holder {
        Object value;
    }

    public static Holder allocate() {
        return new Holder();
    }
}
