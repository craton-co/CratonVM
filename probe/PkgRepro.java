package pkgtest;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.ObjectMapper;

public class PkgRepro {
    public static class A {
        @JsonProperty("y") private int y;
        public int getY() { return y; }
        public void setY(int v) { y = v; }
    }

    public static void main(String[] args) throws Exception {
        ObjectMapper m = new ObjectMapper();
        A a = new A();
        a.setY(42);
        String s = m.writeValueAsString(a);
        System.out.println("RESULT: " + s);
    }
}
