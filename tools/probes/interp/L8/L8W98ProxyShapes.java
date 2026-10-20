// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L8 -- serialized dynamic proxies.
// CratonVM forces its own `ObjectInputStream.resolveProxyClass` over the JDK
// body (`reflect_annotations.rs`, `native_ois_resolve_proxy_class`); HotSpot
// runs `Class.forName(name, false, latestUserDefinedLoader())` per interface
// and `Proxy.getProxyClass(loader, classObjs)` over the ORDERED list.
//
//   order-*      two proxy classes over the same interfaces in the two
//                orders; each round-trips to ITS OWN class with its own
//                interface order (CratonVM matched the interface SET and could
//                answer the other class -- FIXED wave 98)
//   call         the deserialized proxy dispatches to its (serializable) handler
//   no-init      an interface with a static initializer is not initialized by
//                proxy creation or by deserializing the proxy (CratonVM's
//                resolution ran `ensure_class_initialized` -- FIXED wave 98)
//   missing      the stream names an interface that does not exist (patched
//                name): ClassNotFoundException naming it (CratonVM:
//                "null class" -- FIXED wave 98)
//   not-iface    the stream names a class: ClassNotFoundException(null) caused
//                by "... is not an interface" (FIXED wave 98)
//   desc         ObjectStreamClass of a proxy class: SUID 0, no fields
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L8/L8W98ProxyShapes.java
//   $B --java-home $J/.. -cp out L8W98ProxyShapes
//   $B --compatible --java-home $J/.. -cp out L8W98ProxyShapes
//   (each also with --nojit)
//   $J/java -cp out L8W98ProxyShapes
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 sources, not measured)
// order-ab same=true [interface L8W98ProxyShapes$Ia, interface L8W98ProxyShapes$Ib]
// order-ba same=true [interface L8W98ProxyShapes$Ib, interface L8W98ProxyShapes$Ia]
// order-distinct true
// call a:handled b:handled
// no-init log=
// missing java.lang.ClassNotFoundException: L8W98ProxyShapes$Iz
// not-iface java.lang.ClassNotFoundException: null cause=java.lang.IllegalArgumentException: L8W98ProxyShapes$Ca is not an interface
// desc suid=0 fields=0
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.ObjectStreamClass;
import java.io.Serializable;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public class L8W98ProxyShapes {
    static final List<String> LOG = new ArrayList<>();

    static Object log(String s) {
        LOG.add(s);
        return s;
    }

    public interface Ia {
        String a();
    }

    public interface Ib {
        String b();
    }

    // Same name length as Ia / Ib, so a stream naming Ia can be patched.
    public static class Ca implements Serializable {
    }

    public interface Ic {
        Object INIT = log("Ic.<clinit>");

        String c();
    }

    static final class Handler implements InvocationHandler, Serializable {
        @Override
        public Object invoke(Object proxy, Method m, Object[] args) {
            if (m.getDeclaringClass() == Object.class) {
                return switch (m.getName()) {
                    case "hashCode" -> 7;
                    case "equals" -> proxy == args[0];
                    default -> "proxy";
                };
            }
            return m.getName() + ":handled";
        }
    }

    static byte[] write(Object o) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(o);
        }
        return bytes.toByteArray();
    }

    static Object read(byte[] b) throws IOException, ClassNotFoundException {
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return in.readObject();
        }
    }

    /** Replace the first occurrence of `from` in `stream` by `to` (same length). */
    static byte[] patch(byte[] stream, String from, String to) {
        byte[] f = from.getBytes(StandardCharsets.UTF_8);
        byte[] t = to.getBytes(StandardCharsets.UTF_8);
        byte[] out = stream.clone();
        outer:
        for (int i = 0; i + f.length <= out.length; i++) {
            for (int j = 0; j < f.length; j++) {
                if (out[i + j] != f[j]) {
                    continue outer;
                }
            }
            System.arraycopy(t, 0, out, i, t.length);
            return out;
        }
        throw new AssertionError("not in the stream: " + from);
    }

    static String outcome(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    public static void main(String[] args) throws Exception {
        ClassLoader cl = L8W98ProxyShapes.class.getClassLoader();
        Handler h = new Handler();
        // Make the [Ib, Ia] class first so a set-keyed lookup would find it.
        Object ba = Proxy.newProxyInstance(cl, new Class<?>[] {Ib.class, Ia.class}, h);
        Object ab = Proxy.newProxyInstance(cl, new Class<?>[] {Ia.class, Ib.class}, h);

        Object abBack = read(write(ab));
        System.out.println("order-ab same=" + (abBack.getClass() == ab.getClass()) + " "
                + Arrays.toString(abBack.getClass().getInterfaces()));
        Object baBack = read(write(ba));
        System.out.println("order-ba same=" + (baBack.getClass() == ba.getClass()) + " "
                + Arrays.toString(baBack.getClass().getInterfaces()));
        System.out.println("order-distinct " + (ab.getClass() != ba.getClass()));
        System.out.println("call " + ((Ia) abBack).a() + " " + ((Ib) abBack).b());

        Object c = Proxy.newProxyInstance(cl, new Class<?>[] {Ic.class}, h);
        Object cBack = read(write(c));
        System.out.println("no-init log=" + String.join(" ", LOG)
                + (cBack.getClass() == c.getClass() ? "" : " other-class"));

        byte[] onlyA = write(Proxy.newProxyInstance(cl, new Class<?>[] {Ia.class}, h));
        try {
            read(patch(onlyA, "L8W98ProxyShapes$Ia", "L8W98ProxyShapes$Iz"));
            System.out.println("missing no-throw");
        } catch (Exception e) {
            System.out.println("missing " + outcome(e));
        }
        try {
            read(patch(onlyA, "L8W98ProxyShapes$Ia", "L8W98ProxyShapes$Ca"));
            System.out.println("not-iface no-throw");
        } catch (Exception e) {
            System.out.println("not-iface " + outcome(e)
                    + (e.getCause() == null ? "" : " cause=" + outcome(e.getCause())));
        }

        ObjectStreamClass desc = ObjectStreamClass.lookup(ab.getClass());
        System.out.println("desc suid=" + desc.getSerialVersionUID() + " fields=" + desc.getFields().length);
    }
}
