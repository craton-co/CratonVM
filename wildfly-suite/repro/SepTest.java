import java.nio.file.*;
public class SepTest {
  public static void main(String[] a){
    for(String p: a) System.out.println("["+p+"] nameCount="+Paths.get(p).getNameCount()+" norm=["+Paths.get(p).normalize()+"]");
  }
}
