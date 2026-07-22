import java.io.File;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class StaticWarFileAfterFailure {
    private static File warFile;

    private static boolean alwaysThrow() {
        return true;
    }

    public static void before() {
        if (alwaysThrow()) {
            throw new RuntimeException("before failed before assignment");
        }
        warFile = new File(System.getProperty("java.io.tmpdir"), "StaticWarFileAfterFailure.tmp");
    }

    public static void after() {
        System.out.println("warFile is " + (warFile == null ? "null" : warFile.getAbsolutePath()));
        System.out.println("delete returned " + warFile.delete());
    }

    public static void main(String[] args) throws Exception {
        boolean reflect = args.length > 0 && args[0].equals("reflect");
        if (reflect) {
            invoke("before");
            invoke("after");
        } else {
            try {
                before();
            } catch (Throwable t) {
                System.out.println("before threw " + t.getClass().getName());
            }
            try {
                after();
            } catch (Throwable t) {
                System.out.println("after threw " + t.getClass().getName());
                t.printStackTrace(System.out);
            }
        }
    }

    private static void invoke(String name) throws Exception {
        Method method = StaticWarFileAfterFailure.class.getDeclaredMethod(name);
        try {
            method.invoke(null);
        } catch (InvocationTargetException e) {
            Throwable cause = e.getCause();
            System.out.println(name + " threw " + cause.getClass().getName());
            cause.printStackTrace(System.out);
        }
    }
}

