// Interpreter round i1 wave 9, lane L5: wrapping a failed <clinit>'s exception
// in ExceptionInInitializerError must not call the exception's toString().
//
// HotSpot builds `new ExceptionInInitializerError(thrown)`. CratonVM built the
// no-arg form, wrote `cause` directly, and then called initCause(thrown),
// which always failed (the no-arg constructor had already run initCause(null))
// by building IllegalStateException("Can't overwrite cause with " +
// thrown.toString()) -- so the application's toString() ran once per failed
// initialization, printing "toString called" below.
//
// No setup; plain --compatible, with and without --nojit.
//
// HotSpot 25 prints exactly:
//   caught: java.lang.ExceptionInInitializerError
//   cause is Loud: true
//   getException is getCause: true
//   message: null
//   again: java.lang.NoClassDefFoundError
public class EiieNoCauseToString {
    static class Loud extends RuntimeException {
        Loud() {
            super("loud");
        }

        @Override
        public String toString() {
            System.out.println("toString called");
            return "Loud!";
        }
    }

    static class Boom {
        static {
            if (true) {
                throw new Loud();
            }
        }

        static void touch() {}
    }

    public static void main(String[] args) {
        try {
            Boom.touch();
        } catch (ExceptionInInitializerError e) {
            System.out.println("caught: " + e.getClass().getName());
            System.out.println("cause is Loud: " + (e.getCause() instanceof Loud));
            System.out.println("getException is getCause: " + (e.getException() == e.getCause()));
            System.out.println("message: " + e.getMessage());
        }
        try {
            Boom.touch();
        } catch (NoClassDefFoundError e) {
            System.out.println("again: " + e.getClass().getName());
        }
    }
}
