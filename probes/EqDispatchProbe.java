public class EqDispatchProbe {
    static class EqNull {
        public boolean equals(Object o) { System.out.println("    [EqNull.equals called with " + o + "]"); return this == o || o == null; }
        public int hashCode() { return 7; }
        public String toString() { return "null"; }
    }
    static class EqAlways {
        public boolean equals(Object o) { return true; }
        public int hashCode() { return 9; }
    }
    public static void main(String[] a) {
        EqNull x = new EqNull();
        Object o = x;
        System.out.println("1 static-type EqNull  x.equals(null)      = " + x.equals(null));
        System.out.println("2 static-type Object  o.equals(null)      = " + o.equals(null));
        System.out.println("3 static-type Object  o.equals(new Obj)   = " + o.equals(new Object()));
        EqAlways y = new EqAlways();
        Object oy = y;
        System.out.println("4 EqAlways  oy.equals(null)               = " + oy.equals(null));
        System.out.println("5 EqAlways  oy.equals(\"s\")                = " + oy.equals("s"));
        System.out.println("6 java.util.Objects.equals(x, null)       = " + java.util.Objects.equals(x, null));
    }
}
