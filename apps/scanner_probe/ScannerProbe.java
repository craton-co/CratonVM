import java.util.Scanner;
public class ScannerProbe {
    public static void main(String[] a) {
        Scanner s = new Scanner(System.in);
        if (s.hasNextLine()) {
            String line = s.nextLine();
            System.out.println("line=" + line);
        } else {
            System.out.println("hasNextLine=false");
        }
        if (s.hasNextInt()) {
            int n = s.nextInt();
            System.out.println("int=" + n);
        } else {
            System.out.println("hasNextInt=false");
        }
        System.out.println("OK");
    }
}
