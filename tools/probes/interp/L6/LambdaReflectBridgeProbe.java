/*
 * Interpreter round i1 wave 8, lane L6: reflection over, and serialization
 * of, a lambda class that `altMetafactory` gave marker interfaces and
 * FLAG_BRIDGES bridges.
 *
 * javac compiles `(ObjM & StrM) () -> "s"` (ObjM { Object m(); },
 * StrM { String m(); }) to altMetafactory with one marker (StrM) and one
 * bridge. HotSpot's spun class implements ObjM and StrM and declares BOTH
 * `m` methods (the SAM and the bridge); invoking either reflectively runs the
 * body. Before wave 8 CratonVM's `getDeclaredMethods()` listed the SAM only
 * and `getInterfaces()` omitted the marker
 * (docs/internal/fixed-bugs/interpreter-L6-lambda-reflection-omits-flag-bridges-methods-FIXED-20260924.md).
 * A serializable intersection lambda must keep both across a serialization
 * round trip (HotSpot replays the bootstrap through `$deserializeLambda$`).
 *
 * Method and interface lists are printed sorted, so the output does not
 * depend on reflection order.
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   declaredM=java.lang.Object,java.lang.String
 *   invoke()Object=s
 *   invoke()String=s
 *   interfaces=ObjM,StrM
 *   plainDeclaredM=java.lang.String
 *   plainInterfaces=StrM
 *   serDeclaredM=java.lang.Object,java.lang.String
 *   serInterfaces=ObjM,Serializable,StrM
 *   deserObj=s
 *   deserStr=s
 *   deserMarker=true
 *
 * Run with and without --nojit.
 */
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

public class LambdaReflectBridgeProbe {
    interface ObjM {
        Object m();
    }

    interface StrM {
        String m();
    }

    static String declaredM(Object lambda) {
        List<String> names = new ArrayList<>();
        for (Method m : lambda.getClass().getDeclaredMethods()) {
            if (m.getName().equals("m") && m.getParameterCount() == 0) {
                names.add(m.getReturnType().getName());
            }
        }
        Collections.sort(names);
        return String.join(",", names);
    }

    static String interfaces(Object lambda) {
        List<String> names = new ArrayList<>();
        for (Class<?> c : lambda.getClass().getInterfaces()) {
            names.add(c.getSimpleName());
        }
        Collections.sort(names);
        return String.join(",", names);
    }

    static void invokeEach(Object lambda) {
        List<String> lines = new ArrayList<>();
        for (Method m : lambda.getClass().getDeclaredMethods()) {
            if (!m.getName().equals("m") || m.getParameterCount() != 0) {
                continue;
            }
            String out;
            try {
                out = String.valueOf(m.invoke(lambda));
            } catch (Throwable t) {
                out = t.getClass().getName();
            }
            lines.add("invoke()" + m.getReturnType().getSimpleName() + "=" + out);
        }
        Collections.sort(lines);
        for (String line : lines) {
            System.out.println(line);
        }
    }

    static Object roundTrip(Object o) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(o);
        }
        try (ObjectInputStream in =
                new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            return in.readObject();
        }
    }

    public static void main(String[] args) throws Exception {
        Object both = (ObjM & StrM) () -> "s";
        System.out.println("declaredM=" + declaredM(both));
        invokeEach(both);
        System.out.println("interfaces=" + interfaces(both));

        StrM plain = () -> "p";
        System.out.println("plainDeclaredM=" + declaredM(plain));
        System.out.println("plainInterfaces=" + interfaces(plain));

        Object ser = (ObjM & StrM & Serializable) () -> "s";
        System.out.println("serDeclaredM=" + declaredM(ser));
        System.out.println("serInterfaces=" + interfaces(ser));

        Object back;
        try {
            back = roundTrip(ser);
        } catch (Throwable t) {
            back = t;
        }
        String obj;
        try {
            obj = String.valueOf(((ObjM) back).m());
        } catch (Throwable t) {
            obj = t.getClass().getName();
        }
        System.out.println("deserObj=" + obj);
        String str;
        try {
            str = ((StrM) back).m();
        } catch (Throwable t) {
            str = t.getClass().getName();
        }
        System.out.println("deserStr=" + str);
        System.out.println("deserMarker=" + (back instanceof StrM));
    }
}
