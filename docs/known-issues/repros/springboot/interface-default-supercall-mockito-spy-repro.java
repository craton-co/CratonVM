interface Greeter {
    default String greet() { return "interface-default"; }
}

class MyGreeter implements Greeter {
    @Override
    public String greet() {
        return Greeter.super.greet() + "-overridden";
    }
}

public class ReproSpy {
    public static void main(String[] args) throws Exception {
        MyGreeter real = new MyGreeter();
        MyGreeter spy = org.mockito.Mockito.spy(real);
        System.out.println("RESULT=" + spy.greet());
    }
}
