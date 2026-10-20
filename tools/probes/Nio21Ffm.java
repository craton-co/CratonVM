import java.lang.foreign.*;
import java.lang.invoke.*;
import java.lang.reflect.Modifier;
public class Nio21Ffm {
    static void show(String n, Object o) {
        Class<?> c = o.getClass();
        System.out.println(n + " " + c.getName() + " abstract=" + Modifier.isAbstract(c.getModifiers()));
    }
    public static void main(String[] a) throws Throwable {
        try (Arena ar = Arena.ofConfined()) {
            show("confined.arena", ar);
            show("confined.scope", ar.scope());
            MemorySegment s = ar.allocate(16);
            show("segment", s);
            show("segment.scope", s.scope());
            s.set(ValueLayout.JAVA_INT, 0, 42);
            System.out.println(s.get(ValueLayout.JAVA_INT, 0) + " alive=" + ar.scope().isAlive());
        }
        Arena sh = Arena.ofShared();
        show("shared.scope", sh.scope());
        MemorySegment ss = sh.allocate(8);
        sh.close();
        System.out.println("shared alive after close=" + ss.scope().isAlive());
        try { ss.get(ValueLayout.JAVA_BYTE, 0); } catch (IllegalStateException e) { System.out.println("ISE " + e.getMessage()); }
        show("global.scope", Arena.global().scope());
        show("auto.scope", Arena.ofAuto().scope());
        show("heap.scope", MemorySegment.ofArray(new int[4]).scope());
        Linker l = Linker.nativeLinker();
        MethodHandle h = l.downcallHandle(l.defaultLookup().find(System.getProperty("os.name").startsWith("Win") ? "abs" : "abs").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        show("downcall", h);
        System.out.println((int) h.invokeExact(-5) + " " + h.type());
        System.out.println(h.getClass().getSuperclass().getName());
    }
}
