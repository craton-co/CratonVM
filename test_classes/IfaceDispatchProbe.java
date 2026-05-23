public class IfaceDispatchProbe {
    interface Foo { default void hello() { System.out.println("default hello"); } }
    static class Bar implements Foo { public void hello() { System.out.println("Bar.hello"); } }
    public static void main(String[] args) {
        Foo f = new Bar();
        f.hello();  // should print "Bar.hello"
    }
}
