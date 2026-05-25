public class ClassLoadProbe {
    public static void main(String[] args) {
        if (args.length < 1) {
            System.out.println("Usage: ClassLoadProbe <fully.qualified.ClassName> [<more>...]");
            System.exit(1);
        }
        int n = 0;
        for (String name : args) {
            try {
                Class<?> c = Class.forName(name, false, ClassLoadProbe.class.getClassLoader());
                System.out.println("Loaded: " + c.getName() + " (super=" + c.getSuperclass() + ")");
                n++;
            } catch (Throwable t) {
                System.out.println("FAIL: " + name + ": " + t.getClass().getSimpleName() + ": " + t.getMessage());
                System.exit(1);
            }
        }
        System.out.println("Loaded " + n + " classes");
        System.out.println("OK");
        System.exit(0);
    }
}
