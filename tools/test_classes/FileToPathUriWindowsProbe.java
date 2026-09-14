import java.io.File;
import java.net.URI;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;

public class FileToPathUriWindowsProbe {
    public static void main(String[] args) throws Exception {
        File file = new File("test_classes/FileToPathUriWindowsProbe.java").getAbsoluteFile();
        Path fromFile = file.toPath();
        String uriText = fromFile.toUri().toString();

        if (File.separatorChar == '\\') {
            if (!uriText.startsWith("file:///")) {
                throw new AssertionError("Windows Path.toUri should use file:/// form: " + uriText);
            }
            if (uriText.contains("%5C") || uriText.indexOf('\\') >= 0) {
                throw new AssertionError("Windows Path.toUri encoded backslashes: " + uriText);
            }
            if (!uriText.contains("/test_classes/")) {
                throw new AssertionError("Windows Path.toUri should use slash separators: " + uriText);
            }
        }

        Path reparsed = Paths.get(new URI(uriText));
        if (Files.exists(fromFile) && !Files.isRegularFile(reparsed)) {
            throw new AssertionError("URI round-trip did not resolve to the source file: " + uriText);
        }

        System.out.println("FileToPathUriWindowsProbe OK " + uriText);
    }
}
