@echo off
set PATH=C:\Users\Victor\.cargo\bin;%PATH%
"C:\Users\Victor\.cargo\bin\cargo.exe" build --release -p cratonvm-cli --bin cratonvm --target-dir "C:\craton\CratonVM\target\hib-longtail-final-cache-link-20260713" --manifest-path "C:\craton\CratonVM-hib-longtail-final-20260713\Cargo.toml" > "C:\craton\CratonVM\hib-final-h2expr-cache-build-20260713.stdout.log" 2> "C:\craton\CratonVM\hib-final-h2expr-cache-build-20260713.stderr.log"
echo EXIT:%ERRORLEVEL% >> "C:\craton\CratonVM\hib-final-h2expr-cache-build-20260713.status.log"
