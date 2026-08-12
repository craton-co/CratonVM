import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.concurrent.atomic.AtomicReference;

/**
 * W7-89 — is the FFM liveness gate real?
 *
 * <p>W7-86 §4.1 row 6 recorded {@code MemorySessionImpl.checkValidState(MemorySegment)}
 * as a static native whose body reads {@code args[0]} as the <em>session</em>
 * when {@code args[0]} is in fact the <em>segment</em>, and concluded that the
 * check therefore fails open. This probe measures that from Java rather than
 * assuming it, and it carries its own over-correction guard: a gate that
 * refuses <em>everything</em> passes the closed-arena rows and fails the live
 * ones.
 *
 * <p>Every row prints an exact value. A row whose expected value is a throw
 * prints the exception's class name and message, so a VM that returns instead
 * of throwing shows what it returned ({@code NO-THROW:<value>}) rather than
 * silently satisfying an "assertThrows"-shaped assertion.
 *
 * <pre>
 * javac -d out probes/MemorySessionValidStateProbe.java
 * java     --add-exports java.base/jdk.internal.foreign=ALL-UNNAMED -cp out MemorySessionValidStateProbe
 * cratonvm --add-exports java.base/jdk.internal.foreign=ALL-UNNAMED -cp out MemorySessionValidStateProbe
 * </pre>
 */
public final class MemorySessionValidStateProbe {

    // Printed and FLUSHED per row rather than buffered to the end: CratonVM
    // takes a fatal EXCEPTION_ACCESS_VIOLATION partway through this probe, and
    // a buffered probe loses every row it had already established.
    private static void row(String key, Object value) {
        System.out.println(key + " = " + value);
        System.out.flush();
    }

    /** Runs {@code body}; reports {@code Class:message} on throw, {@code NO-THROW:<value>} otherwise. */
    private static String throwName(Call body) {
        Object value;
        try {
            value = body.call();
        } catch (Throwable t) {
            Throwable c = t;
            // Reflection wraps; report what the callee actually raised.
            while (c instanceof InvocationTargetException ite && ite.getCause() != null) {
                c = ite.getCause();
            }
            return c.getClass().getName() + ":" + c.getMessage();
        }
        return "NO-THROW:" + value;
    }

    private interface Call {
        Object call() throws Throwable;
    }

    // ---- section A: the static native under test, reached directly -------

    private static Method checkValidState;

    private static void resolveStaticCheck() {
        try {
            Class<?> impl = Class.forName("jdk.internal.foreign.MemorySessionImpl");
            checkValidState = impl.getMethod("checkValidState", MemorySegment.class);
            row("A.method.found", "true");
            row("A.method.static", java.lang.reflect.Modifier.isStatic(checkValidState.getModifiers()));
            row("A.method.paramCount", checkValidState.getParameterCount());
            row("A.method.param0", checkValidState.getParameterTypes()[0].getName());
        } catch (Throwable t) {
            row("A.method.found", "false");
            row("A.method.error", t.getClass().getName() + ":" + t.getMessage());
        }
    }

    /** Invokes the static {@code checkValidState(MemorySegment)} reflectively. */
    private static String staticCheck(MemorySegment segment) {
        if (checkValidState == null) {
            return "UNRESOLVED";
        }
        return throwName(() -> {
            checkValidState.invoke(null, segment);
            return "void";
        });
    }

    // ---- section B: a live confined arena must keep working --------------

    private static void liveConfined() {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(16);
            seg.set(ValueLayout.JAVA_INT, 0, 0x11223344);
            row("B.live.get", Integer.toHexString(seg.get(ValueLayout.JAVA_INT, 0)));
            row("B.live.byteSize", seg.byteSize());
            row("B.live.scope.isAlive", seg.scope().isAlive());
            row("B.live.staticCheck", staticCheck(seg));
            row("B.live.set", throwName(() -> {
                seg.set(ValueLayout.JAVA_INT, 4, 7);
                return seg.get(ValueLayout.JAVA_INT, 4);
            }));
            row("B.live.asSlice.get", throwName(() -> seg.asSlice(0, 8).get(ValueLayout.JAVA_INT, 0)));
            row("B.live.copyFrom", throwName(() -> {
                MemorySegment other = arena.allocate(16);
                other.copyFrom(seg);
                return other.get(ValueLayout.JAVA_INT, 0);
            }));
        }
    }

    // ---- section C: the RED — a closed confined arena --------------------

    private static void closedConfined() {
        Arena arena = Arena.ofConfined();
        MemorySegment seg = arena.allocate(16);
        MemorySegment slice = seg.asSlice(0, 8);
        seg.set(ValueLayout.JAVA_INT, 0, 0x55667788);
        arena.close();

        row("C.closed.scope.isAlive", seg.scope().isAlive());
        row("C.closed.byteSize", seg.byteSize());
        row("C.closed.get", throwName(() -> seg.get(ValueLayout.JAVA_INT, 0)));
        row("C.closed.set", throwName(() -> {
            seg.set(ValueLayout.JAVA_INT, 0, 1);
            return "void";
        }));
        row("C.closed.getAtIndex", throwName(() -> seg.getAtIndex(ValueLayout.JAVA_INT, 1)));
        row("C.closed.slice.get", throwName(() -> slice.get(ValueLayout.JAVA_INT, 0)));
        row("C.closed.asSlice", throwName(() -> seg.asSlice(0, 4).byteSize()));
        row("C.closed.staticCheck", staticCheck(seg));
        row("C.closed.staticCheck.slice", staticCheck(slice));
        row("C.closed.reclose", throwName(() -> {
            arena.close();
            return "void";
        }));
        row("C.closed.allocate", throwName(() -> arena.allocate(8).byteSize()));
    }

    // ---- section D: a closed SHARED arena ---------------------------------

    private static void closedShared() {
        Arena arena = Arena.ofShared();
        MemorySegment seg = arena.allocate(16);
        seg.set(ValueLayout.JAVA_LONG, 0, 42L);
        row("D.shared.live.get", seg.get(ValueLayout.JAVA_LONG, 0));
        row("D.shared.live.staticCheck", staticCheck(seg));
        arena.close();
        row("D.shared.closed.scope.isAlive", seg.scope().isAlive());
        row("D.shared.closed.get", throwName(() -> seg.get(ValueLayout.JAVA_LONG, 0)));
        row("D.shared.closed.staticCheck", staticCheck(seg));
    }

    // ---- section E: sessions that can NEVER close -------------------------
    // The over-correction guard proper: a gate that refuses everything reddens
    // every row here while still passing C.

    private static void neverClosing() {
        MemorySegment global = Arena.global().allocate(16);
        global.set(ValueLayout.JAVA_INT, 0, 5);
        row("E.global.get", global.get(ValueLayout.JAVA_INT, 0));
        row("E.global.scope.isAlive", global.scope().isAlive());
        row("E.global.staticCheck", staticCheck(global));

        MemorySegment auto = Arena.ofAuto().allocate(16);
        auto.set(ValueLayout.JAVA_INT, 0, 6);
        row("E.auto.get", auto.get(ValueLayout.JAVA_INT, 0));
        row("E.auto.scope.isAlive", auto.scope().isAlive());
        row("E.auto.staticCheck", staticCheck(auto));

        byte[] backing = new byte[16];
        MemorySegment heap = MemorySegment.ofArray(backing);
        // A byte[] heap segment has alignment 1, so the aligned JAVA_INT layout
        // is refused before liveness is ever consulted. Use the unaligned twin.
        heap.set(ValueLayout.JAVA_INT_UNALIGNED, 0, 7);
        row("E.heap.get", heap.get(ValueLayout.JAVA_INT_UNALIGNED, 0));
        row("E.heap.scope.isAlive", heap.scope().isAlive());
        row("E.heap.staticCheck", staticCheck(heap));
        row("E.heap.aliasesArray", backing[0] != 0 || backing[3] != 0);

        MemorySegment nil = MemorySegment.NULL;
        row("E.null.byteSize", nil.byteSize());
        row("E.null.scope.isAlive", nil.scope().isAlive());
        row("E.null.staticCheck", staticCheck(nil));
    }

    // ---- section F: confinement, which shares the same gate ---------------

    private static void confinement() throws Exception {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(16);
            AtomicReference<String> off = new AtomicReference<>();
            AtomicReference<String> offCheck = new AtomicReference<>();
            Thread t = new Thread(() -> {
                off.set(throwName(() -> seg.get(ValueLayout.JAVA_INT, 0)));
                offCheck.set(staticCheck(seg));
            });
            t.start();
            t.join();
            row("F.confined.offThread.get", off.get());
            row("F.confined.offThread.staticCheck", offCheck.get());
            row("F.confined.onThread.get", throwName(() -> seg.get(ValueLayout.JAVA_INT, 0)));
        }
    }

    // ---- section G: a closed arena's ByteBuffer view ----------------------
    // W7-83 established that `Arena…allocate(n).asByteBuffer()` carries the
    // segment at `Buffer.segment`. Whether the buffer view honours the closed
    // scope is a separate question from the segment accessors, and is measured
    // rather than assumed.

    private static void bufferView() {
        Arena arena = Arena.ofConfined();
        MemorySegment seg = arena.allocate(16);
        java.nio.ByteBuffer live = seg.asByteBuffer();
        live.putInt(0, 0x0abcdef0);
        row("G.live.buffer.getInt", Integer.toHexString(live.getInt(0)));
        row("G.live.buffer.isDirect", live.isDirect());
        row("G.live.buffer.hasArray", live.hasArray());
        arena.close();
        row("G.closed.buffer.getInt", throwName(() -> live.getInt(0)));
        row("G.closed.buffer.putInt", throwName(() -> {
            live.putInt(0, 1);
            return "void";
        }));
        row("G.closed.buffer.capacity", live.capacity());
    }

    /**
     * Sections are selectable so a VM that dies on one of them can still be
     * measured on the rest. CratonVM takes a fatal
     * {@code EXCEPTION_ACCESS_VIOLATION at 0x10} inside {@code E.heap} — a
     * heap-segment defect that is not this lane's — and without this switch
     * that one row would hide sections F and G entirely.
     *
     * <p>{@code java … MemorySessionValidStateProbe ABCDFG} runs all but E.
     * With no argument every section runs, which is what HotSpot is measured on.
     */
    public static void main(String[] args) throws Exception {
        String sections = args.length > 0 ? args[0] : "ABCDEFG";
        if (sections.indexOf('A') >= 0) {
            resolveStaticCheck();
        }
        if (sections.indexOf('B') >= 0) {
            liveConfined();
        }
        if (sections.indexOf('C') >= 0) {
            closedConfined();
        }
        if (sections.indexOf('D') >= 0) {
            closedShared();
        }
        if (sections.indexOf('E') >= 0) {
            neverClosing();
        }
        if (sections.indexOf('F') >= 0) {
            confinement();
        }
        if (sections.indexOf('G') >= 0) {
            bufferView();
        }
    }
}
