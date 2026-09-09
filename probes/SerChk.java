import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.lang.reflect.Constructor;
import java.util.ArrayList;
import java.util.HashMap;
import sun.reflect.ReflectionFactory;

/**
 * Acceptance instrument for the JDK 21 serialization round-trip defect.
 *
 * Part A is the user-visible contract (round-trip identity). Part B is the
 * layer below it (the factory that produces the serialization constructor), so
 * a fix can be shown to work at the level it was made, not only at the surface.
 *
 * Every case asserts the IDENTITY of the result, not merely that nothing was
 * thrown: half of this defect is SILENT (Integer deserialises to a bare Object
 * and throws nothing), and a check that only catches throws cannot see it.
 *
 * String is a deliberate control: it round-trips through TC_STRING in the
 * stream itself, never through the object-construction path under test, so it
 * must pass even on a completely broken build. An all-red run including String
 * means the probe is broken, not the VM.
 */
public class SerChk {

    static int pass = 0;
    static int fail = 0;

    static byte[] enc(Object o) throws Exception {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        ObjectOutputStream oos = new ObjectOutputStream(bos);
        oos.writeObject(o);
        oos.close();
        return bos.toByteArray();
    }

    static void roundTrip(Object o, boolean isControl) {
        String want = o.getClass().getName();
        String tag = isControl ? "  [control]" : "";
        try {
            Object back = new ObjectInputStream(new ByteArrayInputStream(enc(o))).readObject();
            String got = (back == null) ? "null" : back.getClass().getName();
            boolean ok = want.equals(got);
            if (ok) pass++; else fail++;
            System.out.println("  A " + (ok ? "OK   " : "WRONG") + " wrote " + want
                    + " -> read " + got + tag);
        } catch (Throwable t) {
            fail++;
            System.out.println("  A THREW wrote " + want + " -> "
                    + t.getClass().getName() + ": " + t.getMessage() + tag);
        }
    }

    static void factory(Class<?> cl) {
        try {
            ReflectionFactory rf = ReflectionFactory.getReflectionFactory();
            Constructor<?> ctor = rf.newConstructorForSerialization(cl);
            String decl = ctor.getDeclaringClass().getName();
            Object made = ctor.newInstance();
            String got = (made == null) ? "null" : made.getClass().getName();
            boolean ok = cl.getName().equals(got);
            if (ok) pass++; else fail++;
            System.out.println("  B " + (ok ? "OK   " : "WRONG") + " target " + cl.getName()
                    + "  ctorDeclaredBy " + decl + "  newInstance -> " + got);
        } catch (Throwable t) {
            fail++;
            System.out.println("  B THREW target " + cl.getName() + " -> "
                    + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("java.version = " + System.getProperty("java.version"));

        System.out.println("part A: ObjectInputStream round-trip identity");
        roundTrip(Integer.valueOf(42), false);
        roundTrip(Long.valueOf(7L), false);
        roundTrip(Boolean.TRUE, false);
        ArrayList<String> al = new ArrayList<>();
        al.add("a");
        al.add("b");
        roundTrip(al, false);
        HashMap<String, String> hm = new HashMap<>();
        hm.put("k", "v");
        roundTrip(hm, false);
        roundTrip("hello", true);

        System.out.println("part B: ReflectionFactory.newConstructorForSerialization");
        factory(Integer.class);
        factory(ArrayList.class);
        factory(HashMap.class);

        System.out.println("RESULT pass=" + pass + " fail=" + fail
                + (fail == 0 ? "  ALL-OK" : "  DEFECT-PRESENT"));
    }
}
