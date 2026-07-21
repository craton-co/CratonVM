package cratonvm;

/**
 * Regression for native ClassLoader.loadClass(String) shadow dispatch.
 *
 * Both entry paths must run a real one-argument override exactly once:
 * Class.forName is native-first, while AggregatingLoader.findClass invokes the
 * override directly in bytecode before that override delegates with super.
 */
public final class ClassLoaderSingleOverrideDispatch {
    private static final class CountingLoader extends ClassLoader {
        private int calls;

        CountingLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        public Class<?> loadClass(String name) throws ClassNotFoundException {
            calls++;
            return super.loadClass(name);
        }

        int calls() {
            return calls;
        }
    }

    private static final class AggregatingLoader extends ClassLoader {
        private final ClassLoader delegate;

        AggregatingLoader(ClassLoader delegate) {
            super(null);
            this.delegate = delegate;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            return delegate.loadClass(name);
        }
    }

    public static void main(String[] args) throws Exception {
        String name = ClassLoaderSingleOverrideDispatch.class.getName();

        CountingLoader nativeFirst = new CountingLoader(
                ClassLoaderSingleOverrideDispatch.class.getClassLoader());
        Class.forName(name, false, nativeFirst);
        require(nativeFirst.calls() == 1, "native-first calls=" + nativeFirst.calls());

        CountingLoader bytecodeFirst = new CountingLoader(
                ClassLoaderSingleOverrideDispatch.class.getClassLoader());
        Class.forName(name, false, new AggregatingLoader(bytecodeFirst));
        require(bytecodeFirst.calls() == 1, "bytecode-first calls=" + bytecodeFirst.calls());

        System.out.println("CLASSLOADER_SINGLE_OVERRIDE_DISPATCH_OK");
    }

    private static void require(boolean value, String detail) {
        if (!value) throw new AssertionError(detail);
    }
}
