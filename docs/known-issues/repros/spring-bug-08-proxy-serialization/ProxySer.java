import java.io.*;
import java.lang.reflect.*;

public class ProxySer {
    public interface Greeter extends Serializable { String greet(String n); }

    static final class Handler implements InvocationHandler, Serializable {
        private final String prefix;
        Handler(String p) { prefix = p; }
        public Object invoke(Object proxy, Method m, Object[] args) {
            if (m.getName().equals("greet")) return prefix + args[0];
            if (m.getName().equals("equals")) return proxy == args[0];
            if (m.getName().equals("hashCode")) return System.identityHashCode(proxy);
            if (m.getName().equals("toString")) return "GreeterProxy[" + prefix + "]";
            return null;
        }
    }

    public static void main(String[] a) throws Exception {
        Greeter g = (Greeter) Proxy.newProxyInstance(
            ProxySer.class.getClassLoader(),
            new Class<?>[]{ Greeter.class },
            new Handler("hello "));
        System.out.println("orig: " + g.greet("world") + "  class=" + g.getClass().getName());

        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream oos = new ObjectOutputStream(bos)) { oos.writeObject(g); }
        byte[] bytes = bos.toByteArray();
        System.out.println("serialized " + bytes.length + " bytes");

        Object back;
        try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(bytes))) {
            back = ois.readObject();
        }
        Greeter g2 = (Greeter) back;
        System.out.println("deser: " + g2.greet("again") + "  class=" + g2.getClass().getName());
        System.out.println("RESULT=" + ("hello again".equals(g2.greet("again")) ? "OK" : "FAIL"));
    }
}
