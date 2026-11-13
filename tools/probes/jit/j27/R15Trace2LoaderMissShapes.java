// Regression probe for TR2W2-6: Class.forName on an array class must preserve
// the original loader failure's trace while renaming its detail message to the
// missing element type.
public final class R15Trace2LoaderMissShapes {
    static final class AppLoader extends ClassLoader {
        AppLoader() { super(R15Trace2LoaderMissShapes.class.getClassLoader()); }
        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals("r15.Missing")) return missingClass();
            return super.loadClass(name, resolve);
        }
        private Class<?> missingClass() throws ClassNotFoundException {
            throw new ClassNotFoundException("loader detail");
        }
    }

    public static void main(String[] args) {
        try {
            Class.forName("[Lr15.Missing;", false, new AppLoader());
            fact("threw", false);
        } catch (ClassNotFoundException e) {
            boolean loader = false;
            boolean forName = false;
            for (StackTraceElement frame : e.getStackTrace()) {
                loader |= frame.getClassName().equals(AppLoader.class.getName())
                    && frame.getMethodName().equals("missingClass");
                forName |= frame.getClassName().equals("java.lang.Class")
                    && frame.getMethodName().equals("forName");
            }
            fact("type", e.getClass().getName());
            fact("message", e.getMessage());
            fact("loader-frame", loader);
            fact("for-name-frame", forName);
        }
    }

    static void fact(String key, Object value) {
        System.out.println("fact " + key + " " + value);
    }
}
