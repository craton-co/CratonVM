import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * W7-89 — the object graph behind the FFM liveness gate, printed rather than assumed.
 *
 * <p>Answers, on whichever VM it is run: what runtime class an `Arena`, its
 * session and its segments actually are; whether the segment carries a `scope`
 * field and what it points at; and what the session's own `state` word reads
 * before and after `close()`. `MemorySessionValidStateProbe` measures the
 * behaviour; this measures the shape the repair has to key on.
 *
 * <pre>
 * javac -d out probes/MemorySessionShapeProbe.java
 * java     --enable-native-access=ALL-UNNAMED \
 *          --add-exports java.base/jdk.internal.foreign=ALL-UNNAMED \
 *          --add-opens   java.base/jdk.internal.foreign=ALL-UNNAMED -cp out MemorySessionShapeProbe
 * </pre>
 */
public final class MemorySessionShapeProbe {

    private static void row(String key, Object value) {
        System.out.println(key + " = " + value);
        System.out.flush();
    }

    private static String cls(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    /** Every declared field of the object's class chain, with its value. */
    private static void dumpFields(String prefix, Object o) {
        if (o == null) {
            row(prefix + ".class", "null");
            return;
        }
        row(prefix + ".class", o.getClass().getName());
        int index = 0;
        for (Class<?> c = o.getClass(); c != null && c != Object.class; c = c.getSuperclass()) {
            for (Field f : c.getDeclaredFields()) {
                if (java.lang.reflect.Modifier.isStatic(f.getModifiers())) {
                    continue;
                }
                String value;
                try {
                    f.setAccessible(true);
                    Object v = f.get(o);
                    value = (v instanceof MemorySegment || v instanceof Arena || v == null
                            || v instanceof Number || v instanceof Boolean || v instanceof String)
                            ? String.valueOf(v == null ? "null" : (v.getClass().isPrimitive() ? v : v))
                            : cls(v);
                    if (v != null && !(v instanceof Number) && !(v instanceof Boolean)
                            && !(v instanceof String)) {
                        value = cls(v);
                    }
                } catch (Throwable t) {
                    value = "ERR:" + t.getClass().getSimpleName();
                }
                row(prefix + ".f" + index + "." + c.getSimpleName() + "." + f.getName(), value);
                index++;
            }
        }
    }

    private static Object readByName(Object o, String name) {
        for (Class<?> c = o.getClass(); c != null; c = c.getSuperclass()) {
            try {
                Field f = c.getDeclaredField(name);
                f.setAccessible(true);
                return f.get(o);
            } catch (NoSuchFieldException e) {
                // keep walking
            } catch (Throwable t) {
                return "ERR:" + t.getClass().getSimpleName() + ":" + t.getMessage();
            }
        }
        return "ABSENT";
    }

    public static void main(String[] args) throws Exception {
        Arena arena = Arena.ofConfined();
        row("arena.class", cls(arena));
        MemorySegment seg = arena.allocate(16);
        row("seg.class", cls(seg));
        Object scope = seg.scope();
        row("seg.scope().class", cls(scope));
        row("seg.scope().isAlive", seg.scope().isAlive());
        row("arena.scope().class", cls(arena.scope()));
        row("arena.scope()==seg.scope()", arena.scope() == seg.scope());

        row("seg.field.scope", cls(readByName(seg, "scope")));
        row("seg.field.scope.identity", readByName(seg, "scope") == scope);
        row("session.field.state.before", readByName(scope, "state"));
        row("session.field.acquireCount.before", readByName(scope, "acquireCount"));
        row("session.field.owner.before", cls(readByName(scope, "owner")));

        dumpFields("segDump", seg);
        dumpFields("sessionDump", scope);

        // Does the JDK's own MemorySessionImpl.close() run?
        arena.close();
        row("session.field.state.after", readByName(scope, "state"));
        row("session.isAlive.after", seg.scope().isAlive());

        // Reach the instance checkValidState()V directly, bypassing the segment.
        try {
            Class<?> impl = Class.forName("jdk.internal.foreign.MemorySessionImpl");
            Method m = impl.getMethod("checkValidState");
            try {
                m.invoke(scope);
                row("session.checkValidState.after", "NO-THROW");
            } catch (Throwable t) {
                Throwable c = t.getCause() == null ? t : t.getCause();
                row("session.checkValidState.after", c.getClass().getName() + ":" + c.getMessage());
            }
            Method raw = impl.getMethod("checkValidStateRaw");
            try {
                raw.invoke(scope);
                row("session.checkValidStateRaw.after", "NO-THROW");
            } catch (Throwable t) {
                Throwable c = t.getCause() == null ? t : t.getCause();
                row("session.checkValidStateRaw.after", c.getClass().getName() + ":" + c.getMessage());
            }
            row("session.instanceof.MemorySessionImpl", impl.isInstance(scope));
            Class<?> absSeg = Class.forName("jdk.internal.foreign.AbstractMemorySegmentImpl");
            row("seg.instanceof.AbstractMemorySegmentImpl", absSeg.isInstance(seg));
            Method sessionImpl = absSeg.getMethod("sessionImpl");
            try {
                row("seg.sessionImpl()", cls(sessionImpl.invoke(seg)));
                row("seg.sessionImpl()==scope", sessionImpl.invoke(seg) == scope);
            } catch (Throwable t) {
                Throwable c = t.getCause() == null ? t : t.getCause();
                row("seg.sessionImpl()", c.getClass().getName() + ":" + c.getMessage());
            }
        } catch (Throwable t) {
            row("impl.error", t.getClass().getName() + ":" + t.getMessage());
        }

        // A heap segment for contrast: no arena at all.
        MemorySegment heap = MemorySegment.ofArray(new byte[16]);
        row("heap.class", cls(heap));
        row("heap.scope().class", cls(heap.scope()));
        row("heap.field.scope", cls(readByName(heap, "scope")));
        row("global.scope().class", cls(Arena.global().scope()));
        row("global.seg.field.scope", cls(readByName(Arena.global().allocate(8), "scope")));
        row("auto.seg.field.scope", cls(readByName(Arena.ofAuto().allocate(8), "scope")));
        row("shared.seg.field.scope", cls(readByName(Arena.ofShared().allocate(8), "scope")));
        row("NULL.field.scope", cls(readByName(MemorySegment.NULL, "scope")));
        row("slice.field.scope", cls(readByName(heap.asSlice(0, 8), "scope")));
        row("done", ValueLayout.JAVA_INT.byteSize());
    }
}
