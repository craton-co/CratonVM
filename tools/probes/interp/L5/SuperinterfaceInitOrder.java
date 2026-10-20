// Interpreter round i1 wave 9, lane L5: which superinterfaces a class's
// initialization initializes (JVMS §5.5 step 7; HotSpot
// InstanceKlass::initialize_super_interfaces).
//
// Only superinterfaces that DECLARE a default method are initialized, in
// recursive interfaces-array order, and only when a CLASS is initialized: an
// interface's own initialization initializes no superinterface. Before wave 9
// CratonVM also ran the <clinit> of a direct superinterface that only
// INHERITED a default ("init Inherits" too early) and initialized an
// interface's superinterfaces ("init Top").
//
// HotSpot 25 prints exactly:
//   -- new C
//   init Declares
//   init Other
//   init C
//   -- Sub.touch
//   init Sub
//   Sub.touch
//   -- Inherits.Y
//   init Inherits
//   Y=1
public class SuperinterfaceInitOrder {
    static int log(String s) {
        System.out.println(s);
        return 1;
    }

    interface Declares {
        int X = log("init Declares");

        default void d() {}
    }

    interface Inherits extends Declares {
        int Y = log("init Inherits");

        void a();
    }

    interface NoDefaults {
        int W = log("init NoDefaults");

        void n();
    }

    interface Other {
        int Z = log("init Other");

        default void o() {}
    }

    static class C implements Inherits, NoDefaults, Other {
        static {
            log("init C");
        }

        public void a() {}

        public void n() {}
    }

    interface Top {
        int T = log("init Top");

        default void t() {}
    }

    interface Sub extends Top {
        int S = log("init Sub");

        static void touch() {
            log("Sub.touch");
        }
    }

    public static void main(String[] args) {
        log("-- new C");
        new C();
        log("-- Sub.touch");
        Sub.touch();
        log("-- Inherits.Y");
        log("Y=" + Inherits.Y);
    }
}
