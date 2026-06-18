import java.lang.reflect.*;
public class Anew {
  public static void main(String[] a){
    System.out.println("new Type[3]      -> "+new Type[3].getClass().getName());
    System.out.println("new Comparable[2]-> "+new Comparable[2].getClass().getName());
    System.out.println("new Number[2]    -> "+new Number[2].getClass().getName());
    System.out.println("new String[2]    -> "+new String[2].getClass().getName());
  }
}
