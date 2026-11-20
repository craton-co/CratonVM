public class RejectFieldAccess {
    // A static field read from inside a static method emits
    // `getstatic` (0xB2), which the analyzer classifies as
    // Reject(FieldAccess). The signature stays primitive so the
    // descriptor parser doesn't bail first.
    private static int COUNTER = 0;

    public static int read() {
        return COUNTER;
    }
}
