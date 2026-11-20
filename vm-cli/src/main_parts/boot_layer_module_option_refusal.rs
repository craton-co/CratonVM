#[allow(unused_imports)]
use super::super::*;

/// zt t14/tg6 (t12 verdicts new defect 1): the exception HotSpot 25's boot
/// layer throws for an `--add-reads` / `--add-exports` / `--add-opens` value
/// it cannot decode, or `None`. libjli lets `--add-opens=` through (it is not
/// on `IsLongFormModuleOption`'s list), and the VM turns every value into a
/// `jdk.module.add{reads,exports,opens}.<n>` property; `ModuleBootstrap`
/// then refuses it, even under `-version` (local 25.0.3, row `tg5_m17`:
/// `java.lang.RuntimeException: Unable to parse --add-opens
/// <module>=<value>: `, rc 1). The rules are JDK 25's `ModuleBootstrap`:
///
/// - `decode`, per value: no `=`, an `=` first, or nothing after it:
///   `Unable to parse <opt> <module>=<value>: <value>`; a right side of
///   commas only: `Target must be specified: <opt> <value>`;
/// - `addExtraExportsOrOpens`, per distinct key of `--add-exports` /
///   `--add-opens`: a key that `String.split("/")` (trailing empty strings
///   dropped) does not cut into two non-empty parts:
///   `Unable to parse <opt> <module>/<package>: <key>`;
/// - in boot order: `--add-reads` decoded, `--add-exports` decoded then its
///   keys, `--add-opens` decoded then its keys. The keys are a `HashMap`
///   there; here they are checked in command-line order, which differs only
///   when one option has two bad keys.
///
/// Not modelled: the unknown-module `WARNING:` lines `addExtraReads` prints
/// before a later refusal. `--patch-module` is decoded earlier, in
/// `ModuleBootstrap`'s static initializer ([`patch_module_refusal`], zt
/// t16/tg7). Pure.
pub(crate) fn boot_layer_module_option_refusal(options: &BootLayerModuleOptions) -> Option<String> {
    fn decode(option: &str, values: &[String]) -> Result<Vec<String>, String> {
        let mut keys: Vec<String> = Vec::new();
        for value in values {
            let unable = || format!("Unable to parse {option} <module>=<value>: {value}");
            let Some(pos) = value.find('=') else {
                return Err(unable());
            };
            let (key, rhs) = (&value[..pos], &value[pos + 1..]);
            if key.is_empty() || rhs.is_empty() {
                return Err(unable());
            }
            if rhs.split(',').all(str::is_empty) {
                return Err(format!("Target must be specified: {option} {value}"));
            }
            if !keys.iter().any(|k| k == key) {
                keys.push(key.to_string());
            }
        }
        Ok(keys)
    }
    fn module_and_package(option: &str, keys: &[String]) -> Result<(), String> {
        for key in keys {
            // `String.split("/")` drops the trailing empty strings.
            let mut parts: Vec<&str> = key.split('/').collect();
            while parts.last() == Some(&"") {
                parts.pop();
            }
            if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
                return Err(format!("Unable to parse {option} <module>/<package>: {key}"));
            }
        }
        Ok(())
    }
    fn in_boot_order(options: &BootLayerModuleOptions) -> Result<(), String> {
        decode("--add-reads", &options.add_reads)?;
        let exports = decode("--add-exports", &options.add_exports)?;
        module_and_package("--add-exports", &exports)?;
        let opens = decode("--add-opens", &options.add_opens)?;
        module_and_package("--add-opens", &opens)
    }
    in_boot_order(options)
        .err()
        .map(|message| format!("java.lang.RuntimeException: {message}"))
}

/// zt t14/tg6: HotSpot's boot-layer abort (`vm_exit_during_initialization`,
/// on STDOUT, as `rewrite_module_launch`'s): the header line, the exception
/// line, exit status 1.
pub(crate) fn exit_with_boot_layer_refusal(exception: &str) -> ! {
    println!("Error occurred during initialization of boot layer\n{exception}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(1);
}



/// `CRATONVM_LAUNCHER_JAR_OPTION_SCAN` (zt t16/tg7; default ON, `=0` kill):
/// libjli's `-jar` is the launch mode, not an option with a value, so the
/// options after it are still options ([`relocate_jar_selector`]). `=0`: the
/// token after `-jar` is the jar, as before. Read from the process
/// environment, as [`jdk_java_options_enabled`]: `main`'s early argv pass
/// runs before the flag snapshot exists.
pub(crate) fn launcher_jar_option_scan_enabled() -> bool {
    None::<std::ffi::OsString>
        .is_none_or(|v| !cratonvm_types::flags::value_is_off(&v.to_string_lossy()))
}

/// `CRATONVM_LAUNCHER_JAR_CLASS_PATH_KEEPS_JAR` (zt t20/tg9, Defect D2 of
/// `t16-row-verdicts.md`; default ON, `=0` kill): under
/// [`launcher_jar_option_scan_enabled`], a `-cp` / `-classpath` /
/// `--class-path` after `-jar` leaves the jar launch in place, as HotSpot 25
/// does (rows `tg7_j3` / `j3_c` / `j6`: `Error: Unable to access jarfile
/// <jar>`, where this launcher read the jar as a main class). `=0`: the
/// class path option drops the `-jar` (the t16 rule). Read from the process
/// environment for the same reason as the scan's own switch.
pub(crate) fn launcher_jar_class_path_keeps_jar_enabled() -> bool {
    None::<std::ffi::OsString>
        .is_none_or(|v| !cratonvm_types::flags::value_is_off(&v.to_string_lossy()))
}

/// `CRATONVM_LAUNCHER_NATIVE_ACCESS_SPACE_FORM` (zt t16/tg7; default ON,
/// `=0` kill): `--enable-native-access <modules>` takes its separate value as
/// libjli does ([`join_enable_native_access_space_form`]). `=0`: only the
/// `=` form takes a value; a bare `--enable-native-access` grants
/// `ALL-UNNAMED` and the next token is the main class, as before. Read from
/// the process environment, as [`launcher_jar_option_scan_enabled`].
pub(crate) fn launcher_native_access_space_form_enabled() -> bool {
    None::<std::ffi::OsString>
        .is_none_or(|v| !cratonvm_types::flags::value_is_off(&v.to_string_lossy()))
}

/// `CRATONVM_LAUNCHER_PATCH_MODULE_REFUSALS` (zt t16/tg7; default ON, `=0`
/// kill): a `--patch-module` value is refused with HotSpot 25's texts and in
/// its order ([`patch_module_refusal`]), on the version path too. `=0`: only
/// the parsed path refuses it, with this launcher's older
/// `IllegalArgumentException` texts, after the module path.
pub(crate) fn launcher_patch_module_refusals_enabled() -> bool {
    true
}

/// `CRATONVM_LAUNCHER_PATCH_MODULE_JAVA_BASE_TWICE` (zt t20/tg9, Defect D4 of
/// `t16-row-verdicts.md`; default ON, `=0` kill). Under
/// [`launcher_patch_module_refusals_enabled`]: `java.base` given twice to
/// `--patch-module` is the VM's own argument parse refusal
/// (`Arguments::process_patch_mod_option`), in command-line order with the
/// `Missing '='` refusal: `Error occurred during initialization of VM` /
/// `Cannot specify java.base more than once to --patch-module` on stdout,
/// nothing on stderr, exit 1 (local 25.0.3, row `tg7_pm8`). Any other module
/// given twice is still `ModuleBootstrap`'s (row `tg7_pm7` SAME). `=0`: the
/// boot layer's `java.base specified more than once` text, as t16.
pub(crate) fn launcher_patch_module_java_base_twice_enabled() -> bool {
    true
}



/// `File.pathSeparator`, the `{0}` of the usage texts' path lines.
pub(crate) const HOTSPOT_PATH_SEPARATOR: &str = if cfg!(windows) { ";" } else { ":" };

/// `java.launcher.opt.header` (JDK 25 `sun.launcher.resources.launcher`), verbatim;
/// `{0}` is filled by [`hotspot_launcher_message`].
pub(crate) const HOTSPOT_USAGE_HEADER: &str = concat!(
    "Usage: {0} [options] <mainclass> [args...]\n",
    "           (to execute a class)\n",
    "   or  {0} [options] -jar <jarfile>.jar [args...]\n",
    "           (to execute a jar file)\n",
    "   or  {0} [options] -m <module>[/<mainclass>] [args...]\n",
    "       {0} [options] --module <module>[/<mainclass>] [args...]\n",
    "           (to execute the main class in a module)\n",
    "   or  {0} [options] <sourcefile>.java [args]\n",
    "           (to execute a source-file program)\n",
    "\n",
    " Arguments following the main class, source file, -jar <jarfile>.jar,\n",
    " -m or --module <module>/<mainclass> are passed as the arguments to\n",
    " main class.\n",
    "\n",
    " where options include:\n",
    "\n",
);

/// `java.launcher.opt.footer` (JDK 25 `sun.launcher.resources.launcher`), verbatim;
/// `{0}` is filled by [`hotspot_launcher_message`].
pub(crate) const HOTSPOT_USAGE_FOOTER: &str = concat!(
    "    -cp <class search path of directories and zip/jar files>\n",
    "    -classpath <class search path of directories and zip/jar files>\n",
    "    --class-path <class search path of directories and zip/jar files>\n",
    "                  A \"{0}\"-separated list of directories, JAR archives,\n",
    "                  and ZIP archives to search for class files.\n",
    "    -p <module path>\n",
    "    --module-path <module path>...\n",
    "                  A \"{0}\"-separated list of elements, each element is a file path\n",
    "                  to a module or a directory containing modules. Each module is either\n",
    "                  a modular JAR or an exploded-module directory.\n",
    "    --upgrade-module-path <module path>...\n",
    "                  A \"{0}\"-separated list of elements, each element is a file path\n",
    "                  to a module or a directory containing modules to replace\n",
    "                  upgradeable modules in the runtime image. Each module is either\n",
    "                  a modular JAR or an exploded-module directory.\n",
    "    --add-modules <module name>[,<module name>...]\n",
    "                  root modules to resolve in addition to the initial module.\n",
    "                  <module name> can also be ALL-DEFAULT, ALL-SYSTEM,\n",
    "                  ALL-MODULE-PATH.\n",
    "    --enable-native-access <module name>[,<module name>...]\n",
    "                  allow code in modules to access code and data outside the Java runtime.\n",
    "                  <module name> can also be ALL-UNNAMED to indicate code on the class path.\n",
    "    --illegal-native-access=<value>\n",
    "                  allow or deny access to code and data outside the Java runtime\n",
    "                  by code in modules for which native access is not explicitly enabled.\n",
    "                  <value> is one of \"deny\", \"warn\" or \"allow\". The default value is \"warn\".\n",
    "                  This option will be removed in a future release.\n",
    "    --list-modules\n",
    "                  list observable modules and exit\n",
    "    -d <module name>\n",
    "    --describe-module <module name>\n",
    "                  describe a module and exit\n",
    "    --dry-run     create VM and load main class but do not execute main method.\n",
    "                  The --dry-run option may be useful for validating the\n",
    "                  command-line options such as the module system configuration.\n",
    "    --validate-modules\n",
    "                  validate all modules and exit\n",
    "                  The --validate-modules option may be useful for finding\n",
    "                  conflicts and other errors with modules on the module path.\n",
    "    -D<name>=<value>\n",
    "                  set a system property\n",
    "    -verbose:[class|module|gc|jni]\n",
    "                  enable verbose output for the given subsystem\n",
    "    -version      print product version to the error stream and exit\n",
    "    --version     print product version to the output stream and exit\n",
    "    -showversion  print product version to the error stream and continue\n",
    "    --show-version\n",
    "                  print product version to the output stream and continue\n",
    "    --show-module-resolution\n",
    "                  show module resolution output during startup\n",
    "    -? -h -help\n",
    "                  print this help message to the error stream\n",
    "    --help        print this help message to the output stream\n",
    "    -X            print help on extra options to the error stream\n",
    "    --help-extra  print help on extra options to the output stream\n",
    "    -ea[:<packagename>...|:<classname>]\n",
    "    -enableassertions[:<packagename>...|:<classname>]\n",
    "                  enable assertions with specified granularity\n",
    "    -da[:<packagename>...|:<classname>]\n",
    "    -disableassertions[:<packagename>...|:<classname>]\n",
    "                  disable assertions with specified granularity\n",
    "    -esa | -enablesystemassertions\n",
    "                  enable system assertions\n",
    "    -dsa | -disablesystemassertions\n",
    "                  disable system assertions\n",
    "    -agentlib:<libname>[=<options>]\n",
    "                  load native agent library <libname>, e.g. -agentlib:jdwp\n",
    "                  see also -agentlib:jdwp=help\n",
    "    -agentpath:<pathname>[=<options>]\n",
    "                  load native agent library by full pathname\n",
    "    -javaagent:<jarpath>[=<options>]\n",
    "                  load Java programming language agent, see java.lang.instrument\n",
    "    -splash:<imagepath>\n",
    "                  show splash screen with specified image\n",
    "                  HiDPI scaled images are automatically supported and used\n",
    "                  if available. The unscaled image filename, e.g. image.ext,\n",
    "                  should always be passed as the argument to the -splash option.\n",
    "                  The most appropriate scaled image provided will be picked up\n",
    "                  automatically.\n",
    "                  See the SplashScreen API documentation for more information\n",
    "    @argument files\n",
    "                  one or more argument files containing options\n",
    "    --disable-@files\n",
    "                  prevent further argument file expansion\n",
    "    --enable-preview\n",
    "                  allow classes to depend on preview features of this release\n",
    "To specify an argument for a long option, you can use --<name>=<value> or\n",
    "--<name> <value>.\n",
);

/// `java.launcher.opt.concise.header` (JDK 25 `sun.launcher.resources.launcher`), verbatim;
/// `{0}` is filled by [`hotspot_launcher_message`].
pub(crate) const HOTSPOT_CONCISE_USAGE: &str = concat!(
    "Usage: java [java options...] <application> [application arguments...]\n",
    "\n",
    "Where <application> is one of:\n",
    "  <mainclass>                to execute the main method of a compiled main class\n",
    "  -jar <jarfile>.jar         to execute the main class of a JAR archive\n",
    "  -m <module>[/<mainclass>]  to execute the main class of a module\n",
    "  <sourcefile>.java          to compile and execute a source-file program\n",
    "\n",
    "Where key java options include:\n",
    "  --class-path <class path>\n",
    "    where <class path> is a list of directories and JAR archives to search for class files, separated by \"{0}\"\n",
    "  --module-path <module path>\n",
    "    where <module path> is a list of directories and JAR archives to search for modules, separated by \"{0}\"\n",
    "  -version\n",
    "    to print product version to the error stream and exit\n",
    "\n",
    "For additional help on usage:           java --help\n",
    "For an interactive Java environment:    jshell",
);

/// zt t16/tg7: `LauncherHelper.getLocalizedMessage`'s `MessageFormat` for
/// the usage texts, which hold no quote and no pattern element but `{0}`.
pub(crate) fn hotspot_launcher_message(text: &str, arg0: &str) -> String {
    text.replace("{0}", arg0)
}

/// zt t16/tg7: the full usage libjli's `PrintUsage` prints when
/// `printUsage` is set (`-help`, an `ARG_CHECK` refusal):
/// `LauncherHelper.initHelpMessage` with the program name `java` (the
/// launcher's `PROGNAME`, on every platform), no `vmselect` / `hotspot`
/// lines (they list `jvm.cfg`'s VMs past the default one, and JDK 25's
/// file has only `-server KNOWN` and `-client IGNORE`), then
/// `printHelpMessage`'s footer. Without `println`'s last line break.
///
/// zt t26/rt2 (`CRATONVM_LAUNCHER_HELP_JVM_CFG_VMS`): the `vmselect` /
/// `hotspot` lines are the class library's own `lib/jvm.cfg`'s
/// ([`hotspot_vm_select_lines`]); a distribution's file can list more VMs
/// (Ubuntu's `openjdk-25` lists `-zero KNOWN` and `-dcevm KNOWN`, and its
/// HotSpot prints both, t18 `tg8_h4`).
pub(crate) fn hotspot_full_usage_text() -> String {
    hotspot_full_usage_text_with(&hotspot_vm_select_lines())
}

/// [`hotspot_full_usage_text`] with the `vmselect` / `hotspot` lines given.
/// Pure.
pub(crate) fn hotspot_full_usage_text_with(vm_select: &str) -> String {
    format!(
        "{}{}{}",
        hotspot_launcher_message(HOTSPOT_USAGE_HEADER, "java"),
        vm_select,
        hotspot_launcher_message(HOTSPOT_USAGE_FOOTER, HOTSPOT_PATH_SEPARATOR)
    )
}



/// zt t26/rt2: the `vmselect` / `hotspot` lines of the full usage for the
/// class library found by the default search (`JAVA_HOME`, `PATH`; the help
/// is printed before any `--java-home` is parsed), or nothing when the
/// switch is off, no JDK is found or it has no readable `lib/jvm.cfg`.
pub(crate) fn hotspot_vm_select_lines() -> String {
    let Ok(home) = cratonvm_vm::config::require_real_jdk(None) else {
        return String::new();
    };
    match std::fs::read_to_string(home.join("lib").join("jvm.cfg")) {
        Ok(text) => jvm_cfg_vm_select_lines(&text),
        Err(_) => String::new(),
    }
}

/// zt t26/rt2: libjli's `PrintUsage` over a `jvm.cfg` text, read as
/// `ReadKnownVMs` reads it: a line starting with `#` is a comment; the first
/// whitespace-separated token is the VM's name, the second its flag
/// (compared by prefix, `JLI_StrCCmp`): `KNOWN`, `ALIASED_TO <name>`,
/// `WARN`, `IGNORE`, `ERROR`, `IF_SERVER_CLASS ...`; an unknown flag is
/// `KNOWN` (libjli also warns on stderr, not modelled); a line without a
/// flag is no VM. Then, past the first VM (the default), each `KNOWN` VM's
/// `java.launcher.opt.vmselect` line (`    {0}\t  to select the "{1}" VM`),
/// then each `ALIASED_TO` VM's `java.launcher.opt.hotspot` line
/// (`    {0}\t  is a synonym for the "{1}" VM  [deprecated]`), `{1}` the
/// name (or alias) without its `-`. Pure.
pub(crate) fn jvm_cfg_vm_select_lines(cfg: &str) -> String {
    enum Kind {
        Known,
        Aliased(String),
        Other,
    }
    let mut vms: Vec<(String, Kind)> = Vec::new();
    for line in cfg.lines() {
        if line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let (Some(name), Some(flag)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        let kind = if flag.starts_with("KNOWN") {
            Kind::Known
        } else if flag.starts_with("ALIASED_TO") {
            match tokens.next() {
                Some(alias) => Kind::Aliased(alias.to_string()),
                None => continue,
            }
        } else if ["WARN", "IGNORE", "ERROR", "IF_SERVER_CLASS"]
            .iter()
            .any(|f| flag.starts_with(f))
        {
            Kind::Other
        } else {
            Kind::Known
        };
        vms.push((name.to_string(), kind));
    }
    let bare = |name: &str| name.strip_prefix('-').unwrap_or(name).to_string();
    let mut out = String::new();
    for (name, kind) in vms.iter().skip(1) {
        if matches!(kind, Kind::Known) {
            out.push_str(&format!("    {name}\t  to select the \"{}\" VM\n", bare(name)));
        }
    }
    for (name, kind) in vms.iter().skip(1) {
        if let Kind::Aliased(alias) = kind {
            out.push_str(&format!(
                "    {name}\t  is a synonym for the \"{}\" VM  [deprecated]\n",
                bare(alias)
            ));
        }
    }
    out
}

/// zt t16/tg7: the concise usage libjli's `PrintUsage` prints when no
/// program is named and `printUsage` is not set (`java`, `java -cp x`,
/// `java -Xmx64m`; `LauncherHelper.printConciseUsageMessage`, JDK 22+).
/// Without `println`'s last line break.
pub(crate) fn hotspot_concise_usage_text() -> String {
    hotspot_launcher_message(HOTSPOT_CONCISE_USAGE, HOTSPOT_PATH_SEPARATOR)
}

/// zt t16/tg7: libjli's refusal of a `-jar` with nothing after it:
/// `ARG_CHECK` prints `ARG_ERROR2` on stderr, sets `printUsage` and exit
/// status 1, and the launcher then prints the full usage on stderr. Printed
/// with `eprintln!`, this is HotSpot's stderr byte for byte.
pub(crate) fn bare_jar_refusal_text() -> String {
    format!("{BARE_JAR_ERROR_LINE}\n{}", hotspot_full_usage_text())
}

/// libjli's `ARG_ERROR2` for `-jar` (the first line of
/// [`bare_jar_refusal_text`]).
pub(crate) const BARE_JAR_ERROR_LINE: &str = "Error: -jar requires jar file specification";

/// `CRATONVM_LAUNCHER_BARE_JAR_XLOG_BOOT` (zt t27/lg1, page
/// `zgc-zt12tg5-*` item 5; default ON, `=0` kill). libjli's `ARG_CHECK` for a
/// bare `-jar` prints its error line while it parses, sets `printUsage` and
/// goes on: `JavaMain` creates the VM, then prints the full usage and exits 1.
/// So with an `-Xlog` selection HotSpot's startup lines (`Using The Z Garbage
/// Collector`, ...) are printed between the two, and its exit lines after
/// them. Under this switch the launcher prints the error line, then the usage
/// from a booted VM, as the concise usage under `-Xlog` already does
/// (`CRATONVM_LAUNCHER_USAGE_XLOG_BOOT`, [`usage_query_boots_vm`]); without an
/// `-Xlog` selection nothing changes. `=0`: both texts, no VM, exit 1, as
/// before.
pub(crate) fn launcher_bare_jar_xlog_boot_enabled() -> bool {
    true
}

/// `java.launcher.X.usage` (JDK 25 `sun.launcher.resources.launcher`), verbatim
/// (zt t18/tg8); `{0}` is filled by [`hotspot_launcher_message`].
pub(crate) const HOTSPOT_X_USAGE: &str = concat!(
    "\n",
    "    -Xbatch           disable background compilation\n",
    "    -Xbootclasspath/a:<directories and zip/jar files separated by {0}>\n",
    "                      append to end of bootstrap class path\n",
    "    -Xcheck:jni       perform additional checks for JNI functions\n",
    "    -Xcomp            forces compilation of methods on first invocation\n",
    "    -Xdebug           does nothing; deprecated for removal in a future release.\n",
    "    -Xdiag            show additional diagnostic messages\n",
    "    -Xint             interpreted mode execution only\n",
    "    -Xinternalversion\n",
    "                      displays more detailed JVM version information than the\n",
    "                      -version option\n",
    "    -Xlog:<opts>      Configure or enable logging with the Java Virtual\n",
    "                      Machine (JVM) unified logging framework. Use -Xlog:help\n",
    "                      for details.\n",
    "    -Xloggc:<file>    log GC status to a file with time stamps.\n",
    "                      This option is deprecated and may be removed in a\n",
    "                      future release. It is replaced by -Xlog:gc:<file>.\n",
    "    -Xmixed           mixed mode execution (default)\n",
    "    -Xmn<size>        sets the initial and maximum size (in bytes) of the heap\n",
    "                      for the young generation (nursery)\n",
    "    -Xms<size>        set minimum and initial Java heap size\n",
    "    -Xmx<size>        set maximum Java heap size\n",
    "    -Xnoclassgc       disable class garbage collection\n",
    "    -Xrs              reduce use of OS signals by Java/VM (see documentation)\n",
    "    -Xshare:auto      use shared class data if possible (default)\n",
    "    -Xshare:off       do not attempt to use shared class data\n",
    "    -Xshare:on        require using shared class data, otherwise fail.\n",
    "                      This is a testing option and may lead to intermittent\n",
    "                      failures. It should not be used in production environments.\n",
    "    -XshowSettings    show all settings and continue\n",
    "    -XshowSettings:all\n",
    "                      show all settings in verbose detail and continue\n",
    "    -XshowSettings:locale\n",
    "                      show all locale related settings and continue\n",
    "    -XshowSettings:properties\n",
    "                      show all property settings and continue\n",
    "    -XshowSettings:vm\n",
    "                      show all vm related settings and continue\n",
    "    -XshowSettings:security\n",
    "                      show all security settings and continue\n",
    "    -XshowSettings:security:all\n",
    "                      show all security settings and continue\n",
    "    -XshowSettings:security:properties\n",
    "                      show security properties and continue\n",
    "    -XshowSettings:security:providers\n",
    "                      show static security provider settings and continue\n",
    "    -XshowSettings:security:tls\n",
    "                      show TLS related security settings and continue\n",
    "    -XshowSettings:system\n",
    "                      (Linux Only) show host system or container\n",
    "                      configuration and continue\n",
    "    -Xss<size>        set java thread stack size\n",
    "                      The actual size may be rounded up to a multiple of the\n",
    "                      system page size as required by the operating system.\n",
    "    -Xverify          sets the mode of the bytecode verifier\n",
    "                      Note that option -Xverify:none is deprecated and\n",
    "                      may be removed in a future release.\n",
    "    --add-reads <module>=<target-module>(,<target-module>)*\n",
    "                      updates <module> to read <target-module>, regardless\n",
    "                      of module declaration. \n",
    "                      <target-module> can be ALL-UNNAMED to read all unnamed\n",
    "                      modules.\n",
    "    --add-exports <module>/<package>=<target-module>(,<target-module>)*\n",
    "                      updates <module> to export <package> to <target-module>,\n",
    "                      regardless of module declaration.\n",
    "                      <target-module> can be ALL-UNNAMED to export to all\n",
    "                      unnamed modules.\n",
    "    --add-opens <module>/<package>=<target-module>(,<target-module>)*\n",
    "                      updates <module> to open <package> to\n",
    "                      <target-module>, regardless of module declaration.\n",
    "    --limit-modules <module name>[,<module name>...]\n",
    "                      limit the universe of observable modules\n",
    "    --patch-module <module>=<file>({0}<file>)*\n",
    "                      override or augment a module with classes and resources\n",
    "                      in JAR files or directories.\n",
    "    --source <version>\n",
    "                      set the version of the source in source-file mode.\n",
    "    --finalization=<value>\n",
    "                      controls whether the JVM performs finalization of objects,\n",
    "                      where <value> is one of \"enabled\" or \"disabled\".\n",
    "                      Finalization is enabled by default.\n",
    "    --sun-misc-unsafe-memory-access=<value>\n",
    "                      allow or deny usage of unsupported API sun.misc.Unsafe\n",
    "                      <value> is one of \"allow\", \"warn\", \"debug\", or \"deny\".\n",
    "                      The default value is \"warn\".\n",
    "\n",
    "These extra options are subject to change without notice.\n",
);

/// `java.launcher.X.macosx.usage` (JDK 25 `sun.launcher.resources.launcher`),
/// verbatim (zt t18/tg8). It holds no `{0}`.
pub(crate) const HOTSPOT_X_MACOSX_USAGE: &str = concat!(
    "\n",
    "The following options are macOS specific:\n",
    "    -XstartOnFirstThread\n",
    "                      run the main() method on the first (AppKit) thread\n",
    "    -Xdock:name=<application name>\n",
    "                      override default application name displayed in dock\n",
    "    -Xdock:icon=<path to icon file>\n",
    "                      override default icon displayed in dock\n",
    "\n",
);

/// zt t18/tg8: `LauncherHelper.printXUsageMessage`'s text (`-X`,
/// `--help-extra`): `java.launcher.X.usage` with `File.pathSeparator`, then,
/// on macOS only, a second `println` of `java.launcher.X.macosx.usage`.
/// Without the last `println`'s line break.
pub(crate) fn hotspot_extra_usage_text() -> String {
    let usage = hotspot_launcher_message(HOTSPOT_X_USAGE, HOTSPOT_PATH_SEPARATOR);
    if cfg!(target_os = "macos") {
        format!("{usage}\n{HOTSPOT_X_MACOSX_USAGE}")
    } else {
        usage
    }
}

/// zt t18/tg8: the help query of a launch (libjli `ParseArguments`): which
/// text HotSpot's `PrintUsage` prints, and on which stream (`printTo`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HotspotHelpQuery {
    /// `-help`, `-h`, `-?`: the full usage on stderr.
    UsageToStderr,
    /// `--help`: the full usage on stdout.
    UsageToStdout,
    /// `-X`: the extra options on stderr.
    ExtraToStderr,
    /// `--help-extra`: the extra options on stdout.
    ExtraToStdout,
}

impl HotspotHelpQuery {
    pub(crate) fn of(arg: &str) -> Option<Self> {
        Some(match arg {
            "-help" | "-h" | "-?" => HotspotHelpQuery::UsageToStderr,
            "--help" => HotspotHelpQuery::UsageToStdout,
            "-X" => HotspotHelpQuery::ExtraToStderr,
            "--help-extra" => HotspotHelpQuery::ExtraToStdout,
            _ => return None,
        })
    }

    /// The text, without `println`'s last line break.
    pub(crate) fn text(self) -> String {
        match self {
            HotspotHelpQuery::UsageToStderr | HotspotHelpQuery::UsageToStdout => {
                hotspot_full_usage_text()
            }
            HotspotHelpQuery::ExtraToStderr | HotspotHelpQuery::ExtraToStdout => {
                hotspot_extra_usage_text()
            }
        }
    }

    pub(crate) fn to_stdout(self) -> bool {
        matches!(
            self,
            HotspotHelpQuery::UsageToStdout | HotspotHelpQuery::ExtraToStdout
        )
    }

    /// HotSpot's answer: the text on its stream, exit status 0 (libjli
    /// returns from `ParseArguments` at the query with `ret` still 0).
    pub(crate) fn print_and_exit(self) -> ! {
        use std::io::Write as _;
        let text = self.text();
        if self.to_stdout() {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{text}");
            let _ = out.flush();
        } else {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{text}");
            let _ = err.flush();
        }
        std::process::exit(0);
    }
}



/// zt t18/tg8: this launcher's own help option. It prints clap's long help
/// for [`Args`] ([`LONG_ABOUT`] and every CratonVM flag) on stdout, exit
/// status 0, wherever it stands in the leading option section
/// ([`print_cratonvm_help_if_asked`]). HotSpot has no option of that name.
pub(crate) const CRATONVM_HELP_OPTION: &str = "--cratonvm-help";

/// zt t18/tg8: does the leading option section of `argv` (the launch argv,
/// before [`insert_program_args_separator`]) name [`CRATONVM_HELP_OPTION`]?
/// The section ends at `--` or the first token that is not an option; the
/// value of a value-taking option is skipped. Pure.
pub(crate) fn cratonvm_help_requested(argv: &[String]) -> bool {
    let mut i = 1usize;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        if arg == CRATONVM_HELP_OPTION {
            return true;
        }
        if arg == "--" || !arg.starts_with('-') {
            return false;
        }
        i += if VALUE_TAKING_OPTS.contains(&arg) {
            2
        } else {
            1
        };
    }
    false
}

/// zt t18/tg8: [`CRATONVM_HELP_OPTION`]: clap's long help on stdout, exit 0.
pub(crate) fn print_cratonvm_help_if_asked(argv: &[String]) {
    if !cratonvm_help_requested(argv) {
        return;
    }
    let mut command = <Args as clap::CommandFactory>::command();
    let _ = command.print_long_help();
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(0);
}

/// zt t18/tg8 (`CRATONVM_LAUNCHER_HOTSPOT_HELP_TEXT`): libjli's help query.
/// `ParseArguments` reads the leading option section in order and returns
/// at the first version query, help option or `-X` (`LAUNCHER_SCAN_RETURNS`):
/// when that token is a help option ([`HotspotHelpQuery::of`]), the options
/// before it are the VM's (they are parsed, and refused, as on any launch),
/// and the tokens from it on are never read. So argv is cut at the help
/// option and the query returned; [`run`] prints it where it prints the
/// concise usage, after those options were accepted. The section ends, with
/// `None`, at a version query, `--`, the program selector (a bare token, a
/// `-m` / `--module` module, this launcher's `--jar` / `-jar=` spellings) or
/// a `-jar` under the old `-jar` rule (`scan_past_jar` false). Values are
/// skipped as [`relocate_jar_selector`] skips them. Pure.
pub(crate) fn take_hotspot_help_query(
    argv: &mut Vec<String>,
    scan_past_jar: bool,
) -> Option<HotspotHelpQuery> {
    let mut i = 1usize;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        if let Some(query) = HotspotHelpQuery::of(arg) {
            argv.truncate(i);
            return Some(query);
        }
        if LAUNCHER_SCAN_RETURNS.contains(&arg) || !arg.starts_with('-') {
            return None;
        }
        if arg == "-jar" {
            if scan_past_jar {
                i += 1;
                continue;
            }
            return None;
        }
        if arg == "--jar"
            || arg.starts_with("-jar=")
            || arg.starts_with("--jar=")
            || arg == "-m"
            || arg == "--module"
            || arg.starts_with("--module=")
        {
            return None;
        }
        let next_is_value = argv.get(i + 1).is_some_and(|next| !next.starts_with('-'));
        i += if jdk_java_options_takes_value(arg) {
            if next_is_value {
                2            } else {
                1            }        } else if VALUE_TAKING_OPTS.contains(&arg) {
            2
        } else {
            1
        };
    }
    None
}

/// zt t16/tg7 (`CRATONVM_LAUNCHER_JAR_OPTION_SCAN`): libjli's `-jar`.
/// `ParseArguments` reads `-jar` as the launch mode only (`mode = LM_JAR`):
/// the options after it are still options, and the jar is the first token
/// of the leading section that is not one (`*pwhat`). By that rule
/// `-jar -version` prints the banner and `-jar -Xmx64m app.jar` runs
/// `app.jar` with `-Xmx64m`; this launcher took the token after `-jar` as
/// the jar. Here argv is reshaped to the spelling this launcher reads: the
/// `-jar` moves to just before the jar.
///
/// - A later module launch (`-m`, `--module`, `--module=`) replaces `-jar`,
///   as `checkMode` does. The `-jar` is dropped. A class path option after
///   `-jar` does not (zt t20/tg9, [`launcher_jar_class_path_keeps_jar_enabled`]):
///   the jar launch stands and the class path is ignored, as HotSpot 25.
/// - A `-jar` whose section ends without a jar (a version or help query,
///   the end of argv) is dropped: the query is answered, or no program is
///   named ([`hotspot_concise_usage_text`]).
/// - `--source`, an explicit `--` and this launcher's own `--jar` / `-jar=`
///   / `--jar=` spellings keep argv unchanged.
///
/// A `-jar` with nothing after it is refused before this runs
/// ([`bare_jar_refusal_text`]). Reads its zt t20 switch; the rule itself is
/// [`relocate_jar_selector_with`].
pub(crate) fn relocate_jar_selector(argv: Vec<String>) -> Vec<String> {
    relocate_jar_selector_with(argv, launcher_jar_class_path_keeps_jar_enabled())
}

/// [`relocate_jar_selector`] with its zt t20/tg9 switch given:
/// `class_path_keeps_jar` false is the t16 rule (a class path option after
/// `-jar` dropped it). Pure.
pub(crate) fn relocate_jar_selector_with(argv: Vec<String>, class_path_keeps_jar: bool) -> Vec<String> {
    let mut live: Option<usize> = None;
    let mut dropped: Vec<usize> = Vec::new();
    let mut jar_file: Option<usize> = None;
    let mut i = 1usize;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        if arg == "--"
            || arg == "--source"
            || arg.starts_with("--source=")
            || arg == "--jar"
            || arg.starts_with("-jar=")
            || arg.starts_with("--jar=")
        {
            return argv;
        }
        if LAUNCHER_SCAN_RETURNS.contains(&arg) {
            break;
        }
        if !arg.starts_with('-') {
            if live.is_some() {
                jar_file = Some(i);
            }
            break;
        }
        if arg == "-jar" {
            dropped.extend(live.replace(i));
            i += 1;
            continue;
        }
        let module_launch = arg == "-m" || arg == "--module" || arg.starts_with("--module=");
        // zt t20/tg9 (Defect D2 of `t16-row-verdicts.md`): a class path option
        // after `-jar` does not end the jar launch on HotSpot 25 (rows
        // `tg7_j3` / `j3_c` / `j6`: `-jar -Xmx64m -cp <dir> nosuch.jar` prints
        // `Error: Unable to access jarfile nosuch.jar`); the class path is
        // recorded and the jar replaces it. Only a module launch replaces
        // `-jar`. `CRATONVM_LAUNCHER_JAR_CLASS_PATH_KEEPS_JAR=0`: the t16 rule
        // (a later class path option dropped the `-jar`).
        let class_path = matches!(arg, "-cp" | "-classpath" | "--class-path")
            || arg.starts_with("--class-path=");
        if module_launch || (class_path && !class_path_keeps_jar) {
            dropped.extend(live.take());
            if module_launch {
                break;
            }
        }
        // libjli's white-space options take the next token only when it does
        // not start with `-`; this launcher's own value options take it.
        let next_is_value = argv.get(i + 1).is_some_and(|next| !next.starts_with('-'));
        i += if jdk_java_options_takes_value(arg) {
            if next_is_value {
                2            } else {
                1            }        } else if VALUE_TAKING_OPTS.contains(&arg) {
            2
        } else {
            1
        };
    }
    let jar_at = match (live, jar_file) {
        (Some(at), Some(_)) => Some(at),
        (Some(at), None) => {
            dropped.push(at);
            None
        }
        (None, _) => None,
    };
    if dropped.is_empty() && jar_at.zip(jar_file).is_none_or(|(at, file)| at + 1 == file) {
        return argv;
    }
    let mut out = Vec::with_capacity(argv.len());
    for (k, token) in argv.into_iter().enumerate() {
        if dropped.contains(&k) || jar_at == Some(k) {
            continue;
        }
        if jar_at.is_some() && jar_file == Some(k) {
            out.push("-jar".to_string());
        }
        out.push(token);
    }
    out
}

/// zt t16/tg7 (`CRATONVM_LAUNCHER_NATIVE_ACCESS_SPACE_FORM`; page
/// `zgc-zt12tg5-*` item 5): libjli's `--enable-native-access` is a
/// white-space option (`IsModuleOption`): the next token is its value when
/// it does not start with `-`, so `--enable-native-access ALL-UNNAMED Main`
/// runs `Main` with the grant, and `--enable-native-access Main` names no
/// program (HotSpot prints the concise usage). clap declares the option with
/// `require_equals`, so the separate value is joined into the `=` form here,
/// in the leading section only (the section ends as in
/// [`relocate_jar_selector`]). An empty value is left for the missing-value
/// refusal. Pure.
pub(crate) fn join_enable_native_access_space_form(mut argv: Vec<String>) -> Vec<String> {
    let mut i = 1usize;
    while i < argv.len() {
        let arg = argv[i].clone();
        let arg = arg.as_str();
        if LAUNCHER_SCAN_RETURNS.contains(&arg) || !arg.starts_with('-') {
            break;
        }
        if arg == "-m" || arg == "--module" || arg.starts_with("--module=") {
            break;
        }
        let next = argv.get(i + 1).filter(|next| !next.starts_with('-'));
        let next_is_value = next.is_some();
        if arg == "--enable-native-access" {
            if next.is_some_and(|next| !next.is_empty()) {
                let value = argv.remove(i + 1);
                argv[i] = format!("--enable-native-access={value}");
            }
            i += 1;
            continue;
        }
        if arg == "-jar" {
            // The launch mode; the jar ends the section.
            if next_is_value {
                break;
            }
            i += 1;
            continue;
        }
        i += if jdk_java_options_takes_value(arg) {
            if next_is_value {
                2            } else {
                1            }        } else if VALUE_TAKING_OPTS.contains(&arg) {
            2
        } else {
            1
        };
    }
    argv}

/// zt t16/tg7: the launch argv reshaped to HotSpot's reading of the leading
/// section before every later stage, under their switches:
/// [`join_enable_native_access_space_form`], then [`relocate_jar_selector`].
/// Both `main`'s early argv pass and `run` take it.
pub(crate) fn hotspot_launcher_argv_shape(argv: Vec<String>) -> Vec<String> {
    let argv = if launcher_native_access_space_form_enabled() {
        join_enable_native_access_space_form(argv)
    } else {
        argv
    };
    if launcher_jar_option_scan_enabled() {
        relocate_jar_selector(argv)
    } else {
        argv
    }
}

/// zt t16/tg7: how HotSpot 25 refuses a `--patch-module` value
/// ([`patch_module_refusal`]).
#[derive(Debug, PartialEq)]
pub(crate) enum PatchModuleRefusal {
    /// `Arguments::process_patch_mod_option`: a value with no `=` fails the
    /// VM's own argument parse. `Missing '=' in --patch-module
    /// specification` on stdout (`defaultStream::output_stream()`), then
    /// libjli's two lines for a failed `JNI_CreateJavaVM` on stderr.
    MissingEquals,
    /// zt t20/tg9 (`CRATONVM_LAUNCHER_PATCH_MODULE_JAVA_BASE_TWICE`): the same
    /// parse's `vm_exit_during_initialization("Cannot specify java.base more
    /// than once to --patch-module")` at the second `java.base` value:
    /// `Error occurred during initialization of VM` and that line on stdout,
    /// nothing on stderr.
    JavaBaseTwice,
    /// `ModuleBootstrap.decode("jdk.module.patch.", File.pathSeparator,
    /// false)`, which `ModuleBootstrap`'s static initializer runs
    /// (`initModulePatcher`): the `RuntimeException`'s message. It is thrown
    /// inside `<clinit>`, so `System.initPhase2` reports an
    /// `ExceptionInInitializerError` caused by it
    /// ([`boot_layer_patch_exception`]).
    BootLayer(String),
}

/// zt t16/tg7 (`CRATONVM_LAUNCHER_PATCH_MODULE_REFUSALS`): HotSpot 25's
/// refusal of the `--patch-module` values, in command-line order, or `None`.
///
/// - The VM's argument parse, over every value first: no `=` is
///   [`PatchModuleRefusal::MissingEquals`].
/// - Then `ModuleBootstrap.decode`, per value: an `=` first or nothing after
///   it, `Unable to parse --patch-module <module>=<value>: <value>`; a module
///   given twice (`allowDuplicates` false), `<module> specified more than
///   once to --patch-module`; a right side of path separators only, `Target
///   must be specified: --patch-module <value>`.
///
/// The boot layer decodes these before it scans the module path or decodes
/// `--add-reads` / `--add-exports` / `--add-opens`.
///
/// zt t20/tg9: the VM's parse also refuses `java.base` given twice
/// ([`PatchModuleRefusal::JavaBaseTwice`]), value by value in command-line
/// order with `Missing '='` (the first of the two met wins), under
/// [`launcher_patch_module_java_base_twice_enabled`]; the rule is
/// [`patch_module_refusal_with`].
pub(crate) fn patch_module_refusal(values: &[String]) -> Option<PatchModuleRefusal> {
    patch_module_refusal_with(values, launcher_patch_module_java_base_twice_enabled())
}

/// [`patch_module_refusal`] with the zt t20 switch given (`java_base_twice`
/// false: t16's rule, where the VM parse refused only a missing `=`). Pure.
pub(crate) fn patch_module_refusal_with(
    values: &[String],
    java_base_twice: bool,
) -> Option<PatchModuleRefusal> {
    // `Arguments::process_patch_mod_option`, once per value in command-line
    // order: no `=` returns `JNI_ERR` at once; a second `java.base` exits.
    let mut java_base_seen = false;
    for value in values {
        let Some((module, _)) = value.split_once('=') else {
            return Some(PatchModuleRefusal::MissingEquals);
        };
        if java_base_twice && module == "java.base" {
            if java_base_seen {
                return Some(PatchModuleRefusal::JavaBaseTwice);
            }
            java_base_seen = true;
        }
    }
    let mut keys: Vec<&str> = Vec::new();
    for value in values {
        let Some(pos) = value.find('=') else {
            return Some(PatchModuleRefusal::MissingEquals);
        };
        let (key, rhs) = (&value[..pos], &value[pos + 1..]);
        if key.is_empty() || rhs.is_empty() {
            return Some(PatchModuleRefusal::BootLayer(format!(
                "Unable to parse --patch-module <module>=<value>: {value}"
            )));
        }
        if keys.contains(&key) {
            return Some(PatchModuleRefusal::BootLayer(format!(
                "{key} specified more than once to --patch-module"
            )));
        }
        keys.push(key);
        if rhs.split(HOTSPOT_PATH_SEPARATOR).all(str::is_empty) {
            return Some(PatchModuleRefusal::BootLayer(format!(
                "Target must be specified: --patch-module {value}"
            )));
        }
    }
    None
}

/// zt t16/tg7: the exception lines `System.initPhase2`'s `logInitException`
/// prints for a [`PatchModuleRefusal::BootLayer`] (`log.println(e)`, then
/// `Caused by: <cause>`; no stack trace without `-Xlog:init=debug`).
pub(crate) fn boot_layer_patch_exception(message: &str) -> String {
    format!(
        "java.lang.ExceptionInInitializerError\nCaused by: java.lang.RuntimeException: {message}"
    )
}

/// zt t16/tg7: HotSpot's output for a [`PatchModuleRefusal`], exit status 1.
pub(crate) fn exit_with_patch_module_refusal(refusal: &PatchModuleRefusal) -> ! {
    match refusal {
        PatchModuleRefusal::MissingEquals => {
            println!("Missing '=' in --patch-module specification");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            eprintln!("Error: Could not create the Java Virtual Machine.");
            eprintln!("Error: A fatal exception has occurred. Program will exit.");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            std::process::exit(1);
        }
        PatchModuleRefusal::JavaBaseTwice => exit_with_vm_initialization_error(
            "Cannot specify java.base more than once to --patch-module",
        ),
        PatchModuleRefusal::BootLayer(message) => {
            exit_with_boot_layer_refusal(&boot_layer_patch_exception(message))
        }
    }
}

/// zt t16/tg7 (`CRATONVM_LAUNCHER_VERSION_MODULE_PATH_CHECK`): the text of
/// the boot layer's `FindException` for the module path `module_path` of a
/// launch argv, or `None`, as the parsed path reads it (`module_path_find_error_at`
/// at the booted `JarFile.runtimeVersion()`). The java home and the JDK mode
/// are read from argv as [`rewrite_module_launch`] reads them.
pub(crate) fn launcher_module_path_find_error(argv: &[String], module_path: &str) -> Option<String> {
    let entries = VmConfig::parse_classpath(module_path);
    let java_home = scan_explicit_java_home(argv).or_else(|| std::env::var("JAVA_HOME").ok());
    let booted = (!matches!(
        scan_requested_jdk_mode(argv),
        cratonvm_vm::config::JdkMode::Synthetic
    ))
    .then(|| detect_jdk_feature(java_home.as_deref()))
    .flatten();
    let feature = launch_jar_runtime_feature(
        booted,
        argv_system_property(argv, "jdk.util.jar.version"),
        argv_system_property(argv, "jdk.util.jar.enableMultiRelease"),
    );
    cratonvm_classloading::module::module_path_find_error_at(&entries, feature)
}

pub(crate) fn insert_program_args_separator(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let mut out = vec![args[0].clone()];
    let mut i = 1usize;
    while i < args.len() {
        let a = args[i].as_str();
        // An explicit separator already delimits the program args -- respect
        // it and copy the remainder verbatim.
        if a == "--" {
            out.extend_from_slice(&args[i..]);
            return out;
        }
        // `-jar <jar>`: the jar is the selector. Copy `-jar` and its operand,
        // then insert `--` so the rest of argv is program args (unless the
        // caller already placed an explicit `--` there).
        if (a == "-jar" || a == "--jar") && i + 1 < args.len() {
            out.push(args[i].clone());
            out.push(args[i + 1].clone());
            if args.get(i + 2).map(String::as_str) != Some("--") {
                out.push("--".into());
            }
            out.extend_from_slice(&args[i + 2..]);
            return out;
        }
        // Inline `--jar=<jar>` / `-jar=<jar>` form.
        if a.starts_with("-jar=") || a.starts_with("--jar=") {
            out.push(args[i].clone());
            if args.get(i + 1).map(String::as_str) != Some("--") {
                out.push("--".into());
            }
            out.extend_from_slice(&args[i + 1..]);
            return out;
        }
        // An option that consumes the next token as its value -- copy both
        // and keep scanning; the value token is not the main-class name.
        if VALUE_TAKING_OPTS.contains(&a) {
            out.push(args[i].clone());
            if i + 1 < args.len() {
                out.push(args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Any other `-`/`--`-prefixed token in the leading section is a
        // launcher option (boolean flag or inline `--opt=value`) -- copy and
        // continue scanning.
        if a.starts_with('-') {
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        // First bare token: the main-class name. It selects the program;
        // insert `--` right after it so all following tokens are program
        // args (unless an explicit `--` already follows).
        out.push(args[i].clone());
        if args.get(i + 1).map(String::as_str) != Some("--") {
            out.push("--".into());
        }
        out.extend_from_slice(&args[i + 1..]);
        return out;
    }
    out
}

/// Launcher options that are **silently discarded** when they appear after the
/// main class, and whose silence is indistinguishable from success.
///
/// # Why this list exists and why it is not "every option"
///
/// `java` positional semantics are that everything after the program selector
/// belongs to the program, and [`insert_program_args_separator`] implements
/// exactly that. For most options a misplacement announces itself: `-cp` in the
/// tail produces a `ClassNotFoundException`, `-Xmx` in the tail produces an
/// `ArrayIndexOutOfBoundsException` from a program that did not expect an extra
/// argument. The options below are the ones where nothing at all happens —
/// exit 0, no file, no diagnostic — because their entire effect is to write a
/// diagnostic artefact or to turn a subsystem off.
///
/// That silence has cost real time in this campaign. `JDK-ONLY-REPORT-CENSUS-20260812`
/// closes with "**Flag order matters and is silent when wrong**" as its last
/// line, and `G33-1` was commissioned partly because it happened again. A
/// measurement lane that gets an empty result cannot tell "the VM says nothing
/// happened" from "the VM never heard me".
///
/// This is a **warning**, never an error, and the argv is never rewritten. A
/// Java program is entitled to an argument spelled `--real-jdk-report`, and
/// silently hoisting it out of the program's own argv would be a far worse bug
/// than the one being reported. Suppress with
/// `CRATONVM_NO_MISPLACED_FLAG_WARNING=1` for a program that really does take
/// one of these names.
pub(crate) const SILENTLY_IGNORED_IF_MISPLACED: &[&str] = &[
    // The census/diagnostic dump family (`docs/book/src/user-guide/real-jdk-mode.md`
    // §9). Every one of these takes a path and its only observable effect is
    // the file, so a discarded flag looks exactly like a clean run.
    "--dump-native-registry",
    "--real-jdk-report",
    "--dump-class-origins",
    "--dump-missing-natives",
    "--dump-missing-natives-grouped",
    "--dump-phase-report",
    // The compatibility-policy switches. A discarded one runs the whole
    // measurement under the OTHER policy, which is the failure that produces
    // a *confidently wrong* result rather than an empty one. That was true of
    // `--real-jdk` before the default flipped and is now true of
    // `--compatible` (and its `--real-jdk` alias) in the same way: a dropped
    // `--compatible` silently measures strict mode and labels it compatible.
    "--real-jdk",
    "--compatible",
    "--fast-boot",
    "--explain-real-jdk",
    "--trace-real-jdk",
    "--XX:AuditMissingNatives",
    // `--nojit` is on this list for the same reason as `--real-jdk`: a
    // discarded one silently measures the JIT arm and reports it as the
    // interpreter arm. `G20-1` §3 is an entire table of paired JIT/`--nojit`
    // arms; a silent miss there is not recoverable from the output.
    "--nojit",
    // Sampling/diagnostic switches whose absence is a quieter run, not an
    // error.
    "--stack-dump-on-timeout",
    "--stack-sample-ms",
];

/// Environment switch that silences [`misplaced_launcher_flags`]'s warning, for
/// a Java program that genuinely takes one of those names as its own argument.
pub(crate) const MISPLACED_FLAG_WARNING_OFF: &str = "CRATONVM_NO_MISPLACED_FLAG_WARNING";

/// Names from [`SILENTLY_IGNORED_IF_MISPLACED`] that appear in `argv` **after**
/// the program-args separator, i.e. that the launcher will discard.
///
/// Pure and order-preserving so it can be tested without a process: takes the
/// argv as [`insert_program_args_separator`] left it, returns the offending
/// spellings in the order they appear, each at most once. Both the bare
/// `--flag` and the inline `--flag=value` forms are recognised; the reported
/// name is always the bare one, because that is what the user has to move.
///
/// Returns empty when there is no separator at all — `java --version` with no
/// program has no tail, and every token is still a launcher option.
pub(crate) fn misplaced_launcher_flags(argv: &[String]) -> Vec<&'static str> {
    let Some(sep) = argv.iter().position(|a| a == "--") else {
        return Vec::new();
    };
    let mut found: Vec<&'static str> = Vec::new();
    for token in &argv[sep + 1..] {
        // `--flag=value` and `--flag` both report as `--flag`: the fix is the
        // same move either way, and naming the value back at the user only
        // makes the line harder to scan.
        let name = token.split('=').next().unwrap_or(token.as_str());
        if let Some(flag) = SILENTLY_IGNORED_IF_MISPLACED
            .iter()
            .find(|f| **f == name)
            .copied()
        {
            if !found.contains(&flag) {
                found.push(flag);
            }
        }
    }
    found
}

/// Print the [`misplaced_launcher_flags`] warning, if any, to stderr.
///
/// Deliberately loud and deliberately specific: it names each flag, states the
/// consequence in the tense that matters ("was passed to the Java program and
/// the launcher ignored it"), and shows the fix. A warning that says only
/// "check your argument order" leaves the reader doing the work this function
/// already did.
pub(crate) fn warn_about_misplaced_launcher_flags(argv: &[String]) {
    // `std::env::var_os`, deliberately, and this one may NOT be converted.
    // This function runs from `main` BEFORE `install_flags` latches the
    // snapshot (the call is ~37 lines earlier), so reading through
    // `flags::runtime_var_os` here latches it early and `install_flags` then
    // fails with "runtime flags were read before launcher configuration" —
    // every run exits 1. Measured, not reasoned: converting it broke `java
    // Hello`. The name is exempt in `flag_declaration_guard`'s `ALLOWED` as
    // kind 4 for exactly this reason.
    if std::env::var_os(MISPLACED_FLAG_WARNING_OFF).is_some() {
        return;
    }
    let misplaced = misplaced_launcher_flags(argv);
    if misplaced.is_empty() {
        return;
    }
    for flag in &misplaced {
        eprintln!(
            "[cratonvm] WARNING: `{flag}` appears AFTER the main class (or after `-jar <jar>`), \
             so it was passed to the Java program as an argument and the launcher IGNORED it. \
             This flag has no effect where it is."
        );
    }
    eprintln!(
        "[cratonvm] WARNING: move {} before the main class. \
         Launcher options are recognised only ahead of the program selector, exactly as in \
         `java`. If the program really does take {} as its own argument, set {}=1 to silence \
         this.",
        misplaced
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", "),
        if misplaced.len() == 1 {
            "that name"
        } else {
            "those names"
        },
        MISPLACED_FLAG_WARNING_OFF,
    );
}

/// Rewrite common HotSpot launcher spellings so clap can parse them.
///
/// HotSpot uses single-dash flags with idiosyncratic syntax (`-Xmx256m`,
/// `-Xshare:on`, `-XX:AOTMode=off`, `-XX:-UseContainerSupport`, etc.) that
/// clap's "long option" parser cannot handle natively — clap expects
/// `--Xmx 256m`. The `[[bin]] name = "java"` alias (see Cargo.toml) exists
/// specifically so the cratonvm binary is drop-in compatible with stock
/// `java`, so a Maven / Surefire / Gradle invocation like
/// `java -Xmx256m -classpath x Main` MUST parse.
///
/// This function rewrites every HotSpot single-dash spelling we care about
/// to the equivalent double-dash clap form *before* clap sees the argv.
/// `extract_hotspot_flags` handles the few cases that aren't expressible as
/// clap long options (`-XX:+/-Foo` boolean toggles for `HeapDumpOnOutOfMemoryError`,
/// `-agentlib:` / `-agentpath:` / `-javaagent:`).
pub(crate) fn normalize_java_launcher_argv(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let mut out = vec![args[0].clone()];
    let mut i = 1usize;
    let mut past_separator = false;
    // zt t3/ob: the collector the command line selected so far (the `<Name>`
    // of the last `-XX:+Use<Name>GC`), for `minus_gc_flag_deselects`.
    let mut gc_selected: Option<String> = None;
    while i < args.len() {
        let a = args[i].as_str();
        if past_separator {
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        if a == "--" {
            past_separator = true;
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        // JBoss Modules / WildFly use `-mp <modules dir>` after `-jar
        // jboss-modules.jar`. clap parses `-mp` as the short-flag cluster
        // `-m` + `-p`, which errors ("unexpected argument '-m'"). Mirror an
        // explicit `--` so everything from `-mp` onward becomes program args.
        if a == "-mp" {
            out.push("--".into());
            past_separator = true;
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        if a == "-jar" && i + 1 < args.len() {
            out.push("--jar".into());
            out.push(args[i + 1].clone());
            i += 2;
        } else if a == "-version" || a == "-v" {
            out.push("--version".into());
            i += 1;
        } else if a == "-p" && i + 1 < args.len() {
            // HotSpot's short `-p <path>` for `--module-path`: clap's
            // `alias = "p"` is the LONG alias `--p`, so `-p` was refused as
            // an unknown short flag (interpreter round i4 wave 86, lane L5).
            out.push("--module-path".into());
            out.push(args[i + 1].clone());
            i += 2;
        } else if (a == "-classpath" || a == "-cp") && i + 1 < args.len() {
            out.push("--classpath".into());
            out.push(args[i + 1].clone());
            i += 2;
        } else if let Some(rest) = a.strip_prefix("-classpath=") {
            out.push("--classpath".into());
            out.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-cp=") {
            out.push("--classpath".into());
            out.push(rest.to_string());
            i += 1;
        }
        // zt t12/tg5 (`CRATONVM_LAUNCHER_MISSING_VALUE_REFUSALS`, default ON,
        // `=0` kill): the JDK 9+ long spelling `--class-path <path>` /
        // `--class-path=<path>` (an empty `=` value boots, as on 25.0.3).
        // clap knew only `--classpath` / `--cp`, so it refused both.
        else if a == "--class-path"
            && i + 1 < args.len()
        {
            out.push("--classpath".into());
            out.push(args[i + 1].clone());
            i += 2;
        } else if let Some(rest) = a
            .strip_prefix("--class-path=")
            .filter(|_| true)
        {
            out.push("--classpath".into());
            out.push(rest.to_string());
            i += 1;
        }
        // -------- HotSpot -X compat: inline single-dash spellings --------
        // `-Xmx256m` / `-Xmx 256m` -> `--Xmx 256m`
        else if let Some(rest) = a.strip_prefix("-Xmx") {
            if rest.is_empty() && i + 1 < args.len() {
                out.push("--Xmx".into());
                out.push(args[i + 1].clone());
                i += 2;
            } else {
                out.push("--Xmx".into());
                out.push(rest.to_string());
                i += 1;
            }
        }
        // `-Xshare:on` / `-Xshare:off` -> `--Xshare on`
        else if let Some(rest) = a.strip_prefix("-Xshare:") {
            out.push("--Xshare".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-Xshare<sp>val` -> `--Xshare val` (rare)
        else if a == "-Xshare" && i + 1 < args.len() {
            out.push("--Xshare".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // `-Xverify:none|remote|all` -> `--Xverify none|remote|all`
        else if let Some(rest) = a.strip_prefix("-Xverify:") {
            // `-Xverify:none` is the HotSpot shorthand for `-noverify`.
            // Translate to `--noverify` so the boolean flag fires; the
            // existing CLI also accepts `--Xverify none` as a value flag.
            if rest == "none" {
                out.push("--noverify".into());
            } else {
                out.push("--Xverify".into());
                out.push(rest.to_string());
            }
            i += 1;
        }
        // `-Xverify val` (separate token, rare)
        else if a == "-Xverify" && i + 1 < args.len() {
            out.push("--Xverify".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // `-noverify` -> `--noverify` (HotSpot deprecated but still accepted)
        else if a == "-noverify" {
            out.push("--noverify".into());
            i += 1;
        }
        // `-XX:-BytecodeVerificationRemote` (interpreter round i5 wave 101,
        // lane L7): HotSpot then verifies no class a non-boot loader defines,
        // which is `-Xverify:none` here (boot classes are not verified by
        // default either). It fell into the "ignored `-XX:`" arm below, so an
        // app that relies on it still met our VerifyErrors. `+` is the
        // default: nothing to emit.
        else if a == "-XX:-BytecodeVerificationRemote" {
            out.push("--noverify".into());
            i += 1;
        } else if a == "-XX:+BytecodeVerificationRemote" {
            i += 1;
        }
        // `-verbose:gc` -> `--verbose:gc`; `-verbose:class` and bare `-verbose`
        // (HotSpot: class loading) -> `--verbose:class`. gc-common w2-e
        // (2026-09-23): the java launcher's own spelling of the most common GC
        // flag was REFUSED at startup — clap read `-verbose:gc` as the short
        // cluster `-v…` and exited with "unexpected argument '-v' found", so a
        // HotSpot command line carrying it never ran. Only the two spellings
        // clap has a flag for are rewritten; `-verbose:jni` / `-verbose:module`
        // still reach clap and fail loudly rather than being dropped in silence.
        // HotSpot's `-verbose:gc` IS `-Xlog:gc` (one `GC(n) Pause … NM->NM(NM)
        // T ms` line per collection on stdout); the single dash means that here
        // too since gcg g6/yg (below). This VM's own diagnostics (its
        // per-collection `[GC] pause:` lines and the `[GC] …` exit census) are
        // the double-dash `--verbose:gc`.
        else if a == "-verbose:gc" {
            // gcg g3/x (`gcg-g3x-proposal-verbose-gc-is-xlog-gc`), default ON
            // since gcg g6/yg (vg3 `x3_verbose_gc_*`, vg5 `yg5_isverbose_*`:
            // `MemoryMXBean.isVerbose()` is true under it, as on HotSpot);
            // `CRATONVM_VERBOSE_GC_IS_XLOG_GC=0` restores the old mapping to
            // `--verbose:gc`. The double-dash `--verbose:gc` keeps this VM's
            // diagnostic lines either way.
            if true {
                out.push("--Xlog".into());
                out.push("gc".into());
            } else {
                out.push("--verbose:gc".into());
            }
            i += 1;
        } else if a == "-verbose:class" || a == "-verbose" {
            out.push("--verbose:class".into());
            i += 1;
        }
        // `-Xbootclasspath:path` -> `--Xbootclasspath path` (a CratonVM
        // replace of the boot class path; HotSpot 9+ refuses this spelling).
        // `-Xbootclasspath/a:path` -> `--Xbootclasspath-append path`: appended
        // AFTER the runtime image, as on HotSpot (round 13 wave 10, lane misc10,
        // `r13w9-proxy6-xbootclasspath-append-replaces-the-boot-image-FIXED-20260928.md`;
        // it used to collapse onto the replace, which dropped the JDK image).
        // `-Xbootclasspath/p:path` -> `--Xbootclasspath-prepend path`, which
        // `run` refuses with HotSpot's message. Whether the two new spellings
        // keep the old replace meaning (`CRATONVM_XBOOTCLASSPATH_APPEND=0`) is
        // decided where the config is built, so this rewrite stays a pure
        // function of argv.
        else if let Some(rest) = a.strip_prefix("-Xbootclasspath/a:") {
            out.push("--Xbootclasspath-append".into());
            out.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-Xbootclasspath/p:") {
            out.push("--Xbootclasspath-prepend".into());
            out.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-Xbootclasspath:") {
            out.push("--Xbootclasspath".into());
            out.push(rest.to_string());
            i += 1;
        } else if a == "-Xbootclasspath" && i + 1 < args.len() {
            out.push("--Xbootclasspath".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // `-Xlog:spec` -> `--Xlog spec`
        else if let Some(rest) = a.strip_prefix("-Xlog:") {
            out.push("--Xlog".into());
            out.push(rest.to_string());
            i += 1;
        }
        // A bare `-Xlog` is HotSpot's `-Xlog:all` (every tag at `info`, to
        // stdout, default decorators). It takes NO separate operand: the next
        // token is the next option or the main class (gen r5w4/obs8; it used
        // to be consumed as the spec, so `java -Xlog -cp app.jar Main` parsed
        // `-cp` as the log selection and `app.jar` as the main class).
        else if a == "-Xlog" {
            out.push("--Xlog".into());
            out.push("all".into());
            i += 1;
        }
        // `-XX:AutoBoxCacheMax=N` -> `-Djava.lang.Integer.IntegerCache.high=N`.
        // F1-1/F29-1 §5 (NOMINATION 5): MEASURED byte-identical to
        // `-Djava.lang.Integer.IntegerCache.high=N` on HotSpot 25 across every
        // row (`int.127/128/1000/1001/-129`, and that `long`/`short`/`char`
        // caches do not move). CratonVM has no `VM.getSavedProperty` split —
        // `-D` already lands in `shared.system_properties`, which
        // `lang_math::integer_cache_bound` reads — so rewriting the `-XX:`
        // spelling onto the same property before `extract_system_properties`
        // sees it reuses that one road rather than adding a second.
        // `System.getProperty(...)` answering `null` for the `-XX:` form on
        // HotSpot (F29-1 §5, point 5) is a fact about HotSpot's saved-property
        // split, not about the cache, and this VM has no such split to
        // reproduce.
        else if let Some(rest) = a.strip_prefix("-XX:AutoBoxCacheMax=") {
            out.push(format!("-Djava.lang.Integer.IntegerCache.high={rest}"));
            i += 1;
        }
        // -------- HotSpot -XX compat: -XX:Foo=val and -XX:+/-Foo --------
        // `-XX:SharedArchiveFile=path` -> `--XX:SharedArchiveFile path`
        else if let Some(rest) = a.strip_prefix("-XX:SharedArchiveFile=") {
            out.push("--XX:SharedArchiveFile".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:AOTMode=off|training|production` -> `--XX:AOTMode <val>`
        else if let Some(rest) = a.strip_prefix("-XX:AOTMode=") {
            out.push("--XX:AOTMode".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:AOTCache=path` -> `--XX:AOTCache <path>`
        else if let Some(rest) = a.strip_prefix("-XX:AOTCache=") {
            out.push("--XX:AOTCache".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:AOTCacheOutput=path` -> `--XX:AOTCacheOutput <path>`
        else if let Some(rest) = a.strip_prefix("-XX:AOTCacheOutput=") {
            out.push("--XX:AOTCacheOutput".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:+AuditMissingNatives` -> `--XX:AuditMissingNatives`
        // `-XX:-AuditMissingNatives` -> drop (default off).
        else if a == "-XX:+AuditMissingNatives" {
            out.push("--XX:AuditMissingNatives".into());
            i += 1;
        } else if a == "-XX:-AuditMissingNatives" {
            // Default is off; nothing to emit.
            i += 1;
        }
        // `-XX:+ShowCodeDetailsInExceptionMessages` -> the clap toggle (on);
        // `-XX:-...` -> the explicit `=false` form (the default is now on, so
        // opting out must be representable, not merely "absent").
        else if a == "-XX:+ShowCodeDetailsInExceptionMessages" {
            out.push("--XX:ShowCodeDetailsInExceptionMessages=true".into());
            i += 1;
        } else if a == "-XX:-ShowCodeDetailsInExceptionMessages" {
            out.push("--XX:ShowCodeDetailsInExceptionMessages=false".into());
            i += 1;
        }
        // `-XX:-UseContainerSupport` -> `--XX:-UseContainerSupport`
        // (the clap long name literally is `XX:-UseContainerSupport`).
        else if a == "-XX:-UseContainerSupport" {
            out.push("--XX:-UseContainerSupport".into());
            i += 1;
        } else if a == "-XX:+UseContainerSupport" {
            // Default is on; nothing to emit.
            i += 1;
        }
        // `-Xms<size>` (minimum/initial heap) -> `--Xms <size>`.
        //
        // F-16: this arm used to DROP the flag. "CratonVM sizes the heap from
        // `-Xmx` only" was true while the collector allocated its whole arena
        // in the constructor — there was no initial size for the value to name.
        // Every collector now reserves `-Xmx` as address space and commits a
        // prefix, so `-Xms` names that prefix and is honoured on all three.
        //
        // Both the inline (`-Xms512m`) and separate-token (`-Xms 512m`) forms
        // must be handled. The separate-token form is listed in
        // `VALUE_TAKING_OPTS`, so `insert_program_args_separator` keeps the
        // value adjacent to the flag here; emitting the value as clap's own
        // argument is what stops a bare `512m` surviving to be mistaken for the
        // main-class positional (which would shift and consume the real class
        // name and the program args after it). Same shape as the `-Xmx` arm
        // above; a bare trailing `-Xms` with no value is dropped, as before.
        else if let Some(rest) = a.strip_prefix("-Xms") {
            if rest.is_empty() {
                if i + 1 < args.len() {
                    out.push("--Xms".into());
                    out.push(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            } else {
                out.push("--Xms".into());
                out.push(rest.to_string());
                i += 1;
            }
        }
        // `-Xmn<size>` (young generation) -> `--Xmn <size>` (gen r4w2/alloc2).
        //
        // This arm used to be the `-X…` catch-all below, which dropped the flag
        // without a word. Same shape as the `-Xms` arm: both the inline
        // (`-Xmn256m`) and separate-token (`-Xmn 256m`) forms, the latter kept
        // adjacent by `VALUE_TAKING_OPTS`; a bare trailing `-Xmn` is dropped.
        // Honoured by the Generational collector; see the config-apply step.
        else if let Some(rest) = a.strip_prefix("-Xmn") {
            if rest.is_empty() {
                if i + 1 < args.len() {
                    out.push("--Xmn".into());
                    out.push(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            } else {
                out.push("--Xmn".into());
                out.push(rest.to_string());
                i += 1;
            }
        }
        // `-Xss<size>` (default thread stack size) -> `--Xss <size>` (gcd
        // d6/f). HotSpot has only the inline spelling; a bare `-Xss` is dropped,
        // as the `-X…` catch-all below always did with every `-Xss` token.
        else if let Some(rest) = a.strip_prefix("-Xss") {
            if !rest.is_empty() {
                out.push("--Xss".into());
                out.push(rest.to_string());
            }
            i += 1;
        }
        // GC selector: `-XX:+Use<Name>GC` -> `--XX:UseGc <Name>`. The collector
        // name (`<Name>` between `Use` and `GC`) is forwarded verbatim; the
        // config-apply step (`parse_gc_algorithm`) honours `G1` / `Generational`
        // and warns-and-falls-back for any other collector. Emitting a value
        // option (rather than acting here) means a later `-XX:+Use...GC`
        // overrides an earlier one via clap last-wins, matching HotSpot.
        //
        // `-XX:-UseG1GC` / `-XX:-UseZGC` explicitly turn that collector off ->
        // select Generational. Note this is "off means the copying collector",
        // NOT "off means whatever the default is" — as of 2026-08-10 the
        // default IS ZGC, so resolving `-XX:-UseZGC` to the default would make
        // the flag select the very collector it disables. Other
        // `-XX:-Use<Name>GC` ("do not use collector X") select nothing and fall
        // through to the silent-ignore arm below.
        //
        // Guarded so `-XX:+UseStringDeduplication`, `-XX:+UseCompressedOops`,
        // etc. (no `GC` suffix) do NOT match and keep their existing handling.
        else if let Some(name) = a
            .strip_prefix("-XX:+Use")
            .and_then(|core| core.strip_suffix("GC"))
            .filter(|core| !core.is_empty())
        {
            out.push("--XX:UseGc".into());
            out.push(name.to_string());
            gc_selected = Some(name.to_string());
            i += 1;
        } else if a == "-XX:-UseG1GC" || a == "-XX:-UseZGC" {
            // zt t3/ob: turning off a collector the command line did not
            // select is a no-op, as on HotSpot (`-XX:+UseZGC -XX:-UseG1GC`
            // runs ZGC); it switched to Generational.
            if minus_gc_flag_deselects(a, gc_selected.as_deref()) {
                out.push("--XX:UseGc".into());
                out.push("Generational".into());
                gc_selected = Some("Generational".to_string());
            }
            i += 1;
        }
        // G1 tuning knobs (§7 item 4). `-XX:Name=Value` → `--XX:<short> Value`;
        // honoured only under G1 (the config-apply step is G1-gated). Each maps
        // to a `G1CollectorConfig` field via `G1ConfigOverrides`.
        else if let Some(v) = a.strip_prefix("-XX:InitiatingHeapOccupancyPercent=") {
            out.push("--XX:IHOP".into());
            out.push(v.to_string());
            i += 1;
        } else if a == "-XX:+ZUncommit" || a == "-XX:-ZUncommit" {
            out.push("--XX:ZUncommit".into());
            out.push((a == "-XX:+ZUncommit").to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:ZUncommitDelay=") {
            out.push("--XX:ZUncommitDelay".into());
            // zt t9/tg2 (`CRATONVM_LAUNCHER_ZGC_FLAG_HOTSPOT_PARSE`): HotSpot's
            // `uintx` syntax (`0x10`, `3k`), handed to clap in decimal; a
            // value HotSpot refuses was refused by `run`'s first scan
            // ([`scan_first_zgc_flag_refusal`]) and stays as written here.
            let decimal = launcher_zgc_flag_hotspot_parse_enabled()
                .then(|| hotspot_parse_uintx_flag(v))
                .flatten();
            out.push(decimal.map_or_else(|| v.to_string(), |n| n.to_string()));
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:SoftMaxHeapSize=") {
            out.push("--XX:SoftMaxHeapSize".into());
            out.push(v.to_string());
            i += 1;
        } else if a == "-XX:+ZProactive" || a == "-XX:-ZProactive" {
            // zt t6/tc (zt1tp item 4): dropped silently before.
            out.push("--XX:ZProactive".into());
            out.push((a == "-XX:+ZProactive").to_string());
            i += 1;
        } else if a == "-XX:+ZCollectionIntervalOnly" || a == "-XX:-ZCollectionIntervalOnly" {
            // zt t10/tg3 (`zgc-zt9tg2-*` item 2): fell into the silent `-XX:`
            // catch-all; the sign-less spelling is refused by `run`'s scan.
            out.push("--XX:ZCollectionIntervalOnly".into());
            out.push((a == "-XX:+ZCollectionIntervalOnly").to_string());
            i += 1;
        } else if let Some(option) = zgc_boolean_without_sign(a) {
            // zt t7/x4b: `-XX:ZProactive=false` / `-XX:ZUncommit` -- a ZGC
            // boolean without its `+`/`-`. HotSpot refuses the launch; `run`
            // decides (ZGC only, `CRATONVM_ZGC_BOOLEAN_FLAG_SIGN_REQUIRED`).
            out.push(format!("--XX:ZBooleanWithoutSign={option}"));
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:ZAllocationSpikeTolerance=") {
            // zt t7/x4: ONE `--name=value` token. HotSpot takes a negative
            // tolerance (`=-1`), and a separate `-1` token reads to clap as a
            // short flag, which failed the whole parse.
            out.push(format!("--XX:ZAllocationSpikeTolerance={v}"));
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:ZCollectionInterval=") {
            // zt t7/tg (zt1tp item 3): dropped silently before. zt t8/x5: ONE
            // `--name=value` token, as `ZAllocationSpikeTolerance` above:
            // HotSpot 25.0.3 takes `-XX:ZCollectionInterval=-1` (off, printed
            // `-1.000000 {command line}`), and a separate `-1` token reads to
            // clap as a short flag, which failed the whole launch.
            out.push(format!("--XX:ZCollectionInterval={v}"));
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:ZCollectionIntervalMajor=") {
            // zt t9/tg2 (zt7tg item 2): the flag the alias above stands for;
            // ONE token, as the alias (a negative value is HotSpot's "off").
            out.push(format!("--XX:ZCollectionIntervalMajor={v}"));
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:ZCollectionIntervalMinor=") {
            // zt t27/gn5 (gn page item 14): the minor timer; ONE token, as
            // the two above (HotSpot's default is -1, a negative "off").
            out.push(format!("--XX:ZCollectionIntervalMinor={v}"));
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:G1HeapRegionSize=") {
            out.push("--XX:G1RegionSize".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxGCPauseMillis=") {
            out.push("--XX:MaxGCPause".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:G1MixedGCLiveThresholdPercent=") {
            out.push("--XX:G1MixedLiveThreshold".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:G1HeapWastePercent=") {
            out.push("--XX:G1HeapWaste".into());
            out.push(v.to_string());
            i += 1;
        }
        // zr z3/mt: the GC worker-count flags. All three fell into the silent
        // `-XX:` catch-all below until 2026-10-12 (`-XX:ParallelGCThreads=4`
        // reached no collector; only the `--XX:ParallelGCThreads 4` spelling
        // reached G1). Both spellings now size G1 and ZGC pools.
        else if let Some(v) = a.strip_prefix("-XX:ParallelGCThreads=") {
            out.push("--XX:ZgcParallelGCThreads".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:ConcGCThreads=") {
            out.push("--XX:ConcGCThreads".into());
            out.push(v.to_string());
            i += 1;
        } else if a == "-XX:+UseDynamicNumberOfGCThreads" || a == "-XX:-UseDynamicNumberOfGCThreads"
        {
            out.push("--XX:UseDynamicNumberOfGCThreads".into());
            out.push((a == "-XX:+UseDynamicNumberOfGCThreads").to_string());
            i += 1;
        }
        // Young generation sizing (gen r4w2/alloc2): `-XX:NewSize=` /
        // `-XX:MaxNewSize=` / `-XX:NewRatio=` -> `--XX:<Name> <value>`. They fell
        // into the silent `-XX:` catch-all below until 2026-09-23.
        else if let Some(v) = a.strip_prefix("-XX:NewSize=") {
            out.push("--XX:NewSize".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxNewSize=") {
            out.push("--XX:MaxNewSize".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:NewRatio=") {
            out.push("--XX:NewRatio".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxDirectMemorySize=") {
            out.push("--XX:MaxDirectMemorySize".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxMetaspaceSize=") {
            // gc-common w6-f: enforced (it fell into the silent `-XX:`
            // catch-all below until 2026-09-24). `-XX:MetaspaceSize` — the
            // first collection threshold — still does.
            out.push("--XX:MaxMetaspaceSize".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxJavaStackTraceDepth=") {
            out.push("--XX:MaxJavaStackTraceDepth".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:SoftRefLRUPolicyMSPerMB=") {
            // gen r5w2/conc6: honoured (`VmConfig::soft_ref_lru_policy_ms_per_mb`).
            // It fell into the silent `-XX:` catch-all below until 2026-09-26.
            out.push("--XX:SoftRefLRUPolicyMSPerMB".into());
            out.push(v.to_string());
            i += 1;
        }
        // gcg g3/x: HotSpot's heap-size flags. `-XX:MaxHeapSize=` IS `-Xmx`
        // (the same flag on HotSpot; clap's last-wins keeps argv order);
        // the other four have their own `--XX:` options. All five fell into
        // the silent `-XX:` catch-all below until 2026-10-01, so
        // `-XX:MaxHeapSize=64m` ran with the ergonomic heap.
        else if let Some(v) = a.strip_prefix("-XX:MaxHeapSize=") {
            out.push("--Xmx".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MinHeapSize=") {
            out.push("--XX:MinHeapSize".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:InitialHeapSize=") {
            out.push("--XX:InitialHeapSize".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MinHeapFreeRatio=") {
            out.push("--XX:MinHeapFreeRatio".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxHeapFreeRatio=") {
            out.push("--XX:MaxHeapFreeRatio".into());
            out.push(v.to_string());
            i += 1;
        } else if a == "-XX:+UseStringDeduplication" {
            out.push("--XX:StringDedup".into());
            out.push("true".into());
            i += 1;
        } else if a == "-XX:-UseStringDeduplication" {
            out.push("--XX:StringDedup".into());
            out.push("false".into());
            i += 1;
        }
        // These `-XX` flags are not expressible as clap long names, so keep
        // them verbatim for `extract_hotspot_flags`, which runs after this
        // normalization stage.
        else if a == "-XX:+HeapDumpOnOutOfMemoryError"
            || a == "-XX:-HeapDumpOnOutOfMemoryError"
            || a.starts_with("-XX:HeapDumpPath=")
            // gen r4w5/thrash5 (2026-09-24): the other three
            // `report_java_out_of_memory` options, implemented since. They
            // fell into the silent catch-all below until then.
            || is_oom_report_toggle(a)
            // gcf f2/o: `-XX:±UseGCOverheadLimit` (`VmConfig::use_gc_overhead_limit`).
            // It fell into the silent catch-all below until 2026-10-01.
            || is_gc_overhead_limit_toggle(a)
            || a.starts_with("-XX:OnOutOfMemoryError=")
            // gen r4w6/young6: the HotSpot tenuring flags, honoured by the
            // Generational collector (`VmConfig::tenuring`).
            || is_tenuring_flag(a)
            // gci i43/lf: `-XX:SurvivorRatio` and the TLAB flags, validated
            // as HotSpot 25 does (`space_tlab_flags_from_hotspot_flags`).
            // They fell into the silent catch-all below until 2026-10-16.
            || is_space_tlab_flag(a)
        {
            out.push(args[i].clone());
            i += 1;
        }
        // gc-common w8-g: `-XX:±DisableExplicitGC` / `-XX:±ExplicitGCInvokesConcurrent`
        // are honoured (`VmConfig::explicit_gc`). They fell into the silent
        // `-XX:` catch-all below until 2026-09-24. Kept verbatim for
        // `extract_hotspot_flags`, like the heap-dump toggles above.
        else if a == "-XX:+DisableExplicitGC"
            || a == "-XX:-DisableExplicitGC"
            || a == "-XX:+ExplicitGCInvokesConcurrent"
            || a == "-XX:-ExplicitGCInvokesConcurrent"
        {
            out.push(args[i].clone());
            i += 1;
        }
        // gcd d5/f: `-XX:StartFlightRecording`, its `:<opts>` form and HotSpot's
        // `=<opts>` spelling are kept verbatim for `extract_hotspot_flags`,
        // which parses them (obsaudit D12). All three fell into the silent
        // `-XX:` catch-all below, so the JFR start-up recording never saw them
        // (found by reading, gcd d5/v).
        else if is_start_flight_recording(a) {
            out.push(args[i].clone());
            i += 1;
        }
        // Interpreter round i2 wave 59, lane L1: `-XX:±DisableAttachMechanism`
        // (`VmConfig::disable_attach_mechanism`). It fell into the silent
        // `-XX:` catch-all below until then, so the attach mechanism stayed
        // on under the flag
        // (`docs/feature-designs/profile-guided-inlining.md`).
        else if is_disable_attach_mechanism_toggle(a) || is_perf_data_toggle(a) {
            out.push(args[i].clone());
            i += 1;
        }
        // Interpreter round i2 wave 60, lane L1: `-XX:+EnableDynamicAgentLoading`
        // and its `-` form (`VmConfig::enable_dynamic_agent_loading`), which
        // hide the dynamic-agent warning; the catch-all below dropped them.
        else if is_enable_dynamic_agent_loading_toggle(a) {
            out.push(args[i].clone());
            i += 1;
        }
        // nio21 wave 16, lane L5: `-XX:±OmitStackTraceInFastThrow`
        // (`VmConfig::omit_stack_trace_in_fast_throw`). The catch-all below
        // dropped it, so `-XX:-` could not turn the fast throw off.
        else if is_omit_stack_trace_in_fast_throw_toggle(a) {
            out.push(args[i].clone());
            i += 1;
        }
        // Interpreter round i4 wave 84, lane L2: `-XX:±ShowHiddenFrames`
        // (`VmConfig::show_hidden_frames`); the catch-all below dropped it.
        else if is_show_hidden_frames_toggle(a)
            || is_stack_trace_in_throwable_toggle(a)
            // Interpreter round i5 wave 102, lane L6:
            // `-XX:DiagnoseSyncOnValueBasedClasses=<n>`
            // (`VmConfig::diagnose_sync_on_value_based_classes`).
            || a.starts_with("-XX:DiagnoseSyncOnValueBasedClasses=")
        {
            // Wave 85: `-XX:±StackTraceInThrowable` too
            // (`VmConfig::stack_trace_in_throwable`).
            out.push(args[i].clone());
            i += 1;
        }
        // Interpreter round i3 wave 79, lane L3: `-Xrs` and
        // `-XX:±ReduceSignalUsage` (`VmConfig::reduce_signal_usage`); the
        // `-X` / `-XX:` catch-alls below dropped them.
        else if is_reduce_signal_usage_toggle(a) {
            out.push(args[i].clone());
            i += 1;
        }
        // zt t21/tg10: `-Xnoclassgc` and `-XX:±ClassUnloading`
        // (`VmConfig::class_unloading`); the `-X` / `-XX:` catch-alls below
        // dropped them, so classes unloaded whatever the command line said.
        else if is_class_unloading_toggle(a) {
            out.push(args[i].clone());
            i += 1;
        }
        // zt t23/hunt4: `-XX:±AlwaysPreTouch` (`VmConfig::always_pre_touch`);
        // the `-XX:` catch-all below dropped it, so the ZGC heap was never
        // touched and `gc,init` said `Pre-touch: Disabled` under the flag.
        else if is_always_pre_touch_toggle(a) {
            out.push(args[i].clone());
            i += 1;
        }
        // gcj j1/st: `-XX:±PrintStringTableStatistics` and
        // `-XX:StringTableSize=<n>` (`VmConfig::print_string_table_statistics`,
        // `VmConfig::string_table_size`); the `-XX:` catch-all below dropped
        // them, so the exit statistics were never printed.
        else if is_string_table_flag(a) {
            out.push(args[i].clone());
            i += 1;
        }
        // zt t24/hunt7: `-XX:±UseTransparentHugePages` and `-XX:±UseLargePages`
        // (`VmConfig::use_transparent_huge_pages`); the `-XX:` catch-all
        // below dropped them, so ZGC's heap was never advised and `gc,init`
        // said `Large Page Support: Disabled` under the flag.
        else if is_large_page_toggle(a) {
            out.push(args[i].clone());
            i += 1;
        }
        // Any other `-XX:...` flag is a HotSpot tuning knob CratonVM does not
        // implement (`-XX:MetaspaceSize`, `-XX:+UseCompressedOops`, …).
        // Recognized `-XX:` flags are rewritten by the branches above;
        // everything else is silently ignored so a Maven Surefire / Gradle
        // fork — which passes these unconditionally — launches instead of clap
        // aborting with "unexpected argument '-X'".
        else if a.starts_with("-XX:") {
            if None::<std::ffi::OsString>.is_some() {
                eprintln!("[cratonvm] ignoring unimplemented HotSpot flag: {a}");            }
            i += 1;
        }
        // Any other single-dash `-X...` flag is a HotSpot knob CratonVM does
        // not implement (`-Xint`, `-Xbatch`, `-Xrs`, `-XshowSettings`,
        // `-Xnoclassgc`, …; `-Xss` is honoured since gcd d6/f, above). The
        // recognized value-taking `-X` spellings
        // (`-Xmx`/`-Xms`/`-Xmn`/`-Xss`/`-Xshare`/`-Xverify`/`-Xbootclasspath`/`-Xlog`) are
        // rewritten by the branches above; everything else is accepted-and-
        // ignored here — same as `-XX:` — so a drop-in `java` launches instead
        // of clap aborting with "unexpected argument '-X...'". These remaining
        // `-X` flags are all the inline/no-value form, so dropping just this
        // single token is correct (HotSpot has no separate-token spelling for
        // the ones not handled above).
        //
        // [LOW arg-parse fix (3)] Deliberately drop ONLY this one token
        // (`i += 1`) and never consume the following token. An unrecognized
        // `-X` flag must not swallow the next token when that token is the
        // main-class name: e.g. `java -Xunknown Main` (or, if the separator
        // inserter did not run, `-Xint Main`) must still resolve `Main` as the
        // main class, not silently treat it as the unknown flag's value and
        // shift the real class into the program args. We make that invariant
        // explicit here: when the next token is a bare positional (no leading
        // `-`), it is the main-class candidate and is left for the normal
        // positional/`--` handling to claim — we never absorb it.
        else if a.starts_with("-X") {
            if None::<std::ffi::OsString>.is_some() {
                eprintln!("[cratonvm] ignoring unimplemented HotSpot flag: {a}");
                // Surface the swallow-avoidance: if a bare positional follows an
                // unknown `-X` flag, it is the main-class candidate and is left
                // untouched (we drop only the flag, never `i += 2`).
                if let Some(next) = args.get(i + 1) {
                    if !next.starts_with('-') && next != "--" {
                        eprintln!(
                            "[cratonvm] keeping following token '{next}' as a \
                             positional (unknown -X flag does not consume it)"
                        );
                    }
                }
            }
            // Drop ONLY this flag token; do NOT consume the next token. This is
            // what keeps an unknown `-X` flag from swallowing the main-class
            // name (see the comment above).
            i += 1;
        }
        // `--sun-misc-unsafe-memory-access=<mode>` (JEP 498). HotSpot's launcher
        // does exactly this rewrite: the flag's only effect is to set the
        // `sun.misc.unsafe.memory.access` system property, and when the flag is
        // absent HotSpot sets NOTHING — the effective mode is still "allow with
        // a warning", but `System.getProperty` answers null.
        //
        // That null is load-bearing for more than tidiness. netty 4.2 disables
        // `sun.misc.Unsafe` by default on Java 25+ *unless* this property is
        // set, so a VM that pins it — which CratonVM did, unconditionally, in
        // `vm_init.rs` — silently puts netty and every other Unsafe-aware
        // library on a different code path than a stock JDK 25 run, and makes
        // any CratonVM-vs-HotSpot comparison over them a comparison of two
        // different code paths rather than two VMs.
        //
        // Rewriting to `-D` here rather than parsing it later gives the
        // property exactly one source: the user.
        else if let Some(mode) = a.strip_prefix("--sun-misc-unsafe-memory-access=") {
            if None::<std::ffi::OsString>.is_some() {
                eprintln!(
                    "[cratonvm] --sun-misc-unsafe-memory-access={mode} -> \
                     -Dsun.misc.unsafe.memory.access={mode}"
                );
            }
            out.push(format!("-Dsun.misc.unsafe.memory.access={mode}"));
            i += 1;
        }
        // HotSpot VM-selection flags. Modern HotSpot accepts `-server` and
        // `-client` for compatibility (the server VM is effectively the only
        // implementation on current JDKs). WildFly's HostController launch
        // command still passes `-server`; accept-and-ignore it so the `java`
        // shim remains drop-in compatible instead of clap interpreting
        // `-server` as a short-option cluster and aborting on `-s`.
        else if a == "-server" || a == "-client" {
            if None::<std::ffi::OsString>.is_some() {
                eprintln!("[cratonvm] ignoring HotSpot VM selection flag: {a}");            }
            i += 1;
        }
        // Assertion control flags: `-ea`/`-enableassertions[:<pkgname>...|:<classname>]`,
        // `-da`/`-disableassertions[...]`, `-esa`/`-enablesystemassertions`,
        // `-dsa`/`-disablesystemassertions`.
        //
        // They are dropped HERE (clap would read `-ea` as the short-option
        // cluster `-e -a` and abort with "unexpected argument '-e'"), but they
        // are no longer *ignored*: `main` scans the same argv with
        // [`launcher_assertions_requested`] before the flag snapshot is
        // latched, and the unscoped user spellings set `CRATONVM_ENABLE_ASSERTIONS`.
        // This arm has to stay a pure token filter — see that function for why
        // the switch cannot be flipped from inside this (pure, ~60-test)
        // normaliser.
        //
        // Since wave 107 (interpreter round i5, lane L5) every form, the
        // system switches and the scoped `-ea:some.pkg...` / `-da:some.Class`
        // included, reaches the VM through `VmConfig::assertion_directives`
        // (`launcher_assertion_directives`, read in `run()` before this strip).
        else if a == "-ea"
            || a == "-da"
            || a == "-esa"
            || a == "-dsa"
            || a.starts_with("-ea:")
            || a.starts_with("-da:")
            || a.starts_with("-enableassertions")
            || a.starts_with("-disableassertions")
            || a.starts_with("-enablesystemassertions")
            || a.starts_with("-disablesystemassertions")
        {
            if None::<std::ffi::OsString>.is_some() {
                eprintln!("[cratonvm] assertion option (VmConfig::assertion_directives): {a}");            }
            i += 1;
        }
        // zt t18/hunt2: `--finalization=enabled|disabled` (JEP 421) is a VM
        // option HotSpot's `Arguments::parse_each_vm_init_arg` matches; it
        // reaches the VM through `VmConfig::finalization_enabled`
        // (`launcher_finalization_option`, read in `run()` before this strip).
        // clap refused it ("unexpected argument").
        else if a.starts_with("--finalization=") {
            i += 1;
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

/// Pre-process raw command-line arguments to extract `-Dkey=value` system
/// property flags (Java-style) before handing the rest to clap.  Returns
/// `(filtered_args, system_properties)`.
pub(crate) fn extract_system_properties(raw: Vec<String>) -> (Vec<String>, Vec<(String, String)>) {
    let mut filtered = Vec::with_capacity(raw.len());
    let mut props = Vec::new();
    let mut past_separator = false;
    // [LOW arg-parse fix (2)] When the PREVIOUS token was a separate-token
    // value-taking option (`--classpath`, `--Xlog`, `--add-opens`, …), the
    // CURRENT token is that option's VALUE, not an option position. A value
    // may legitimately begin with `-D` (e.g. `--Xlog -Dspecial`, or a
    // classpath/module-path entry on an exotic path), and must NOT be hijacked
    // as a `-Dkey=value` system property — doing so both loses the option's
    // value and fabricates a bogus property. Track the value position and pass
    // it through verbatim. (`-D` itself is never a value-taking option name, so
    // a genuine `-Dkey=value` in an *option* position is still extracted.)
    let mut prev_was_value_opt = false;
    for arg in raw {
        // Anything after `--` is a program argument and must be preserved
        // verbatim, including bare `-Dfoo=bar` tokens that the Java program
        // (e.g. jboss-modules) wants to consume itself.
        if past_separator {
            filtered.push(arg);
            continue;
        }
        if arg == "--" {
            // Mark the separator boundary so subsequent `-D...` tokens are
            // preserved as program arguments. We still emit the `--` to
            // clap so it knows where program args begin (otherwise tokens
            // like `-mp` would be rejected as unknown short flags). The
            // `--` itself is stripped out of the program-args vector
            // after clap parsing, before the String[] is built for
            // Java's main().
            past_separator = true;
            prev_was_value_opt = false;
            filtered.push(arg);
            continue;
        }
        // This token is the value of a preceding value-taking option: emit it
        // unchanged even if it starts with `-D`, and do not treat it as an
        // option position.
        if prev_was_value_opt {
            prev_was_value_opt = false;
            filtered.push(arg);
            continue;
        }
        // Remember whether THIS token is a separate-token value-taking option,
        // so the next iteration knows the following token is its value.
        prev_was_value_opt = VALUE_TAKING_OPTS.contains(&arg.as_str());
        if let Some(kv) = arg.strip_prefix("-D") {
            if let Some((k, v)) = kv.split_once('=') {
                props.push((k.to_string(), v.to_string()));
            } else {
                // `-Dkey` with no value -> set to empty string (matches java behaviour)
                props.push((kv.to_string(), String::new()));
            }
        } else {
            filtered.push(arg);
        }
    }
    (filtered, props)
}

/// T6 CLI compat: HotSpot-style flags extracted from raw argv before clap
/// sees them.  clap's long-option format can't natively parse a `+`/`-`
/// sign inside the option name the way HotSpot's `-XX:+Foo` / `-XX:-Foo`
/// and `-agentlib:` spellings do.
///
/// Fields are populated by [`extract_hotspot_flags`] and consumed inside
/// `run()` to mutate `VmConfig` after clap returns.
#[derive(Debug, Default, Clone)]
pub(crate) struct HotspotFlags {
    /// `-XX:+HeapDumpOnOutOfMemoryError`. `-XX:-...` turns it off.
    pub(crate) heap_dump_on_oom: Option<bool>,
    /// `-XX:HeapDumpPath=<path>` companion.
    pub(crate) heap_dump_path: Option<String>,
    /// gc-common w8-g: `-XX:+DisableExplicitGC` / `-XX:-...` (last one wins).
    pub(crate) disable_explicit_gc: Option<bool>,
    /// gc-common w8-g: `-XX:+ExplicitGCInvokesConcurrent` / `-XX:-...`.
    pub(crate) explicit_gc_invokes_concurrent: Option<bool>,
    /// `-XX:+ExitOnOutOfMemoryError` / `-XX:-...` (gen r4w5/thrash5).
    pub(crate) exit_on_oom: Option<bool>,
    /// `-XX:+CrashOnOutOfMemoryError` / `-XX:-...` (gen r4w5/thrash5).
    pub(crate) crash_on_oom: Option<bool>,
    /// `-XX:OnOutOfMemoryError=<cmds>`; repeated options accumulate joined by
    /// `\n`, as HotSpot's `ccstrlist` does (gen r4w5/thrash5).
    pub(crate) on_oom_error: Option<String>,
    /// gen r4w6/young6 — `-XX:MaxTenuringThreshold=<n>` (raw text; validated
    /// by [`tenuring_config_from_flags`]). Last one wins.
    pub(crate) max_tenuring_threshold: Option<String>,
    /// gen r4w6/young6 — `-XX:InitialTenuringThreshold=<n>`.
    pub(crate) initial_tenuring_threshold: Option<String>,
    /// gen r4w6/young6 — `-XX:TargetSurvivorRatio=<n>`.
    pub(crate) target_survivor_ratio: Option<String>,
    /// gci i27/gs (`gci-i23tn-...` item 3): EVERY occurrence of the three
    /// tenuring values in command-line order, as `(which, raw)` with `which`
    /// 0 = Max, 1 = Initial, 2 = TargetSurvivorRatio. HotSpot validates each
    /// option as it reads it and stops at the first bad one, so
    /// [`tenuring_config_from_flags`] checks these in this order before it
    /// takes the last value of each. Empty when built by hand (tests).
    pub(crate) tenuring_values_in_order: Vec<(u8, String)>,
    /// gci i43/lf: every `-XX:<Name>=<v>` of [`SPACE_TLAB_VALUE_FLAGS`]
    /// (`SurvivorRatio` and the TLAB value flags) in command-line order, as
    /// `(name, raw)`. HotSpot validates each occurrence as it reads it, so
    /// [`space_tlab_flags_from_hotspot_flags`] checks them in this order and
    /// keeps the last value of each.
    pub(crate) space_tlab_values_in_order: Vec<(&'static str, String)>,
    /// gci i43/lf: `-XX:±UseTLAB` (last one wins).
    pub(crate) use_tlab: Option<bool>,
    /// gci i43/lf: `-XX:±ResizeTLAB` (last one wins).
    pub(crate) resize_tlab: Option<bool>,
    /// gen r4w6/young6 — `-XX:+PrintTenuringDistribution` / `-XX:-...`.
    pub(crate) print_tenuring_distribution: Option<bool>,
    /// obsaudit D12 — `-XX:StartFlightRecording` (bare, `Some("")`) or
    /// `-XX:StartFlightRecording:opt=val,opt=val` (`Some("opt=val,...")`).
    /// Parsed into a `JfrStartRecordingConfig` by
    /// `parse_jfr_start_recording_opts` after clap returns.
    pub(crate) jfr_start_recording: Option<String>,
    pub(crate) jfr_start_recordings: Vec<String>,
    /// gcf f2/o — `-XX:+UseGCOverheadLimit` / `-XX:-...` (last one wins).
    pub(crate) use_gc_overhead_limit: Option<bool>,
    /// `-XX:+DisableAttachMechanism` / `-XX:-...` (last one wins;
    /// interpreter round i2 wave 59, lane L1).
    pub(crate) disable_attach_mechanism: Option<bool>,
    /// HotSpot perf-data publication policy (last toggle wins).
    pub(crate) use_perf_data: Option<bool>,
    pub(crate) perf_disable_shared_mem: Option<bool>,
    /// `-XX:+EnableDynamicAgentLoading` / `-XX:-...` (last one wins;
    /// interpreter round i2 wave 60, lane L1).
    pub(crate) enable_dynamic_agent_loading: Option<bool>,
    /// `-XX:+OmitStackTraceInFastThrow` / `-XX:-...` (last one wins; nio21
    /// wave 16, lane L5).
    pub(crate) omit_stack_trace_in_fast_throw: Option<bool>,
    /// `-XX:+ShowHiddenFrames` / `-XX:-...` (last one wins; interpreter round
    /// i4 wave 84, lane L2).
    pub(crate) show_hidden_frames: Option<bool>,
    /// `-XX:+StackTraceInThrowable` / `-XX:-...` (last one wins; interpreter
    /// round i4 wave 85, lane L2).
    pub(crate) stack_trace_in_throwable: Option<bool>,
    /// `-XX:DiagnoseSyncOnValueBasedClasses=<n>`, raw text (last one wins;
    /// validated by [`diagnose_sync_on_value_based_classes_from_flag`];
    /// interpreter round i5 wave 102, lane L6).
    pub(crate) diagnose_sync_on_value_based_classes: Option<String>,
    /// `-Xrs` / `-XX:+ReduceSignalUsage` / `-XX:-...` (last one wins;
    /// interpreter round i3 wave 79, lane L3).
    pub(crate) reduce_signal_usage: Option<bool>,
    /// zt t21/tg10: `-XX:+ClassUnloading` / `-XX:-ClassUnloading` /
    /// `-Xnoclassgc` (`false`); last one wins, as in HotSpot's in-order parse.
    pub(crate) class_unloading: Option<bool>,
    /// zt t23/hunt4: `-XX:+AlwaysPreTouch` / `-XX:-AlwaysPreTouch`; last one
    /// wins, as in HotSpot's in-order parse.
    pub(crate) always_pre_touch: Option<bool>,
    /// gcj j1/st: `-XX:±PrintStringTableStatistics` (last one wins).
    pub(crate) print_string_table_statistics: Option<bool>,
    /// gcj j1/st: `-XX:StringTableSize=<n>` (last one wins; the HotSpot flag
    /// table has already refused a value outside 128..=16777216).
    pub(crate) string_table_size: Option<usize>,
    /// zt t24/hunt7: `-XX:±UseTransparentHugePages` (last one wins).
    pub(crate) use_transparent_huge_pages: Option<bool>,
    /// zt t24/hunt7: `-XX:±UseLargePages` (last one wins); only its explicit
    /// `-` is read ([`effective_use_transparent_huge_pages`]).
    pub(crate) use_large_pages: Option<bool>,
    /// `-agentlib:<spec>`, `-agentpath:<spec>`, `-javaagent:<spec>` — the
    /// entire token (including prefix) is preserved so the existing
    /// `AgentRegistry::parse_agent_option` can consume it verbatim.
    pub(crate) agent_options: Vec<String>,
}

/// gcf f2/o: `-XX:±UseGCOverheadLimit`, kept verbatim by the normaliser for
/// [`extract_hotspot_flags`] (`VmConfig::use_gc_overhead_limit`).
pub(crate) fn is_gc_overhead_limit_toggle(arg: &str) -> bool {
    arg == "-XX:+UseGCOverheadLimit" || arg == "-XX:-UseGCOverheadLimit"
}

/// `-XX:±DisableAttachMechanism`, kept verbatim by the normaliser for
/// [`extract_hotspot_flags`] (`VmConfig::disable_attach_mechanism`;
/// interpreter round i2 wave 59, lane L1).
pub(crate) fn is_perf_data_toggle(arg: &str) -> bool {
    matches!(
        arg,
        "-XX:+UsePerfData"
            | "-XX:-UsePerfData"
            | "-XX:+PerfDisableSharedMem"
            | "-XX:-PerfDisableSharedMem"
    )
}

pub(crate) fn is_disable_attach_mechanism_toggle(arg: &str) -> bool {
    arg == "-XX:+DisableAttachMechanism" || arg == "-XX:-DisableAttachMechanism"
}

/// `-XX:+EnableDynamicAgentLoading` and its `-` form, kept verbatim by the
/// normaliser for [`extract_hotspot_flags`]
/// (`VmConfig::enable_dynamic_agent_loading`; interpreter round i2 wave 60,
/// lane L1).
pub(crate) fn is_enable_dynamic_agent_loading_toggle(arg: &str) -> bool {
    arg == "-XX:+EnableDynamicAgentLoading" || arg == "-XX:-EnableDynamicAgentLoading"
}

/// `-XX:±OmitStackTraceInFastThrow`, kept verbatim by the normaliser for
/// [`extract_hotspot_flags`] (`VmConfig::omit_stack_trace_in_fast_throw`;
/// nio21 wave 16, lane L5).
pub(crate) fn is_omit_stack_trace_in_fast_throw_toggle(arg: &str) -> bool {
    arg == "-XX:+OmitStackTraceInFastThrow" || arg == "-XX:-OmitStackTraceInFastThrow"
}

/// `-XX:±ShowHiddenFrames`, kept verbatim by the normaliser for
/// [`extract_hotspot_flags`] (`VmConfig::show_hidden_frames`; interpreter
/// round i4 wave 84, lane L2). HotSpot also wants
/// `-XX:+UnlockDiagnosticVMOptions` for it; this launcher accepts it alone.
pub(crate) fn is_show_hidden_frames_toggle(arg: &str) -> bool {
    arg == "-XX:+ShowHiddenFrames" || arg == "-XX:-ShowHiddenFrames"
}

/// `-XX:±StackTraceInThrowable`, kept verbatim by the normaliser for
/// [`extract_hotspot_flags`] (`VmConfig::stack_trace_in_throwable`;
/// interpreter round i4 wave 85, lane L2).
pub(crate) fn is_stack_trace_in_throwable_toggle(arg: &str) -> bool {
    arg == "-XX:+StackTraceInThrowable" || arg == "-XX:-StackTraceInThrowable"
}

/// `-Xrs` and `-XX:±ReduceSignalUsage` (HotSpot's `-Xrs` sets the flag),
/// kept verbatim by the normaliser for [`extract_hotspot_flags`]
/// (`VmConfig::reduce_signal_usage`; interpreter round i3 wave 79, lane L3).
pub(crate) fn is_reduce_signal_usage_toggle(arg: &str) -> bool {
    arg == "-Xrs" || arg == "-XX:+ReduceSignalUsage" || arg == "-XX:-ReduceSignalUsage"
}

/// zt t21/tg10: `-Xnoclassgc` and `-XX:±ClassUnloading` (HotSpot's
/// `-Xnoclassgc` sets `ClassUnloading=false`), kept verbatim by the
/// normaliser for [`extract_hotspot_flags`] (`VmConfig::class_unloading`).
pub(crate) fn is_class_unloading_toggle(arg: &str) -> bool {
    arg == "-Xnoclassgc" || arg == "-XX:+ClassUnloading" || arg == "-XX:-ClassUnloading"
}

/// zt t23/hunt4: `-XX:±AlwaysPreTouch`, kept verbatim by the normaliser for
/// [`extract_hotspot_flags`] (`VmConfig::always_pre_touch`).
pub(crate) fn is_always_pre_touch_toggle(arg: &str) -> bool {
    arg == "-XX:+AlwaysPreTouch" || arg == "-XX:-AlwaysPreTouch"
}

/// gcj j1/st: does the launcher section ask for
/// `-XX:+PrintStringTableStatistics` (the last spelling wins)? Pure.
pub(crate) fn print_string_table_statistics_requested(argv: &[String]) -> bool {
    let mut on = false;
    for a in argv.iter().skip(1).take_while(|a| a.as_str() != "--") {
        match a.as_str() {
            "-XX:+PrintStringTableStatistics" => on = true,
            "-XX:-PrintStringTableStatistics" => on = false,
            _ => {}
        }
    }
    on
}

/// gcj j1/st: `-XX:±PrintStringTableStatistics` and `-XX:StringTableSize=<n>`,
/// kept verbatim by the normaliser for [`extract_hotspot_flags`].
pub(crate) fn is_string_table_flag(arg: &str) -> bool {
    arg == "-XX:+PrintStringTableStatistics"
        || arg == "-XX:-PrintStringTableStatistics"
        || arg.starts_with("-XX:StringTableSize=")
}

/// zt t24/hunt7: `-XX:±UseTransparentHugePages` and `-XX:±UseLargePages`,
/// kept verbatim by the normaliser for [`extract_hotspot_flags`]
/// (`VmConfig::use_transparent_huge_pages`).
pub(crate) fn is_large_page_toggle(arg: &str) -> bool {
    matches!(
        arg,
        "-XX:+UseTransparentHugePages"
            | "-XX:-UseTransparentHugePages"
            | "-XX:+UseLargePages"
            | "-XX:-UseLargePages"
    )
}

/// zt t24/hunt7: HotSpot 25's `UseTransparentHugePages` as its argument
/// processing leaves it before `os::large_page_init` looks at the kernel:
/// the last `-XX:±UseTransparentHugePages`, turned off by an explicit
/// `-XX:-UseLargePages` (`os::large_page_init`: "The user explicitly turned
/// off large pages"). Linux only: the flag does not exist elsewhere (refused
/// by [`linux_only_flag_unrecognized_line`]; with the refusal killed or
/// `-XX:+IgnoreUnrecognizedVMOptions`, ignored). Pure.
pub(crate) fn effective_use_transparent_huge_pages(
    linux: bool,
    thp: Option<bool>,
    large_pages: Option<bool>,
) -> bool {
    linux && thp == Some(true) && large_pages != Some(false)
}

/// `CRATONVM_LAUNCHER_OBSOLETE_FLAG_WARNINGS` (zt t23/hunt4, default ON,
/// `=0` kill; read by `cratonvm_vm::config::obsolete_flag_warnings_enabled`,
/// shared with `libcratonvm`): an obsolete HotSpot flag prints HotSpot 25's
/// `Ignoring option` warning on stderr, in command-line order, as
/// `Arguments::process_argument` does; `=0` drops it silently, as before.
/// Every collector, both modes.
pub(crate) fn launcher_obsolete_flag_warnings_enabled() -> bool {
    cratonvm_vm::config::obsolete_flag_warnings_enabled()
}


/// zt t23/hunt4: HotSpot 25's warning for ONE argument naming an obsolete
/// flag ([`cratonvm_vm::config::hotspot_obsolete_flag_warning`]), or `None`.
///
/// gc-known-issues retire: and an alias's deprecation warning
/// (`hotspot25_develop_flags::ALIASES`, `-XX:+CreateMinidumpOnCrash`: `Option
/// CreateMinidumpOnCrash was deprecated in version 9.0 ...`), which HotSpot
/// prints for every spelling of the alias.
///
/// t1 wave 10, lane I24W: also `-Xverify:none` / `-noverify`'s deprecation
/// warning ([`NOVERIFY_DEPRECATION_WARNING`]).
pub(crate) fn obsolete_flag_warning_of(arg: &str) -> Option<String> {
    // t1 wave 10, lane I24W (`CRATONVM_LAUNCHER_NOVERIFY_DEPRECATION_WARNING`,
    // default ON, `=0` silent): HotSpot 25's `Arguments::parse_each_vm_init_arg`
    // warns once per `-Xverify:none` (libjli hands `-noverify` to the VM as
    // `-Xverify:none`), in command-line order. CratonVM's own `--noverify`
    // spelling and `-XX:-BytecodeVerificationRemote` stay silent.
    if (arg == "-Xverify:none" || arg == "-noverify")
        && cratonvm_vm::config::noverify_deprecation_warning_enabled()
    {
        return Some(NOVERIFY_DEPRECATION_WARNING.to_string());
    }
    cratonvm_vm::config::hotspot_obsolete_flag_warning(arg).or_else(|| {
        let (name, _) = xx_option_spelling(arg.strip_prefix("-XX:")?);
        let (_, _, warning) = hotspot25_develop_flags::ALIASES
            .iter()
            .find(|(a, _, _)| *a == name)?;
        Some(format!("OpenJDK 64-Bit Server VM warning: {warning}"))
    })
}

/// zt t23/hunt4: the obsolete-flag warnings of the option section of `argv`
/// (after `argv[0]`, up to `--`), in command-line order. HotSpot parses the
/// options left to right and stops at the first one it refuses, so a warning
/// is printed only when no option BEFORE it is refused: `refused_before`
/// answers that for the prefix `argv[..k]` (the launcher's ordered refusal
/// scan; `|_| false` when that scan does not run). The first refused prefix
/// ends the list. Pure over `refused_before`.
pub(crate) fn obsolete_flag_warning_lines(
    argv: &[String],
    mut refused_before: impl FnMut(&[String]) -> bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (k, a) in argv.iter().enumerate().skip(1) {
        if a == "--" {
            break;
        }
        if let Some(line) = obsolete_flag_warning_of(a) {
            if refused_before(&argv[..k]) {
                break;
            }
            lines.push(line);
        }
    }
    lines
}

/// zt t23/hunt4: [`obsolete_flag_warning_lines`] on stderr, flushed (before
/// the refusal or banner that may follow on the same stream).
pub(crate) fn print_obsolete_flag_warnings(lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    for line in lines {
        let _ = writeln!(err, "{line}");
    }
    let _ = err.flush();
}

/// `-XX:±ExitOnOutOfMemoryError` / `-XX:±CrashOnOutOfMemoryError`: the boolean
/// `report_java_out_of_memory` switches the normaliser keeps verbatim for
/// [`extract_hotspot_flags`]. gen r4w5/thrash5 (2026-09-24).
pub(crate) fn is_oom_report_toggle(arg: &str) -> bool {
    matches!(
        arg,
        "-XX:+ExitOnOutOfMemoryError"
            | "-XX:-ExitOnOutOfMemoryError"
            | "-XX:+CrashOnOutOfMemoryError"
            | "-XX:-CrashOnOutOfMemoryError"
    )
}

/// gcd d5/f (2026-09-28): the three spellings of `-XX:StartFlightRecording`
/// HotSpot accepts -- bare, `:<opts>` and `=<opts>` -- which the normaliser
/// keeps verbatim for [`extract_hotspot_flags`].
pub(crate) fn is_start_flight_recording(arg: &str) -> bool {
    arg == "-XX:StartFlightRecording"
        || arg.starts_with("-XX:StartFlightRecording:")
        || arg.starts_with("-XX:StartFlightRecording=")
}

/// gen r4w6/young6 (2026-09-24): the HotSpot tenuring flags the normaliser
/// keeps verbatim for [`extract_hotspot_flags`]. They fell into the silent
/// `-XX:` catch-all until this date.
pub(crate) fn is_tenuring_flag(arg: &str) -> bool {
    arg.starts_with("-XX:MaxTenuringThreshold=")
        || arg.starts_with("-XX:InitialTenuringThreshold=")
        || arg.starts_with("-XX:TargetSurvivorRatio=")
        || arg == "-XX:+PrintTenuringDistribution"
        || arg == "-XX:-PrintTenuringDistribution"
}

/// gci i23/tn: HotSpot 25's ceiling for `-XX:MaxTenuringThreshold` and
/// `-XX:InitialTenuringThreshold` (`range(0, markWord::max_age + 1)`: 16 is
/// `NeverTenure`).
pub(crate) const HOTSPOT_TENURING_THRESHOLD_MAX: u64 = 16;

/// gci i23/tn: HotSpot's `atojulong` -- an unsigned decimal or `0x` hex
/// number with an optional `k`/`m`/`g`/`t` multiplier (either case), nothing
/// else (no sign: a leading `-` or `+` is "Improperly specified"). `None` on
/// any other text or on overflow.
///
/// gci i48/rv48 (`gci-i24rv-...` item 14b): no blanks either. HotSpot 25's
/// `parse_integer` (`utilities/parseInteger.hpp`) refuses a first character
/// that is neither a digit nor `-` before it calls `strtoull` ("strtoll /
/// strtoull may allow leading spaces. Forbid it."), and a remainder of more
/// than one character, or one that is not a `k`/`m`/`g`/`t` multiplier,
/// fails too; so `' 5'` and `'5 '` are both `Improperly specified` there. The
/// value used to be trimmed here.
pub(crate) fn hotspot_parse_uintx(raw: &str) -> Option<u64> {
    let t = raw;
    let (digits, radix) = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => (hex, 16),
        None => (t, 10),
    };
    let end = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    let (number, suffix) = digits.split_at(end);
    if number.is_empty() {
        return None;
    }
    let n = u64::from_str_radix(number, radix).ok()?;
    let shift = match suffix {
        "" => 0,
        "k" | "K" => 10,
        "m" | "M" => 20,
        "g" | "G" => 30,
        "t" | "T" => 40,
        _ => return None,
    };
    n.checked_mul(1u64 << shift)
}

/// gen r4w6/young6: turn the extracted tenuring flags into the VM's
/// [`cratonvm_vm::TenuringConfig`].
///
/// gci i23/tn (`gci-i22sz-g1-survivor-target-is-twice-hotspots-RETIRED-UNFIXED-20261018` item
/// 5): validated as HotSpot 25 does -- a bad value refuses to start with
/// HotSpot's own lines (the caller adds the two `Error:` lines and exits 1),
/// where it used to be a `Warning:` and ignored. Ranges are HotSpot's:
/// `MaxTenuringThreshold` / `InitialTenuringThreshold` `0..=16` (16 =
/// `NeverTenure`; it was `0..=15` here), `TargetSurvivorRatio` `0..=100`.
///
/// * `-XX:MaxTenuringThreshold=` is parsed by `Arguments::parse_each_vm_init_arg`
///   itself (it also sets `AlwaysTenure` / `NeverTenure`): text that is not a
///   number is `Improperly specified VM option 'MaxTenuringThreshold=x'`; an
///   out-of-range number is the range line alone (`FLAG_SET_CMDLINE` fails
///   and the parser returns without the `Improperly specified` line).
/// * the other two go through `Arguments::process_argument`: text is the
///   `Improperly specified` line; out of range is the range line followed by
///   the `Improperly specified` line (as `heap_free_ratios_from_flags`).
///
/// HotSpot stops at the first bad option in command-line order; so does this
/// (gci i27/gs: `HotspotFlags::tenuring_values_in_order`). Hand-built flags
/// with no order fall back to the last occurrence of each, checked
/// `MaxTenuringThreshold`, then `InitialTenuringThreshold`, then
/// `TargetSurvivorRatio`.
pub(crate) fn tenuring_config_from_flags(
    flags: &HotspotFlags,
) -> Result<cratonvm_vm::TenuringConfig, Vec<String>> {
    use cratonvm_vm::TenuringConfig;
    let value = |name: &str,
                 raw: &Option<String>,
                 max: u64,
                 own_parser: bool|
     -> Result<Option<u8>, Vec<String>> {
        let Some(raw) = raw.as_ref() else {
            return Ok(None);
        };
        let improper = format!("Improperly specified VM option '{name}={raw}'");
        match hotspot_parse_uintx(raw) {
            None => Err(vec![improper]),
            Some(v) if v > max => {
                // gci i47w/ob47 (`vi27-gs.list` TO1 FAIL-SPELLING on HotSpot
                // 25.0.3): the three tenuring flags are declared `uint` in
                // HotSpot 25 (`gc_globals.hpp`), and the range checker prints
                // the flag's declared type, so the line reads `uint ...`.
                let range = format!("uint {name}={v} is outside the allowed range [ 0 ... {max} ]");
                Err(if own_parser { vec![range] } else { vec![range, improper] })
            }
            // `v <= max <= 100` here.
            Some(v) => Ok(Some(v as u8)),
        }
    };
    // gci i27/gs (`gci-i23tn-...` item 3): HotSpot validates every occurrence
    // in command-line order and stops at the first bad one, so
    // `-XX:MaxTenuringThreshold=17 -XX:MaxTenuringThreshold=5` refuses to
    // start there (the last-wins values below alone would accept it), and of
    // two different bad flags the EARLIER one is reported.
    for (which, raw) in &flags.tenuring_values_in_order {
        let raw = Some(raw.clone());
        match *which {
            0 => value("MaxTenuringThreshold", &raw, HOTSPOT_TENURING_THRESHOLD_MAX, true)?,
            1 => value("InitialTenuringThreshold", &raw, HOTSPOT_TENURING_THRESHOLD_MAX, false)?,
            _ => value("TargetSurvivorRatio", &raw, 100, false)?,
        };
    }
    let mut cfg = TenuringConfig::default();
    cfg.max_tenuring_threshold = value(
        "MaxTenuringThreshold",
        &flags.max_tenuring_threshold,
        HOTSPOT_TENURING_THRESHOLD_MAX,
        true,
    )?;
    cfg.initial_tenuring_threshold = value(
        "InitialTenuringThreshold",
        &flags.initial_tenuring_threshold,
        HOTSPOT_TENURING_THRESHOLD_MAX,
        false,
    )?;
    cfg.target_survivor_ratio =
        value("TargetSurvivorRatio", &flags.target_survivor_ratio, 100, false)?;
    cfg.print_tenuring_distribution = flags.print_tenuring_distribution.unwrap_or(false);
    Ok(cfg)
}

/// gci i43/lf (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`):
/// HotSpot 25's declarations of `-XX:SurvivorRatio` (`gc_globals.hpp`) and
/// the TLAB value flags (`tlab_globals.hpp`): `(name, type, lo, hi)`.
/// `TLABSize` has no `range(...)` clause (any `size_t`; its constraint is
/// checked after the heap is built, [`space_tlab_constraint_errors`]).
/// `max_uintx - 2`, `max_uintx / 2`, `max_juint` and `max_jint` are
/// HotSpot's own bounds.
pub(crate) const SPACE_TLAB_VALUE_FLAGS: [(&str, &str, u64, u64); 7] = [
    ("SurvivorRatio", "uintx", 1, u64::MAX - 2),
    ("MinTLABSize", "size_t", 1, u64::MAX / 2),
    ("TLABSize", "size_t", 0, u64::MAX),
    ("TLABAllocationWeight", "uintx", 0, 100),
    ("TLABWasteTargetPercent", "uintx", 1, 100),
    ("TLABRefillWasteFraction", "uintx", 1, u32::MAX as u64),
    ("TLABWasteIncrement", "uintx", 0, i32::MAX as u64),
];

/// gci i43/lf: `-XX:<Name>=<raw>` for a name of [`SPACE_TLAB_VALUE_FLAGS`]:
/// `Some((name, raw))`.
pub(crate) fn space_tlab_value_flag(arg: &str) -> Option<(&'static str, &str)> {
    let (name, raw) = arg.strip_prefix("-XX:")?.split_once('=')?;
    SPACE_TLAB_VALUE_FLAGS
        .iter()
        .find(|(n, ..)| *n == name)
        .map(|(n, ..)| (*n, raw))
}

/// gci i43/lf: the flags the normaliser keeps verbatim for
/// [`extract_hotspot_flags`]: the value flags of [`SPACE_TLAB_VALUE_FLAGS`]
/// and `-XX:±UseTLAB` / `-XX:±ResizeTLAB`. They fell into the silent `-XX:`
/// catch-all until 2026-10-16.
pub(crate) fn is_space_tlab_flag(arg: &str) -> bool {
    matches!(
        arg,
        "-XX:+UseTLAB" | "-XX:-UseTLAB" | "-XX:+ResizeTLAB" | "-XX:-ResizeTLAB"
    ) || space_tlab_value_flag(arg).is_some()
}

/// gci i43/lf: validate `-XX:SurvivorRatio` and the TLAB flags as HotSpot
/// 25's `Arguments::process_argument` does, and return
/// `(SurvivorRatio, TlabFlags)` (`None` / default = not given).
///
/// Text that is not an unsigned number (`hotspot_parse_uintx`) is
/// `Improperly specified VM option '<Name>=<raw>'`; a number outside the
/// flag's range is the range line (`uintx SurvivorRatio=0 is outside the
/// allowed range [ 1 ... 18446744073709551613 ]`, `size_t` for the two
/// sizes) followed by the `Improperly specified` line. HotSpot stops at the
/// first bad occurrence in command-line order; so does this. The last
/// occurrence of each flag wins. The caller adds the two `Error:` lines and
/// exits 1, as for the tenuring flags.
pub(crate) fn space_tlab_flags_from_hotspot_flags(
    flags: &HotspotFlags,
) -> Result<(Option<u64>, cratonvm_vm::config::TlabFlags), Vec<String>> {
    let mut survivor_ratio = None;
    let mut tlab = cratonvm_vm::config::TlabFlags {
        use_tlab: flags.use_tlab,
        resize_tlab: flags.resize_tlab,
        ..Default::default()
    };
    for (name, raw) in &flags.space_tlab_values_in_order {
        let Some(&(_, ty, lo, hi)) = SPACE_TLAB_VALUE_FLAGS.iter().find(|(n, ..)| n == name)
        else {
            continue;
        };
        let improper = format!("Improperly specified VM option '{name}={raw}'");
        let v = match hotspot_parse_uintx(raw) {
            None => return Err(vec![improper]),
            Some(v) if v < lo || v > hi => {
                return Err(vec![
                    format!("{ty} {name}={v} is outside the allowed range [ {lo} ... {hi} ]"),
                    improper,
                ])
            }
            Some(v) => v,
        };
        match *name {
            "SurvivorRatio" => survivor_ratio = Some(v),
            "MinTLABSize" => tlab.min_tlab_size = Some(v),
            "TLABSize" => tlab.tlab_size = Some(v),
            "TLABAllocationWeight" => tlab.allocation_weight = Some(v),
            "TLABWasteTargetPercent" => tlab.waste_target_percent = Some(v),
            "TLABRefillWasteFraction" => tlab.refill_waste_fraction = Some(v),
            _ => tlab.waste_increment = Some(v),
        }
    }
    Ok((survivor_ratio, tlab))
}

/// gci i43/lf: HotSpot 25's `(SpaceAlignment, aligned MaxHeapSize, ergonomic
/// TLAB maximum in bytes)` for the collector, as its `AfterMemoryInit`
/// constraints read them. `None` for a collector whose geometry this does
/// not model (none since gci zl1, 2026-10-17, which added ZGC).
///
/// * Serial (this VM's Generational): `SpaceAlignment` 64 KiB, `MaxHeapSize`
///   aligned up to the 2 MiB `HeapAlignment` (the card table's
///   `card_size * page_size`), and `CollectedHeap::max_tlab_size()`: an
///   `int[]` of `max_jint` elements -- `2 + 4 * (max_jint / 8)` words,
///   8 589 934 576 bytes.
/// * G1: `SpaceAlignment` = the region size
///   (`G1HeapRegion::setup_heap_region_size`: `-XX:G1HeapRegionSize` or
///   `clamp(MaxHeapSize / 2048, 1M, 32M)`, rounded up to a power of two),
///   `MaxHeapSize` aligned up to `max(region, 2 MiB)` (the card-table
///   constraint), and `G1CollectedHeap::max_tlab_size()`, the humongous
///   threshold: half a region.
/// * ZGC (gci zl1, zx48 A8): `SpaceAlignment` = `HeapAlignment` =
///   `ZGranuleSize`, 2 MiB (`ZArguments::initialize_alignments`), and
///   `ZCollectedHeap::max_tlab_size()`, `ZObjectSizeLimitSmall`: an eighth
///   of the 2 MiB small page, 256 KiB.
pub(crate) fn hotspot_space_tlab_geometry(
    gc: cratonvm_vm::config::GcAlgorithm,
    max_heap: usize,
    g1_region_size: Option<usize>,
) -> Option<(u64, u64, u64)> {
    const M: u64 = 1024 * 1024;
    let max_heap = max_heap as u64;
    let align_up = |n: u64, a: u64| n.div_ceil(a).saturating_mul(a);
    match gc {
        cratonvm_vm::config::GcAlgorithm::Generational => {
            let tlab_max_words = 2 + 4 * (i32::MAX as u64 / 8);
            Some((64 * 1024, align_up(max_heap, 2 * M), tlab_max_words * 8))
        }
        cratonvm_vm::config::GcAlgorithm::G1 => {
            let region = hotspot_g1_region_bytes(max_heap, g1_region_size);
            // `G1Arguments::calculate_heap_alignment`: at least the card
            // table's 2 MiB, at least a region.
            Some((region, align_up(max_heap, region.max(2 * M)), region / 2))
        }
        cratonvm_vm::config::GcAlgorithm::Zgc => {
            Some((2 * M, align_up(max_heap, 2 * M), 256 * 1024))
        }
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// gci i43/lf (a helper since gci zl1): HotSpot 25's G1 region size in bytes
/// (`G1HeapRegion::setup_heap_region_size`): `-XX:G1HeapRegionSize` when
/// given, else `clamp(MaxHeapSize / 2048, 1M, 32M)`; rounded up to a power of
/// two.
pub(crate) fn hotspot_g1_region_bytes(max_heap: u64, g1_region_size: Option<usize>) -> u64 {
    const M: u64 = 1024 * 1024;
    match g1_region_size {
        Some(r) if r > 0 => r as u64,
        _ => (max_heap / 2048).clamp(M, 32 * M),
    }
    .max(1)
    .next_power_of_two()
}

/// gci zl1 (zx48 A4, `CRATONVM_GC_HEAP_ALIGN_HOTSPOT`): HotSpot 25's
/// `HeapAlignment`, to which `GCArguments::initialize_heap_flags_and_sizes`
/// aligns `MaxHeapSize`, `InitialHeapSize` and `MinHeapSize` up. Serial:
/// [`HOTSPOT_SERIAL_HEAP_ALIGNMENT`], 2 MiB. G1: `max(region, 2 MiB)`
/// (`G1Arguments::calculate_heap_alignment`), the region as
/// [`hotspot_space_tlab_geometry`] computes it from the unaligned maximum.
/// ZGC: `ZGranuleSize`, 2 MiB.
pub(crate) fn hotspot_heap_alignment(
    gc: cratonvm_vm::config::GcAlgorithm,
    max_heap: usize,
    g1_region_size: Option<usize>,
) -> Option<usize> {
    match gc {
        cratonvm_vm::config::GcAlgorithm::Generational
        | cratonvm_vm::config::GcAlgorithm::Zgc => Some(HOTSPOT_SERIAL_HEAP_ALIGNMENT),
        cratonvm_vm::config::GcAlgorithm::G1 => {
            let region = hotspot_g1_region_bytes(max_heap as u64, g1_region_size);
            let region = usize::try_from(region).unwrap_or(usize::MAX);
            Some(region.max(HOTSPOT_SERIAL_HEAP_ALIGNMENT))
        }
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// gci i43/lf: HotSpot 25's `AfterMemoryInit` constraint lines for the
/// values [`space_tlab_flags_from_hotspot_flags`] accepted
/// (`jvmFlagConstraintsGC.cpp`). HotSpot checks every constraint and prints
/// every failure before it refuses to start (rc 1), so this returns all of
/// them, in flag-table order (`SurvivorRatio`, then `MinTLABSize`, then
/// `TLABSize`). Empty = start.
///
/// * `SurvivorRatioConstraintFunc` (command line only):
///   `SurvivorRatio > MaxHeapSize / SpaceAlignment`.
/// * `MinTLABSizeConstraintFunc` (always; the default 2048 passes): above the
///   ergonomic TLAB maximum. Its first test, `MinTLABSize <
///   alignment_reserve_in_bytes()`, is NOT checked here: the reserve is the
///   C2 allocation-prefetch reserve, host-CPU dependent (row `lf43_c_mt*`
///   records HotSpot's value; page item).
/// * `TLABSizeConstraintFunc` (command line only): below `MinTLABSize`, then
///   above the ergonomic TLAB maximum.
pub(crate) fn space_tlab_constraint_errors(
    survivor_ratio: Option<u64>,
    tlab: &cratonvm_vm::config::TlabFlags,
    geometry: Option<(u64, u64, u64)>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let Some((space_alignment, max_heap, tlab_max)) = geometry else {
        return lines;
    };
    if let Some(sr) = survivor_ratio {
        let limit = max_heap / space_alignment.max(1);
        if sr > limit {
            lines.push(format!(
                "SurvivorRatio ({sr}) must be less than or equal to ergonomic SurvivorRatio maximum ({limit})"
            ));
        }
    }
    let min_tlab = tlab.min_tlab_size.unwrap_or(cratonvm_vm::config::TlabFlags::HOTSPOT_MIN_TLAB_SIZE);
    if min_tlab > tlab_max {
        lines.push(format!(
            "MinTLABSize ({min_tlab}) must be less than or equal to ergonomic TLAB maximum ({tlab_max})"
        ));
    }
    if let Some(size) = tlab.tlab_size {
        if size < min_tlab {
            lines.push(format!(
                "TLABSize ({size}) must be greater than or equal to MinTLABSize ({min_tlab})"
            ));
        } else if size > tlab_max {
            lines.push(format!(
                "TLABSize ({size}) must be less than or equal to ergonomic TLAB maximum size ({tlab_max})"
            ));
        }
    }
    lines
}

/// gci i47w/j43: HotSpot 25's G1 `YoungPLABSize` / `OldPLABSize`
/// `AfterMemoryInit` constraint lines, which follow the TLAB lines
/// (`vi43-lf` row `lf43_r_three_g1`, HotSpot 25.0.3). A command-line
/// `MinTLABSize` (`min_tlab_size`, bytes) makes `PLAB::startup_initialization`
/// `FLAG_SET_ERGO` both PLAB sizes to `max(PLAB::min_size(), default)` (the
/// defaults 4096 / 1024 words), with `PLAB::min_size()` =
/// `align_object_size(max(MinTLABSize / HeapWordSize, header_size)) +
/// lab_alignment_reserve()` = `MinTLABSize / 8 + 2` words (the reserve is the
/// `int[]` filler header, 2 words); `MaxPLABSizeBounds` (G1 and Parallel
/// only) then refuses a value above `PLAB::max_size()`, the TLAB maximum in
/// words (`tlab_max_bytes / 8`). The minimum bound cannot fire on a value that
/// is at least `PLAB::min_size()`. A `MinTLABSize` at its default leaves both
/// flags at their defaults, which every G1 region size admits. Empty = no
/// line. G1 only: the caller passes this for G1 alone (Serial has no PLAB
/// constraint). `-XX:YoungPLABSize` / `-XX:OldPLABSize` on the command line
/// are not modelled (this launcher does not parse them).
pub(crate) fn g1_plab_constraint_errors(min_tlab_size: Option<u64>, tlab_max_bytes: u64) -> Vec<String> {
    const HEAP_WORD: u64 = 8;
    const OOP_HEADER_WORDS: u64 = 2;
    const LAB_ALIGNMENT_RESERVE_WORDS: u64 = 2;
    let Some(min_tlab) = min_tlab_size else {
        return Vec::new();
    };
    let plab_min = (min_tlab / HEAP_WORD).max(OOP_HEADER_WORDS) + LAB_ALIGNMENT_RESERVE_WORDS;
    let plab_max = tlab_max_bytes / HEAP_WORD;
    [("YoungPLABSize", 4096_u64), ("OldPLABSize", 1024_u64)]
        .into_iter()
        .map(|(name, default)| (name, default.max(plab_min)))
        .filter(|&(_, value)| value > plab_max)
        .map(|(name, value)| {
            format!("{name} ({value}) must be less than or equal to ergonomic PLAB maximum size ({plab_max})")
        })
        .collect()
}

/// Pull HotSpot-style flags out of raw argv.
///
/// This runs before clap so the bare `-XX:+Foo` / `-agentlib:` spellings
/// don't confuse its parser. Flags we recognize are removed from the
/// returned vector; anything we don't recognize passes through unchanged
/// so clap can still reject unknown flags with a useful error.
pub(crate) fn extract_hotspot_flags(raw: Vec<String>) -> (Vec<String>, HotspotFlags) {
    let mut filtered = Vec::with_capacity(raw.len());
    let mut out = HotspotFlags::default();
    let mut past_separator = false;
    for arg in raw {
        // Tokens after `--` are program arguments — pass through unchanged.
        if past_separator {
            filtered.push(arg);
            continue;
        }
        if arg == "--" {
            past_separator = true;
            filtered.push(arg);
            continue;
        }
        match arg.as_str() {
            // Boolean toggles: -XX:+Foo / -XX:-Foo
            "-XX:+HeapDumpOnOutOfMemoryError" => out.heap_dump_on_oom = Some(true),
            "-XX:-HeapDumpOnOutOfMemoryError" => out.heap_dump_on_oom = Some(false),
            // gc-common w8-g: explicit-GC policy (`VmConfig::explicit_gc`).
            "-XX:+DisableExplicitGC" => out.disable_explicit_gc = Some(true),
            "-XX:-DisableExplicitGC" => out.disable_explicit_gc = Some(false),
            "-XX:+ExplicitGCInvokesConcurrent" => out.explicit_gc_invokes_concurrent = Some(true),
            "-XX:-ExplicitGCInvokesConcurrent" => out.explicit_gc_invokes_concurrent = Some(false),
            // gen r4w5/thrash5 — HotSpot's other `report_java_out_of_memory`
            // switches.
            "-XX:+ExitOnOutOfMemoryError" => out.exit_on_oom = Some(true),
            "-XX:-ExitOnOutOfMemoryError" => out.exit_on_oom = Some(false),
            "-XX:+CrashOnOutOfMemoryError" => out.crash_on_oom = Some(true),
            "-XX:-CrashOnOutOfMemoryError" => out.crash_on_oom = Some(false),
            // gcf f2/o — the GC-overhead limit (`VmConfig::use_gc_overhead_limit`).
            "-XX:+UseGCOverheadLimit" => out.use_gc_overhead_limit = Some(true),
            "-XX:-UseGCOverheadLimit" => out.use_gc_overhead_limit = Some(false),
            // gen r4w6/young6 — the tenuring distribution print.
            "-XX:+PrintTenuringDistribution" => out.print_tenuring_distribution = Some(true),
            "-XX:-PrintTenuringDistribution" => out.print_tenuring_distribution = Some(false),
            // gci i43/lf — the two TLAB toggles (`VmConfig::tlab_flags`).
            "-XX:+UseTLAB" => out.use_tlab = Some(true),
            "-XX:-UseTLAB" => out.use_tlab = Some(false),
            "-XX:+ResizeTLAB" => out.resize_tlab = Some(true),
            "-XX:-ResizeTLAB" => out.resize_tlab = Some(false),
            // Interpreter round i2 wave 59, lane L1 (`VmConfig::disable_attach_mechanism`).
            "-XX:+UsePerfData" => out.use_perf_data = Some(true),
            "-XX:-UsePerfData" => out.use_perf_data = Some(false),
            "-XX:+PerfDisableSharedMem" => out.perf_disable_shared_mem = Some(true),
            "-XX:-PerfDisableSharedMem" => out.perf_disable_shared_mem = Some(false),
            "-XX:+DisableAttachMechanism" => out.disable_attach_mechanism = Some(true),
            "-XX:-DisableAttachMechanism" => out.disable_attach_mechanism = Some(false),
            // Interpreter round i2 wave 60, lane L1 (`VmConfig::enable_dynamic_agent_loading`).
            "-XX:+EnableDynamicAgentLoading" => out.enable_dynamic_agent_loading = Some(true),
            "-XX:-EnableDynamicAgentLoading" => out.enable_dynamic_agent_loading = Some(false),
            // nio21 wave 16, lane L5 (`VmConfig::omit_stack_trace_in_fast_throw`).
            "-XX:+OmitStackTraceInFastThrow" => out.omit_stack_trace_in_fast_throw = Some(true),
            "-XX:-OmitStackTraceInFastThrow" => out.omit_stack_trace_in_fast_throw = Some(false),
            // Interpreter round i4 wave 84, lane L2 (`VmConfig::show_hidden_frames`).
            "-XX:+ShowHiddenFrames" => out.show_hidden_frames = Some(true),
            "-XX:-ShowHiddenFrames" => out.show_hidden_frames = Some(false),
            // Wave 85, lane L2 (`VmConfig::stack_trace_in_throwable`).
            "-XX:+StackTraceInThrowable" => out.stack_trace_in_throwable = Some(true),
            "-XX:-StackTraceInThrowable" => out.stack_trace_in_throwable = Some(false),
            // Interpreter round i3 wave 79, lane L3 (`VmConfig::reduce_signal_usage`).
            "-Xrs" | "-XX:+ReduceSignalUsage" => out.reduce_signal_usage = Some(true),
            "-XX:-ReduceSignalUsage" => out.reduce_signal_usage = Some(false),
            // zt t21/tg10: `VmConfig::class_unloading`.
            "-XX:+ClassUnloading" => out.class_unloading = Some(true),
            "-XX:-ClassUnloading" | "-Xnoclassgc" => out.class_unloading = Some(false),
            // zt t23/hunt4: `VmConfig::always_pre_touch`.
            "-XX:+AlwaysPreTouch" => out.always_pre_touch = Some(true),
            "-XX:-AlwaysPreTouch" => out.always_pre_touch = Some(false),
            // gcj j1/st: `VmConfig::print_string_table_statistics`.
            "-XX:+PrintStringTableStatistics" => out.print_string_table_statistics = Some(true),
            "-XX:-PrintStringTableStatistics" => out.print_string_table_statistics = Some(false),
            // gcj j1/st: `VmConfig::string_table_size`.
            s if s.starts_with("-XX:StringTableSize=") => {
                if let Ok(n) = s["-XX:StringTableSize=".len()..].parse::<usize>() {
                    out.string_table_size = Some(n);
                }
            }
            // zt t24/hunt7: `VmConfig::use_transparent_huge_pages`.
            "-XX:+UseTransparentHugePages" => out.use_transparent_huge_pages = Some(true),
            "-XX:-UseTransparentHugePages" => out.use_transparent_huge_pages = Some(false),
            "-XX:+UseLargePages" => out.use_large_pages = Some(true),
            "-XX:-UseLargePages" => out.use_large_pages = Some(false),
            // obsaudit D12 — bare form, no options.
            "-XX:StartFlightRecording" => {
                out.jfr_start_recording = Some(String::new());
                out.jfr_start_recordings.push(String::new());
            },
            _ => {
                if let Some(rest) = arg.strip_prefix("-XX:HeapDumpPath=") {
                    out.heap_dump_path = Some(rest.to_string());
                } else if let Some(rest) = arg.strip_prefix("-XX:OnOutOfMemoryError=") {
                    // A `ccstrlist`: each occurrence appends, `\n`-joined.
                    out.on_oom_error = Some(match out.on_oom_error.take() {
                        Some(prev) => format!("{prev}\n{rest}"),
                        None => rest.to_string(),
                    });
                } else if let Some(rest) = arg.strip_prefix("-XX:StartFlightRecording:") {
                    // obsaudit D12 — options form.
                    out.jfr_start_recording = Some(rest.to_string());
                    out.jfr_start_recordings.push(rest.to_string());
                } else if let Some(rest) = arg.strip_prefix("-XX:StartFlightRecording=") {
                    // gcd d5/f — HotSpot's `=` spelling of the options form
                    // (`-XX:StartFlightRecording=filename=x.jfr`), same options.
                    out.jfr_start_recording = Some(rest.to_string());
                    out.jfr_start_recordings.push(rest.to_string());
                } else if let Some(rest) = arg.strip_prefix("-XX:MaxTenuringThreshold=") {
                    // gen r4w6/young6 — the three tenuring values; last wins.
                    // gci i27/gs: every occurrence is also kept in order.
                    out.max_tenuring_threshold = Some(rest.to_string());
                    out.tenuring_values_in_order.push((0, rest.to_string()));
                } else if let Some(rest) = arg.strip_prefix("-XX:InitialTenuringThreshold=") {
                    out.initial_tenuring_threshold = Some(rest.to_string());
                    out.tenuring_values_in_order.push((1, rest.to_string()));
                } else if let Some(rest) = arg.strip_prefix("-XX:TargetSurvivorRatio=") {
                    out.target_survivor_ratio = Some(rest.to_string());
                    out.tenuring_values_in_order.push((2, rest.to_string()));
                } else if let Some((name, rest)) = space_tlab_value_flag(&arg) {
                    // gci i43/lf — `-XX:SurvivorRatio=` and the TLAB values.
                    out.space_tlab_values_in_order.push((name, rest.to_string()));
                } else if let Some(rest) =
                    arg.strip_prefix("-XX:DiagnoseSyncOnValueBasedClasses=")
                {
                    // Interpreter round i5 wave 102, lane L6; last one wins.
                    out.diagnose_sync_on_value_based_classes = Some(rest.to_string());
                } else if arg.starts_with("-agentlib:")
                    || arg.starts_with("-agentpath:")
                    || arg.starts_with("-javaagent:")
                {
                    out.agent_options.push(arg);
                } else if let Some(options) = arg
                    .strip_prefix("-Xrunjdwp")
                    .filter(|rest| rest.is_empty() || rest.starts_with(':'))
                {
                    // Interpreter round i4 wave 85, lane L1: HotSpot loads
                    // `-Xrun<lib>[:<options>]` as `-agentlib:<lib>[=<options>]`
                    // (`Arguments::parse_each_vm_init_arg`), and JDI's
                    // `CommandLineLaunch` has long built its debuggee's
                    // command line with `-Xdebug -Xrunjdwp:...`.
                    let options = options.strip_prefix(':');
                    out.agent_options.push(match options {
                        Some(options) => format!("-agentlib:jdwp={options}"),
                        None => "-agentlib:jdwp".to_string(),
                    });
                } else if arg == "-Xdebug" || arg == "-Xnoagent" {
                    // Accepted and without effect, as on HotSpot (wave 85).
                } else {
                    filtered.push(arg);
                }
            }
        }
    }
    (filtered, out)
}

/// obsaudit D12 (2026-07-26) — parse `-XX:StartFlightRecording[:opts]`'s
/// comma-separated `key=value` options into a `JfrStartRecordingConfig`.
/// `raw` is `""` for the bare flag (all defaults).
///
/// Recognized keys: `filename`, `duration`, `maxage`, `maxevents`,
/// `dumponexit`. Unlike most of the rest of this parser (which silently
/// drops flags it doesn't specifically recognize so clap can report unknown
/// long options), an unrecognized key *inside* `StartFlightRecording:` is a
/// hard error — a typo'd sub-option here has no other layer that will ever
/// catch it, and JFR's whole failure mode in this audit is options that
/// silently do nothing.
/// Collect `+<EventName>#enabled=<bool>` tokens out of a
/// `-XX:StartFlightRecording:` option string.
///
/// B10 (2026-09-01). Split from [`parse_jfr_start_recording_opts`] rather than
/// folded into it because the two answer different questions about the same
/// string — that one builds a `JfrStartRecordingConfig`, this one names events
/// on `VmConfig` — and because the grammar itself belongs to
/// `cratonvm_vm::config::apply_jfr_event_setting`, which the jcmd `JFR.start`
/// surface calls too. One grammar, two callers; the spelling is HotSpot's own.
///
/// Naming an event **narrows** the recording to exactly the names given:
/// `RecordingSettings::enabled_event_names` is simultaneously the producer
/// arming set and the drain whitelist, so there is no way to say "arm this one
/// and keep everything else". `Recording.enable(...)` from Java behaves the
/// same way, so the two routes agree rather than diverging.
pub(crate) fn parse_jfr_event_settings(raw: &str) -> Result<Option<Vec<String>>, String> {
    let mut events: Option<Vec<String>> = None;
    for pair in raw.split(',') {
        let pair = pair.trim();
        if !pair.starts_with('+') {
            continue;
        }
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            format!(
                "-XX:StartFlightRecording: event setting `{pair}` is missing `=value` (expected `+<EventName>#enabled=true`)"
            )
        })?;
        cratonvm_vm::config::apply_jfr_event_setting(&mut events, key, value)?;
    }
    Ok(events)
}

pub(crate) fn parse_jfr_start_recording_opts(
    raw: &str,
) -> Result<cratonvm_vm::config::JfrStartRecordingConfig, String> {
    parse_jfr_start_recording_opts_with(raw, false)
}

/// `CRATONVM_GEN_JFR_NAME_MAXSIZE_OPTIONS` (gcj j2/ob2 rows on j2a; default ON,
/// `=0` kill): on Generational, `-XX:StartFlightRecording` takes `name=` and
/// `maxsize=` as HotSpot 25.0.3 does (rows `ob2_cap_jfr_name` /
/// `ob2_cap_jfr_size`: HotSpot started the recording; this launcher ended the
/// run with `unrecognized option`). The name and the size limit are accepted
/// and not modelled (this recorder names its recording `cratonvm` and bounds
/// it by `maxevents`). G1 / ZGC unchanged.
pub(crate) fn gen_jfr_name_maxsize_options_enabled() -> bool {
    true
}

/// [`parse_jfr_start_recording_opts`]; `hotspot_options`: also take `name=`
/// and `maxsize=` (gcj j2/ob2, [`gen_jfr_name_maxsize_options_enabled`]).
pub(crate) fn parse_jfr_start_recording_opts_with(
    raw: &str,
    hotspot_options: bool,
) -> Result<cratonvm_vm::config::JfrStartRecordingConfig, String> {
    let mut cfg = cratonvm_vm::config::JfrStartRecordingConfig {
        dump_on_exit: true, // HotSpot's default for -XX:StartFlightRecording
        ..Default::default()
    };
    if raw.is_empty() {
        return Ok(cfg);
    }
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            format!("-XX:StartFlightRecording: option `{pair}` is missing `=value`")
        })?;
        // B10: an event setting, not a recording option. `apply_jfr_event_setting`
        // in `cratonvm_vm::config` owns the grammar; `parse_jfr_event_settings`
        // below makes the second pass that collects them. Skipping here rather
        // than erroring is what lets one comma-separated option string carry both
        // kinds, exactly as HotSpot's does.
        if key.starts_with('+') {
            continue;
        }
        match key {
            "name" | "maxsize" if hotspot_options => {}
            "filename" => cfg.filename = Some(value.to_string()),
            "duration" => cfg.duration = Some(parse_jfr_duration(value)?),
            "maxage" => cfg.max_age = Some(parse_jfr_duration(value)?),
            "maxevents" => {
                cfg.max_events = Some(value.parse::<usize>().map_err(|_| {
                    format!("-XX:StartFlightRecording: maxevents=`{value}` is not a number")
                })?);
            }
            "dumponexit" => {
                cfg.dump_on_exit = match value {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(format!(
                            "-XX:StartFlightRecording: dumponexit=`{value}` must be true or false"
                        ))
                    }
                };
            }
            other => {
                return Err(format!(
                    "-XX:StartFlightRecording: unrecognized option `{other}` (supported: filename, duration, maxage, maxevents, dumponexit, +<EventName>#enabled=true)"
                ))
            }
        }
    }
    Ok(cfg)
}

/// Parse a HotSpot-style duration: plain digits (seconds) or digits with a
/// trailing `s`/`m`/`h`/`d` unit suffix (seconds/minutes/hours/days).
pub(crate) fn parse_jfr_duration(value: &str) -> Result<std::time::Duration, String> {
    let (digits, mult) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1u64),
        Some('m') => (&value[..value.len() - 1], 60),
        Some('h') => (&value[..value.len() - 1], 3600),
        Some('d') => (&value[..value.len() - 1], 86400),
        _ => (value, 1),
    };
    let secs: u64 = digits
        .parse()
        .map_err(|_| format!("`{value}` is not a valid duration (e.g. 30s, 5m, 1h)"))?;
    Ok(std::time::Duration::from_secs(secs.saturating_mul(mult)))
}

// ---------------------------------------------------------------------------
// JDK-mode selection and reporting
//
// CratonVM ships two complete, different standard-library implementations
// (real JDK bytecode vs ~5,200 synthetic Rust stubs). Which one ran decides
// which bug set applies, so:
//
//   * selection is explicit and deterministic — never inferred from what is
//     installed on the host (see `cratonvm_vm::config::JdkMode`);
//   * an unavailable mode is a hard launch error, never a silent downgrade;
//   * the active mode is printed by `-version` / `-Xinternalversion` and in
//     the launcher's fatal-error output, so every bug report carries it.
// ---------------------------------------------------------------------------

/// The mode this process actually booted in, published for the diagnostic
/// paths in `main()` (which run after `run()` has returned an error and no
/// longer have the `VmConfig`).
pub(crate) static ACTIVE_JDK_MODE: std::sync::OnceLock<(cratonvm_vm::config::JdkMode, Option<String>)> =
    std::sync::OnceLock::new();

/// One-line "which class library is this" summary for diagnostics.
pub(crate) fn active_jdk_mode_line() -> String {
    match ACTIVE_JDK_MODE.get() {
        Some((mode, Some(home))) => format!("jdk mode: {mode} (java.home={home})"),
        Some((mode, None)) => format!("jdk mode: {mode}"),
        // NOT "during argument parsing". This arm is reached whenever the
        // OnceLock is unset, and the class library is selected LATE: a
        // `--java-home` that clap accepted but that carries no `jmods/` or
        // `lib/modules` lands here too, and that is the common case (a POSIX
        // path spelling on Windows reaches it with rc=1). Naming a phase this
        // function cannot observe sent two separate triage records after the
        // argument parser for a fault that was never in it.
        None => "jdk mode: <not yet resolved — the failure occurred before the class library was selected>"
            .into(),
    }
}

/// Which flavour of version banner the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionQuery {
    /// `-version` (stderr, exits) / `--version` (stdout, exits).
    Version,
    /// `-fullversion` / `--full-version`: single terse line.
    Full,
    /// `-Xinternalversion`: build + JDK-mode diagnostics.
    Internal,
    /// `-showversion` / `--show-version`: print, then run the program.
    Show,
}

impl VersionQuery {
    /// HotSpot writes the single-dash forms to stderr and the GNU-style
    /// double-dash forms to stdout. Build tools that scrape `java -version`
    /// depend on the stderr side of that.
    pub(crate) fn to_stdout(self, token: &str) -> bool {
        let _ = self;
        token.starts_with("--")
    }

    pub(crate) fn exits(self) -> bool {
        !matches!(self, VersionQuery::Show)
    }
}

/// Scan the launcher section of argv (everything before the `--` that
/// [`insert_program_args_separator`] parks after the program selector) for a
/// version query. Returns the matched token as well so the caller can pick
/// the right output stream.
pub(crate) fn scan_version_query(argv: &[String]) -> Option<(VersionQuery, String)> {
    for a in argv.iter().skip(1) {
        if a == "--" {
            return None;
        }
        let q = match a.as_str() {
            "-version" | "--version" | "-v" | "-V" => VersionQuery::Version,
            "-fullversion" | "--full-version" => VersionQuery::Full,
            "-Xinternalversion" => VersionQuery::Internal,
            "-showversion" | "--show-version" => VersionQuery::Show,
            _ => continue,
        };
        return Some((q, a.clone()));
    }
    None
}

/// zr z9/hp: a version query whose banner waits for a VM boot
/// ([`version_query_boots_vm`]).
pub(crate) struct VersionBoot {
    pub(crate) banner: String,
    pub(crate) to_stdout: bool,
    /// zt t21/tg10: the exit status after the banner: 0 for a version query
    /// and a help option, 1 for the concise usage (no program named).
    /// zt t24/m5 (dev cabdb9f4ae's `exits`): `None` for a `-showversion`
    /// boot (`-XX:+PrintFlagsFinal`, a start-up recording), whose banner is
    /// printed from the booted VM and the launch then goes on to the program.
    pub(crate) exit_code: Option<i32>,
    /// zt t24/m5: a `-showversion` boot that names no program prints the
    /// concise usage on stderr after its banner (libjli's `JavaMain`:
    /// `PrintJavaVersion`, then `PrintUsage`, exit 1).
    pub(crate) usage_after: bool,
}

impl VersionBoot {
    /// Does the launch end after the banner (dev cabdb9f4ae's `exits`)?
    pub(crate) fn exits(&self) -> bool {
        self.exit_code.is_some()
    }
}

/// `CRATONVM_VERSION_XLOG_BOOT` -- default ON (zr z9/hp); `=0` keeps
/// `-version` on its no-VM fast path even with `-Xlog`.
pub(crate) fn version_xlog_boot_enabled() -> bool {
    true
}

/// Whether the class library a version boot would need is there: the banner
/// already reports a missing one (`NONE FOUND` / `NOT COMPILED IN`), and a
/// boot would only turn that report into a launch failure.
pub(crate) fn version_library_resolves(
    mode: cratonvm_vm::config::JdkMode,
    explicit_java_home: Option<&str>,
) -> bool {
    match mode {
        cratonvm_vm::config::JdkMode::Real => {
            cratonvm_vm::config::require_real_jdk(explicit_java_home).is_ok()
        }
        cratonvm_vm::config::JdkMode::Synthetic => cratonvm_vm::config::SYNTHETIC_JDK_COMPILED_IN,
    }
}

/// zr z9/hp: whether a version query boots the VM before its banner. HotSpot
/// creates the VM for `-version` (its `-Xlog` startup lines -- `[gc] Using
/// ...`, the `gc,init` block -- and exit lines are printed around the
/// banner); this launcher prints the banner without a VM, which is right and
/// cheaper as long as nothing would be logged. So: only a query that exits
/// (`-version`, `--version`; not `-showversion`, which boots anyway, nor the
/// one-line `-fullversion` / diagnostic `-Xinternalversion` forms), only with
/// an `-Xlog` option in the launcher section, only with the class library
/// present, and only while `CRATONVM_VERSION_XLOG_BOOT` is on. gci i12/td
/// made the same fix (the gci round's `version_waits_for_vm_init`, folded in
/// here at the 2026-10-15 merge of dev).
pub(crate) fn version_query_boots_vm(
    query: VersionQuery,
    argv: &[String],
    switch_on: bool,
    library_resolves: bool,
) -> bool {
    if !switch_on || !library_resolves || query != VersionQuery::Version {
        return false;
    }
    xlog_selects_output(argv)
}

/// gc-known-issues retire (`CRATONVM_GEN_HOTSPOT_YOUNG_ERGO`): a `-version`
/// boots the VM when the launcher section gives a young-size flag (`-Xmn`,
/// `-XX:NewSize=`, `-XX:MaxNewSize=`, `-XX:NewRatio=`): HotSpot sizes the
/// generations while it creates the VM for `-version`, so its
/// `[warning][gc,ergo]` lines (and Serial's bootstrap failure) come before
/// the banner; the no-VM fast path would print the banner alone.
pub(crate) fn version_young_ergo_boots(query: VersionQuery, argv: &[String], library_resolves: bool) -> bool {
    library_resolves
        && query == VersionQuery::Version
        && argv.iter().skip(1).take_while(|a| a.as_str() != "--").any(|a| {
            a.starts_with("-Xmn")
                || ["-XX:NewSize=", "-XX:MaxNewSize=", "-XX:NewRatio="]
                    .iter()
                    .any(|p| a.starts_with(p))
        })
}

/// zki (page `zgc-zt23hunt4-transparent-huge-pages-flag-ignored` item 4): a
/// `-version` boots the VM when the command line asks a ZGC VM for explicit
/// large pages on Linux and the hugetlbfs pool passes HotSpot's sanity
/// mapping (`CRATONVM_LARGE_PAGES_EXPLICIT`): HotSpot creates the VM before
/// its banner, and an initial heap the pool cannot hold ends the launch
/// there (host row `zki_b5`: four `[error][gc] Failed to commit memory`
/// lines, `Failed to allocate initial Java heap (126M)`, exit 1). The no-VM
/// fast path would print the banner and exit 0.
pub(crate) fn version_explicit_large_pages_boots(
    query: VersionQuery,
    argv: &[String],
    library_resolves: bool,
) -> bool {
    if !cfg!(target_os = "linux") || !library_resolves || query != VersionQuery::Version {
        return false;
    }
    let mut thp = None;
    let mut large_pages = None;
    for a in argv.iter().skip(1).take_while(|a| a.as_str() != "--") {
        match a.as_str() {
            "-XX:+UseTransparentHugePages" => thp = Some(true),
            "-XX:-UseTransparentHugePages" => thp = Some(false),
            "-XX:+UseLargePages" => large_pages = Some(true),
            "-XX:-UseLargePages" => large_pages = Some(false),
            _ => {}
        }
    }
    large_pages == Some(true)
        && !effective_use_transparent_huge_pages(true, thp, large_pages)
        && scan_effective_zgc(argv)
        && cratonvm_vm::runtime::unified_logging::large_pages_explicit_enabled()
        && cratonvm_vm::runtime::unified_logging::hugetlb_sanity_check()
}

/// Does the launcher section of `argv` (before `--`) select `-Xlog` output?
///
/// zt t21/tg10 (`CRATONVM_VERSION_BOOT_VERBOSE_GC`, default ON, `=0` kill):
/// `-verbose:gc` too, which is HotSpot's `-Xlog:gc` (and this launcher's,
/// under `CRATONVM_VERBOSE_GC_IS_XLOG_GC`): `java -verbose:gc -version`
/// prints `[info][gc] Using The Z Garbage Collector` before the banner on
/// HotSpot 25, and the version query here kept its no-VM fast path.
pub(crate) fn xlog_selects_output(argv: &[String]) -> bool {
    let verbose_gc_logs =
        true;
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        // gci i12/td: `-Xlog:disable` alone selects nothing to print.
        .any(|a| {
            ((a == "-Xlog" || a.starts_with("-Xlog:")) && a != "-Xlog:disable")
                || (verbose_gc_logs && a == "-verbose:gc")
        })
}

/// `CRATONVM_LAUNCHER_USAGE_XLOG_BOOT` (zt t21/tg10, page `zgc-zt12tg5-*`
/// item 5; default ON, `=0` kill): with an `-Xlog` selection, the concise
/// usage (no program named) and a help option's text
/// ([`HotspotHelpQuery`]) are printed from a booted VM, as the version boot
/// prints its banner ([`VersionBoot`]): libjli's `JavaMain` creates the VM
/// before `PrintUsage`, so HotSpot's `-Xlog:gc` startup lines (`Using The Z
/// Garbage Collector`, ...) come first and its exit lines after (rows
/// `tg7_u3`, `tg8_h15`: HotSpot's stdout had the log line, this launcher's
/// none). Without an `-Xlog` selection a boot prints nothing more, so the
/// text keeps its no-VM fast path. `=0`: always the fast path.
pub(crate) fn launcher_usage_xlog_boot_enabled() -> bool {
    true
}

/// zt t21/tg10: would a usage or help text on this command line be printed
/// from a booted VM ([`launcher_usage_xlog_boot_enabled`])? The version
/// boot's own conditions: its switch, an `-Xlog` selection and a class
/// library to boot.
pub(crate) fn usage_query_boots_vm(argv: &[String]) -> bool {
    launcher_usage_xlog_boot_enabled()
        && version_xlog_boot_enabled()
        && xlog_selects_output(argv)
        && version_library_resolves(
            scan_requested_jdk_mode(argv),
            scan_explicit_java_home(argv).as_deref(),
        )
}

/// Remove the first occurrence of `token` from the launcher section of
/// argv (everything before the `--` separator). Program arguments after
/// `--` are never touched — a Java program is entitled to its own
/// `-showversion`.
pub(crate) fn remove_first_launcher_token(argv: &mut Vec<String>, token: &str) {
    for i in 1..argv.len() {
        if argv[i] == "--" {
            return;
        }
        if argv[i] == token {
            argv.remove(i);
            return;
        }
    }
}

/// Resolve the requested JDK mode from raw argv, for the version banner.
///
/// The authoritative resolution happens after clap parsing
/// ([`resolve_jdk_mode`]); this pre-parse scan exists only because the
/// banner must be printable before clap runs (clap's own `--version`
/// handling would otherwise exit first, printing a banner that says nothing
/// about which standard library is in play).
pub(crate) fn scan_requested_jdk_mode(argv: &[String]) -> cratonvm_vm::config::JdkMode {
    let mut mode = cratonvm_vm::config::LAUNCHER_DEFAULT_JDK_MODE;
    for a in argv.iter().skip(1) {
        if a == "--" {
            break;
        }
        match a.as_str() {
            "--synthetic-jdk" => mode = cratonvm_vm::config::JdkMode::Synthetic,
            // `--real-jdk` and `--compatible` select a *policy*, but both also
            // require the real class library (contract §9: "--real-jdk implies
            // JdkMode::Real", and `--compatible` is what `--real-jdk` used to
            // spell). The banner must say so, or `cratonvm --real-jdk -version`
            // would report whichever library the bare default names.
            "--real-jdk" | "--compatible" | "--fast-boot" => {
                mode = cratonvm_vm::config::JdkMode::Real
            }
            _ => {}
        }
    }
    mode
}

/// Resolve the requested compatibility mode from raw argv, for the version
/// banner. The counterpart of [`scan_requested_jdk_mode`], and equally
/// non-authoritative: [`resolve_compatibility_mode`] decides for the run.
///
/// A conflicting `--real-jdk --synthetic-jdk` or `--real-jdk --fast-boot`
/// pair is not diagnosed here — the banner is a report, not a gate, and the
/// real diagnosis (which names both flags and the fix) happens in `run()`.
/// This scan mirrors [`resolve_compatibility_mode`]'s precedence so that a
/// banner and the run it precedes cannot disagree: any explicit
/// compatible-side token wins over `--real-jdk`, exactly as the authoritative
/// path's early `Compatible` return does.
pub(crate) fn scan_requested_compatibility_mode(argv: &[String]) -> cratonvm_vm::config::CompatibilityMode {
    let mut saw_real_jdk = false;
    for a in argv.iter().skip(1) {
        if a == "--" {
            break;
        }
        match a.as_str() {
            "--compatible" | "--fast-boot" | "--synthetic-jdk" => {
                return cratonvm_vm::config::CompatibilityMode::Compatible;
            }
            "--real-jdk" => saw_real_jdk = true,
            _ => {}
        }
    }
    if saw_real_jdk {
        return cratonvm_vm::config::CompatibilityMode::JdkOnly;
    }
    cratonvm_vm::config::LAUNCHER_DEFAULT_COMPATIBILITY_MODE
}

/// Pick up an explicit `--java-home <PATH>` / `--java-home=<PATH>` from raw
/// argv so the version banner reports the same JDK the run would use.
/// The JDK the launch will boot, for the launcher's own boot-layer checks
/// (`rewrite_module_launch`, the `--limit-modules` pre-check): the VM's own
/// resolution (`config::resolve_java_home_public`: `--java-home`, then
/// `CRATONVM_JAVA_HOME`, `JAVA_HOME`, then the `java` on `PATH`). These
/// checks read `--java-home` or `JAVA_HOME` alone until interpreter round i6
/// wave 116, lane L5, so a launch that found its JDK on `PATH` could not tell
/// an absent module from a JDK one: `-m m.absent` printed the no-main-class
/// text and `-m m.needs` (requiring `m.absent`) ran, where HotSpot's boot
/// layer aborts with `FindException: Module m.absent not found[, required by
/// m.needs]` (probe `L5W86ModuleLaunch` run with no `--java-home`).
pub(crate) fn launcher_java_home(explicit: Option<String>) -> Option<String> {
    cratonvm_vm::config::resolve_java_home_public(explicit.as_deref())
        .map(|p| p.to_string_lossy().into_owned())
}

pub(crate) fn scan_explicit_java_home(argv: &[String]) -> Option<String> {
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--" {
            return None;
        }
        if let Some(rest) = a.strip_prefix("--java-home=") {
            return Some(rest.to_string());
        }
        if a == "--java-home" {
            return it.next().cloned();
        }
    }
    None
}

/// Render the version banner, always naming the active class library.
///
/// This is the primary fix for "a bug report is uninterpretable without
/// knowing which mode ran": `cratonvm -version` now states it, so the mode
/// travels with every pasted terminal transcript.
pub(crate) fn version_banner(
    query: VersionQuery,
    mode: cratonvm_vm::config::JdkMode,
    compatibility: cratonvm_vm::config::CompatibilityMode,
    explicit_java_home: Option<&str>,
) -> String {
    version_banner_with(query, mode, compatibility, explicit_java_home, false)
}

/// [`version_banner`]; `interpreted`: the launch asked for `-Xint` and the
/// JIT is off (`CRATONVM_LAUNCHER_XINT`), so the build line says
/// `interpreted mode` as `java.vm.info` will (wave 138, lane L1b).
pub(crate) fn version_banner_with(
    query: VersionQuery,
    mode: cratonvm_vm::config::JdkMode,
    compatibility: cratonvm_vm::config::CompatibilityMode,
    explicit_java_home: Option<&str>,
    interpreted: bool,
) -> String {
    use cratonvm_vm::config as cfg;
    let version = env!("CARGO_PKG_VERSION");

    if query == VersionQuery::Full {
        return format!(
            "cratonvm full version \"{version}\" ({mode}, {})\n",
            compatibility.as_str()
        );
    }

    let mut out = String::new();
    out.push_str(&format!("cratonvm version \"{version}\"\n"));
    // The `java -version` second line. HotSpot fills this parenthetical from
    // `java.vm.info`, so the execution-mode list here is the one
    // `cratonvm_vm::vm::vm_info_mode_list` will put in that property — the same
    // function, so the banner and the property cannot spell the policy two
    // different ways (they used to: `mixed mode, jdk-only` vs
    // `compatibility=jdk-only`).
    //
    // The banner then adds `compatibility=<mode>` on top, which the property
    // deliberately does NOT carry. Two reasons it belongs here and not there:
    // a banner is grepped by scripts that must not have to parse a comma list,
    // and `-version` prints before any VM exists, so this line cannot read the
    // property it has to agree with. Machine readers grep for
    // `compatibility=jdk-only`. Under `Compatible` the line is what it has
    // always been (real-jdk-mode.md §10) except that it no longer says
    // `sharing`: gci i33/ms, this VM has no CDS archive and HotSpot
    // `-Xshare:off` prints `mixed mode` alone (gci i39/rw: comment corrected).
    out.push_str(&format!(
        "CratonVM (build {version}, {}, compatibility={})\n",
        cratonvm_vm::vm::vm_info_execution_mode_list(compatibility, interpreted),
        compatibility.as_str()
    ));
    out.push_str(&format!(
        "JDK class library: {mode} — {}\n",
        mode.describe()
    ));

    match mode {
        cfg::JdkMode::Real => match cfg::require_real_jdk(explicit_java_home) {
            Ok(home) => out.push_str(&format!("JDK class library root: {}\n", home.display())),
            Err(_) => {
                out.push_str(
                    "JDK class library root: NONE FOUND — a program launch in this mode \
                     will fail (launch a program for the full diagnostic, or pass \
                     --synthetic-jdk to select the other library)\n",
                );
            }
        },
        cfg::JdkMode::Synthetic => {
            if !cfg::SYNTHETIC_JDK_COMPILED_IN {
                out.push_str(
                    "JDK class library root: NOT COMPILED IN — this binary was built \
                     without the `synthetic-jdk` Cargo feature, so a program launch in \
                     this mode will fail\n",
                );
            }
        }
    }

    if query == VersionQuery::Internal {
        out.push('\n');
        out.push_str(&format!("jdk.mode.active                = {mode}\n"));
        out.push_str(&format!(
            "jdk.compatibility.mode          = {}\n",
            compatibility.as_str()
        ));
        out.push_str(&format!(
            "jdk.mode.default.launcher       = {}\n",
            cfg::LAUNCHER_DEFAULT_JDK_MODE
        ));
        out.push_str(&format!(
            "jdk.mode.default.embedded       = {}\n",
            cfg::EMBEDDED_DEFAULT_JDK_MODE
        ));
        out.push_str(
            "jdk.mode.selection              = explicit flag or fixed default \
             (never host-detected)\n",
        );
        out.push_str(&format!(
            "jdk.mode.synthetic_compiled_in  = {}\n",
            cfg::SYNTHETIC_JDK_COMPILED_IN
        ));
        out.push_str("jdk.search:\n");
        out.push_str(&cfg::describe_jdk_search(explicit_java_home));
        out.push('\n');
    }
    out
}

/// Authoritative JDK-mode resolution + availability validation.
///
/// Returns the selected mode and, in real-JDK mode, the validated
/// `JAVA_HOME` root. An unavailable mode is an error — the launcher does
/// **not** fall back to the other class library, because a run whose
/// standard library was chosen by the host is neither reproducible nor
/// reportable.
pub(crate) fn resolve_jdk_mode(
    synthetic_flag: bool,
    real_flag: bool,
    explicit_java_home: Option<&str>,
) -> Result<(cratonvm_vm::config::JdkMode, Option<std::path::PathBuf>)> {
    use cratonvm_vm::config as cfg;

    // clap enforces this via `conflicts_with`; keep the check so a future
    // argv-preprocessing change can't quietly make one flag win.
    if synthetic_flag && real_flag {
        bail!(
            "--synthetic-jdk cannot be combined with --real-jdk or --fast-boot (or --compatible): \
             they select two different standard-library implementations. Pass \
             exactly one (or neither, for the default {}). Note --synthetic-jdk \
             already implies the compatible policy, so --fast-boot adds nothing to it.",
            cfg::LAUNCHER_DEFAULT_JDK_MODE
        );
    }

    let mode = if synthetic_flag {
        cfg::JdkMode::Synthetic
    } else if real_flag {
        cfg::JdkMode::Real
    } else {
        cfg::LAUNCHER_DEFAULT_JDK_MODE
    };

    match mode {
        cfg::JdkMode::Synthetic => {
            cfg::require_synthetic_jdk().map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((mode, None))
        }
        cfg::JdkMode::Real => {
            let home =
                cfg::require_real_jdk(explicit_java_home).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((mode, Some(home)))
        }
    }
}

// ===========================================================================
// Real-JDK mode (docs/book/src/user-guide/real-jdk-mode.md §9).
//
// The launcher owns three things for this feature: resolving the policy flag
// into a `CompatibilityMode`, draining the violation logs for `--trace-jdk-only`
// / `--explain-jdk-only`, and writing the three census artefacts that
// `difftest`'s `census.rs` parses. Everything below is hand-rolled JSON in the
// style of `SharedVm::dump_missing_natives_json`, sorted so the files are
// diff-stable against a committed baseline.
// ===========================================================================

/// Authoritative compatibility-policy resolution.
///
/// The table, which `docs/book/src/user-guide/real-jdk-mode.md` §9 mirrors:
///
/// | argv | policy |
/// |---|---|
/// | *(nothing)* | `LAUNCHER_DEFAULT_COMPATIBILITY_MODE` = `JdkOnly` |
/// | `--real-jdk` | `JdkOnly` (restates the default) |
/// | `--fast-boot` / `--compatible` | `Compatible` |
/// | `--synthetic-jdk` | `Compatible` (implied; there is no strict synthetic) |
/// | `--real-jdk --fast-boot` | error |
/// | `--real-jdk --synthetic-jdk` | error |
///
/// Runs **before** [`resolve_jdk_mode`] deliberately. Both flag pairs are
/// mutually exclusive, but they are exclusive for different reasons, and the
/// user deserves the specific one: `--real-jdk --synthetic-jdk` is a *policy*
/// conflict (the synthetic library is made of exactly the substitutions
/// `--real-jdk` forbids), whereas `--fast-boot --synthetic-jdk` is a *library*
/// conflict. Resolving the policy first means the policy diagnosis wins;
/// resolving the library first would report "two different standard-library
/// implementations", which is true but unhelpful.
///
/// Why `--synthetic-jdk` *implies* `Compatible` instead of erroring: before
/// the default flipped, a bare `--synthetic-jdk` resolved to `Compatible`
/// because that was the default. Reading the new default into it would make
/// `--synthetic-jdk` alone a hard error via
/// `VmConfig::validate_compatibility` — a flag that worked for a year turned
/// into a configuration failure by a change it has nothing to do with. The
/// pair is only an error when the argv *states* both, which is a
/// contradiction rather than an omission.
pub(crate) fn resolve_compatibility_mode(
    real_jdk: bool,
    fast_boot: bool,
    synthetic_flag: bool,
) -> Result<cratonvm_vm::config::CompatibilityMode> {
    use cratonvm_vm::config::CompatibilityMode;

    // clap enforces these via `conflicts_with`; keep the checks so a future
    // argv-preprocessing change can't quietly make one flag win.
    if real_jdk && synthetic_flag {
        bail!(
            "--real-jdk and --synthetic-jdk cannot be combined.\n\
             \n\
             --real-jdk means real JDK class bytes are authoritative: no \
             fabricated compatibility class and no synthetic-stub native may be \
             registered or invoked. The synthetic class library is ~5,200 such \
             stubs, so the combination selects a VM with no usable class \
             library.\n\
             \n\
             Pass --real-jdk against a real JDK (--java-home <PATH>), or drop \
             --real-jdk and keep --synthetic-jdk for the standalone library."
        );
    }
    if real_jdk && fast_boot {
        bail!(
            "--real-jdk and --fast-boot cannot be combined: they name the two \
             opposite compatibility policies.\n\
             \n\
             --real-jdk means real JDK class bytes are authoritative -- no \
             fabricated compatibility class, no synthetic-stub native \
             registered or invoked. --fast-boot permits both. There is no \
             ordering between them that makes the pair mean something, so the \
             launcher refuses rather than letting the last flag win.\n\
             \n\
             --real-jdk is the default, so pass neither for strict mode, \
             or pass --fast-boot alone to enable the compatibility layer."
        );
    }

    // `--synthetic-jdk` implies `Compatible`; see the doc comment.
    if synthetic_flag || fast_boot {
        return Ok(CompatibilityMode::Compatible);
    }
    if real_jdk {
        return Ok(CompatibilityMode::JdkOnly);
    }
    Ok(cratonvm_vm::config::LAUNCHER_DEFAULT_COMPATIBILITY_MODE)
}

/// Contract §9: `CRATONVM_REAL=-stubs` keeps working as a native-registry
/// filter, but it cannot express the class-loading or dispatch half of the
/// policy — it drops synthetic-stub *registrations* and says nothing about
/// fabricated classes or about a stub shadowing real bytecode. A run that
/// asked for it almost certainly wanted the strict policy, so say so once.
///
/// Takes the **resolved** mode, not the raw `--real-jdk` flag. Since the
/// default flipped, a run with no policy flag at all is already strict, and a
/// note telling it to "pass --real-jdk" would be advice to restate what it is
/// already doing.
///
/// Both spellings are checked: the expanded per-knob key (which
/// `flag_groups::expand_process_env` writes back into the environment during
/// `main()`) and the raw grouped token, so the note still fires if the
/// expansion order ever changes or the variable is set after expansion.
pub(crate) fn note_no_stubs_env_without_jdk_only(mode: cratonvm_vm::config::CompatibilityMode) {
    if mode.is_jdk_only() {
        return;
    }
    let expanded = std::env::var_os("CRATONVM_NO_STUBS").is_some();
    let raw_token = std::env::var("CRATONVM_REAL")
        .map(|v| {
            v.split(',')
                .any(|t| matches!(t.trim(), "-stubs" | "no-stubs" | "stubs=off"))
        })
        .unwrap_or(false);
    if !(expanded || raw_token) {
        return;
    }
    eprintln!(
        "[cratonvm] CRATONVM_REAL=-stubs drops synthetic-stub native \
         registrations only. It cannot reject a fabricated compatibility class, \
         and it cannot stop a registered native from shadowing real bytecode. \
         This run opted out of the full policy with --compatible; drop that flag \
         for strict mode (add --real-jdk-report <FILE> to census what it would \
         reject)."
    );
}

/// The oldest image `--real-jdk` targets (`docs/feature-designs/real-jdk-mode.md`'s stage
/// table names JDK 21 and 25).
pub(crate) const OLDEST_JDK_ONLY_FEATURE: u32 = 21;

/// Say so, once, when strict mode runs over an image older than JDK 21.
///
/// Strict mode binds the image's own natives and boot sequence, which this VM
/// implements for JDK 21 and 25. A JDK 17 `java.home` declares natives and
/// `VM` hooks those images do not (`Thread.sleep(J)V`, `Object.wait(J)V`,
/// `VM.setJavaLangInvokeInited`, `NativeLibraries.newInstance`), so the boot
/// dies inside `System`'s initialisation -- and, with `System.out` never built,
/// it used to print NOTHING and exit 0, which a test battery reads as a clean
/// run (measured 2026-10-05, JDK 17.0.20.1). A warning rather than a refusal:
/// an unknown version (`None`) and `--compatible` are left alone.
pub(crate) fn note_jdk_only_image_below_supported(feature: Option<u32>) {
    if let Some(feature) = feature.filter(|f| *f < OLDEST_JDK_ONLY_FEATURE) {
        eprintln!(
            "[cratonvm] warning: java.home is a JDK {feature} image; --real-jdk (the \
             launcher default) supports JDK {OLDEST_JDK_ONLY_FEATURE} and 25, and an \
             older image is expected to fail during boot, possibly with no output. \
             Pass --compatible, or point --java-home at a JDK 21 or 25 installation."
        );
    }
}

/// The JDK feature version of the runtime image, read from
/// `$JAVA_HOME/release`'s `JAVA_VERSION=` line.
///
/// Thin adapter over `cratonvm_vm::vm::jdk_feature_from_release_file`, which is
/// the same function `SharedVm::jdk_feature_version` uses to fill the report's
/// `jdk_feature`. It lives in `vm` so a `--trace-jdk-only` line and the report
/// written at the end of the same run cannot disagree about which JDK they are
/// talking about. `None` rather than a guess: a fabricated number is worse than
/// an absent one — `null` reads as "not measured", a wrong `25` reads as a fact.
pub(crate) fn detect_jdk_feature(java_home: Option<&str>) -> Option<u32> {
    cratonvm_vm::vm::jdk_feature_from_release_file(java_home?)
}

/// `JarFile.runtimeVersion()` of the VM this launch boots, for the launcher's
/// own module-path scans (`rewrite_module_launch`, the `--module-path` and
/// `--limit-modules` pre-checks): the booted library's feature version
/// (`booted`; unknown or synthetic: 25), capped by `-Djdk.util.jar.version`
/// and turned off by `-Djdk.util.jar.enableMultiRelease=false` -- the rule
/// `vm_init`'s `jar_runtime_feature` applies to the VM's own scans
/// (`ClassPath::jar_runtime_version`). These scans read multi-release jars at
/// 25 before interpreter round i5 wave 105, lane L5, so under a JDK 21 library
/// a jar whose 22..25 version directories change a package or a provider got
/// a `FindException` text or `-m` main class HotSpot 21 does not give.
pub(crate) fn launch_jar_runtime_feature(
    booted: Option<u32>,
    jar_version: Option<&str>,
    enable_multi_release: Option<&str>,
) -> u32 {
    cratonvm_classloading::ClassPath::jar_runtime_version(
        booted.unwrap_or(cratonvm_classloading::module::MODULE_PATH_JAR_FEATURE),
        jar_version,
        enable_multi_release,
    )
}

/// The last `(key, value)` of `extract_system_properties`' list named `key`
/// (HotSpot: the last `-D` wins).
pub(crate) fn last_system_property<'a>(props: &'a [(String, String)], key: &str) -> Option<&'a str> {
    props
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The last `-D<key>=<value>` of raw `argv` before `--` (HotSpot: the last one
/// wins), for [`launch_jar_runtime_feature`] before `extract_system_properties`
/// runs.
pub(crate) fn argv_system_property<'a>(argv: &'a [String], key: &str) -> Option<&'a str> {
    let prefix = format!("-D{key}=");
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .filter_map(|a| a.strip_prefix(prefix.as_str()))
        .last()
}

// JDK-ONLY CENSUS WRITERS: there are none here any more, on purpose.
//
// This file used to carry its own schema-2 native census, class-origin census
// and Real-JDK report, written independently of the ones in
// `vm/src/vm/vm_init.rs`. Two writers stamping the same `schema_version` with
// two different shapes is a consumer hazard, not a redundancy: a reader that
// works against one silently mis-reads the other. The `vm` writers survived
// (they can fill `real_declaring_method` from already-loaded classes without
// perturbing the census, which the launcher could not), and
// `write_jdk_only_dumps` below now calls them. The launcher's contributions —
// path redaction under `--explain-jdk-only`, the `(kind, summary)` violation
// sort, the `OriginBuckets` partition — moved with them.
//
// The JSON helpers (`json_escape`, `json_string`, `json_opt_string`) and the
// path redaction went the same way. `redact_absolute_paths` is imported back
// from `cratonvm_vm::vm` for the `--trace-jdk-only` lines below, so the trace
// and the artefacts cannot disagree about what counts as a private path.

/// How far each append-only violation log has already been reported.
///
/// **Class-origin violations no longer come through here.** They were the one
/// recording site that fires throughout the run, so polling made every mid-run
/// fabrication surface at shutdown, detached from the code that caused it —
/// contract §9 asks for the opposite. `ClassManager` now carries a VM-scoped
/// `set_violation_sink` that `run()` installs immediately after the `vm-init`
/// drain, and this drain skips their
/// *rendering* (but still advances `origins`) once that sink exists, so each
/// violation is printed exactly once, live.
///
/// The remaining four sites are still polled, and for each of them that is the
/// right answer rather than a deferral:
///
/// * `NativeMethodRegistry::refused_registrations` — every refusal happens
///   inside `Vm::new`, and the `vm-init` drain immediately follows it, so the
///   poll already reports them at their real time of occurrence.
/// * The three process sinks behind `SharedVm::jdk_only_process_violations` —
///   these are JIT/dispatch refusals, and they are *process*-global (retired
///   record: additional-wave2-markers-not-in-the-original-inventory.md §2). Note that
///   §2's own subject — the JIT compatibility latch — is no longer one of
///   them; what remains process-global here is the violation SINKS, not the
///   policy. Giving them a live sink means giving them a VM first; a per-VM sink
///   hung off process-global state would report another VM's violations as
///   this one's, which is worse than reporting them late.
///
/// The three JIT/dispatch sinks are drained on **the same schedule** as the
/// other two, so a traced run and `--real-jdk-report` name the same set of
/// violations. In practice their `vm-init` drain is almost always empty and
/// everything lands at `shutdown`: nothing has been compiled or dispatched yet
/// when `Vm::new` returns. That is a property of *when refusals happen*, not a
/// limitation of the drain — no sink here is shutdown-only, and a caller that
/// added a mid-run drain point would get the entries recorded so far.
///
/// Each sink is bounded (256 entries) and internally deduplicated, and none of
/// them ever shrinks, so a positional watermark stays valid across drains: once
/// a sink saturates, `len()` stops growing and the drain correctly reports
/// nothing new.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ViolationWatermark {
    pub(crate) origins: usize,
    pub(crate) registrations: usize,
    /// One watermark per slot of [`cratonvm_vm::vm::JDK_ONLY_PROCESS_SINKS`],
    /// in that constant's documented order. Indexed positionally on purpose:
    /// the array's order is a contract precisely so this stays a plain index.
    pub(crate) process_sinks: [usize; cratonvm_vm::vm::JDK_ONLY_PROCESS_SINKS],
    /// Refusal-event totals as of the last drain, so the summary line reports a
    /// delta rather than a running total that looks like a per-phase count.
    pub(crate) refusals: cratonvm_vm::vm::JdkOnlyRefusalCounts,
}

/// One trace line for a violation.
///
/// `--explain-jdk-only` selects the long-form operator report (which the
/// contract requires to end with the `--real-jdk` fallback hint) and keeps
/// absolute paths; otherwise the one-line summary, redacted.
pub(crate) fn render_violation(
    violation: &cratonvm_types::error::JdkOnlyViolation,
    jdk_feature: Option<u32>,
    explain: bool,
) -> String {
    if explain {
        violation.render(jdk_feature, true)
    } else {
        // The same redaction the census writers apply, from the same function
        // in `vm`: a traced violation and the report row for that violation
        // must not disagree about what counts as a private path.
        cratonvm_vm::vm::redact_absolute_paths(&violation.summary())
    }
}

/// Print every violation recorded since the last drain.
pub(crate) fn trace_jdk_only_violations(
    shared: &cratonvm_vm::SharedVm,
    watermark: &mut ViolationWatermark,
    phase: &str,
    explain: bool,
) {
    let jdk_feature = detect_jdk_feature(shared.config.java_home.as_deref());
    let mut lines: Vec<String> = Vec::new();
    {
        let class_manager = shared.classes.class_manager.read();
        let recorded = class_manager.origin_violations();
        // Skip the rendering — not the watermark — once the live sink is
        // installed: every violation past that point has already been printed
        // at the instant it was recorded, and printing it again here would
        // double-report it. Before the sink exists (the `vm-init` drain, and
        // any run without `--trace-jdk-only`) this is still the only reporter.
        if !class_manager.has_violation_sink() {
            for violation in recorded.iter().skip(watermark.origins) {
                lines.push(render_violation(violation, jdk_feature, explain));
            }
        }
        watermark.origins = recorded.len();
    }
    {
        let refused = shared.natives.native_methods.refused_registrations();
        for violation in refused.iter().skip(watermark.registrations) {
            lines.push(render_violation(violation, jdk_feature, explain));
        }
        watermark.registrations = refused.len();
    }
    // The three process-global sinks the report also folds — JIT compile-time
    // refusals, JIT fast-path refusals, interpreter bytecode-wins observations.
    // Drained here on the same schedule as the two above so a traced run and
    // `--real-jdk-report` cannot disagree about which violations occurred.
    //
    // `jdk_only_process_violations()` returns three empty vectors in
    // `Compatible` mode without touching the sinks, so this loop costs a mode
    // test and three empty iterations on a default `--real-jdk` run.
    let process_sinks = shared.jdk_only_process_violations();
    // §1.7's class-origin field, resolved for the same reason and in the same
    // place as in `dump_jdk_only_report_json_with`: the three sinks are filled
    // by dispatch-path helpers that hold a name triple and no class store, so
    // the lookup happens once, cold, on the reporting side. The two surfaces
    // must not disagree about a row's origin, so both fill it from
    // `dump_class_origins()` and neither invents one.
    //
    // Taken here rather than inside the loop: one read lock for the whole
    // drain, and every row in one drain then describes the same instant.
    let class_origins: std::collections::HashMap<String, String> = {
        let class_manager = shared.classes.class_manager.read();
        class_manager
            .dump_class_origins()
            .into_iter()
            .map(|e| (e.name, e.origin))
            .collect()
    };
    for (slot, sink) in process_sinks.iter().enumerate() {
        for violation in sink.iter().skip(watermark.process_sinks[slot]) {
            let resolved = violation
                .clone()
                .with_class_origin(class_origins.get(violation.class()).cloned());
            lines.push(render_violation(&resolved, jdk_feature, explain));
        }
        watermark.process_sinks[slot] = sink.len();
    }
    for line in lines {
        eprintln!("[cratonvm][jdk-only:{phase}] {line}");
    }
    // Refusal counters, as a delta since the last drain. These are *events*,
    // not violations: they are uncapped and undeduplicated, whereas the lines
    // above are one-per-distinct-triple and capped at 256 per sink. Printing
    // them per-event would bury the identities under repetition, and printing
    // nothing would make a traced run silently disagree with the `refusals`
    // block of the report. One summary line per drain is the compromise.
    //
    // `jit_inline_cache_natives` has no violation object behind it at all — the
    // inline-cache publication site has an entry address, not a name triple —
    // so this line is the only place a traced run can see that refusal class.
    let refusals = shared.jdk_only_refusal_counts();
    if refusals != watermark.refusals {
        eprintln!(
            "[cratonvm][jdk-only:{phase}] refusals since last drain: \
             jit-direct-native-binds={} jit-inline-cache-natives={} \
             jit-fastpath-admissions={} jit-direct-helper-fastpaths={} \
             jit-direct-helper-admissions={} interpreter-bytecode-preferred={}",
            refusals
                .jit_direct_native_binds
                .saturating_sub(watermark.refusals.jit_direct_native_binds),
            refusals
                .jit_inline_cache_natives
                .saturating_sub(watermark.refusals.jit_inline_cache_natives),
            refusals
                .jit_fastpath_admissions
                .saturating_sub(watermark.refusals.jit_fastpath_admissions),
            refusals
                .jit_direct_helper_fastpaths
                .saturating_sub(watermark.refusals.jit_direct_helper_fastpaths),
            // The second term on this line that is not a refusal, and the one
            // that makes the first readable: a zero above means "strict policy
            // blocked no thin helper" only if a thin helper ran at all.
            refusals
                .jit_direct_helper_admissions
                .saturating_sub(watermark.refusals.jit_direct_helper_admissions),
            refusals
                .interpreter_bytecode_preferred
                .saturating_sub(watermark.refusals.interpreter_bytecode_preferred),
            // t1 wave 6, lane Y3: `interpreter-shadow-unenforced` (a `Bridge`
            // that shadowed real bytes and ran) is gone. Bytecode-first
            // dispatch (`3d31008561`) left it no recorder, so it read 0.
        );
    }
    watermark.refusals = refusals;
}

/// Write whichever of the three census artefacts were requested.
///
/// Called from every exit path that has a live VM, including the failing ones:
/// `difftest` categorises a *failing* strict run from these files, so a run that
/// dies on the violation it was launched to find must still leave the census
/// behind. Writes at most once per process (first caller wins — the failure
/// path is the informative one).
/// The absolute path a dump flag's operand actually names, for printing.
///
/// # Why every dump message goes through this
///
/// A dump flag's operand is trusted verbatim and resolved by the OS, and on
/// Windows a POSIX-looking path is neither rejected nor mapped: `/tmp/reg.json`
/// resolves against the current drive to `C:\tmp\reg.json`. The write then
/// succeeds and the caller — typically a Git Bash shell, where `/tmp` means
/// something else entirely — goes looking in the wrong place and reads the
/// absence of the file as the flag having failed. Printing the resolved
/// absolute path turns that into a one-glance answer, on the success line as
/// well as the failure line, because the success case is the one that misleads.
///
/// Falls back to the operand as given if the path cannot be made absolute
/// (empty operand, or a platform error): a diagnostic must never be the thing
/// that fails.
pub(crate) fn absolute_dump_path(path: &str) -> String {
    std::path::absolute(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// Why a dump write failed, in the terms that let a caller fix it: the absolute
/// path attempted, and whether the directory it would have gone in exists.
///
/// The OS error alone is not enough. `os error 3` — and its localised text,
/// which on this host is not English — says "the system cannot find the path",
/// which is true of a missing parent directory, a typo'd drive letter and a
/// POSIX path alike. Naming the parent and saying whether it exists separates
/// those three in one line.
pub(crate) fn describe_dump_failure(path: &str, error: &impl std::fmt::Display) -> String {
    let absolute = absolute_dump_path(path);
    let parent = std::path::Path::new(&absolute)
        .parent()
        .map(|p| p.display().to_string());
    match parent {
        Some(dir) if !std::path::Path::new(&dir).is_dir() => format!(
            "{error} (tried to write {absolute}; its directory {dir} does not exist — \
             create it, or pass a path under an existing directory)"
        ),
        Some(dir) => format!("{error} (tried to write {absolute}; its directory {dir} exists)"),
        None => format!("{error} (tried to write {absolute})"),
    }
}

/// The standing caveat on the census's `invocations` column, printed with every
/// successful `--dump-native-registry`.
///
/// MEASURED 2026-08-17 and recorded in
/// `docs/feature-designs/real-jdk-mode.md`:
/// the column counts dispatches that resolved the triple by name or id, and
/// misses every dispatch served from a pre-resolved function pointer — the
/// interpreter's intrinsic table and the JIT's thin direct-call helpers. 100,000
/// `Math.abs` calls report 1; the same run under `CRATONVM_DISABLE_INTRINSICS=1`
/// reports 100,000.
///
/// It is printed on the *success* line, next to the number, because that is
/// where a reader is standing when they decide what the column means. A caveat
/// that lives only in `--help` or only in a design doc is a caveat that gets
/// quoted around; a dozen records in `docs/known-issues/jdk-only/` already quote
/// this tool's output.
pub(crate) fn census_invocations_caveat(nojit: bool, intrinsics_disabled: bool) -> &'static str {
    if nojit && intrinsics_disabled {
        // Both bypass families are off, so the column is a total for
        // everything measured. Say so — a lane that went to the trouble of
        // configuring an exact census should be told it got one.
        "; `invocations` is an exact count in this configuration \
         (--nojit + CRATONVM_DISABLE_INTRINSICS=1)"
    } else {
        "; `invocations` is a LOWER BOUND, not a call count — intrinsic-table and \
         JIT direct-call dispatches are not counted. For an exact census re-run with \
         --nojit and CRATONVM_DISABLE_INTRINSICS=1 (see G33-1)"
    }
}

pub(crate) fn write_jdk_only_dumps(args: &Args, shared: &cratonvm_vm::SharedVm) {
    if args.dump_class_origins.is_none()
        && args.dump_native_registry.is_none()
        && args.jdk_only_report.is_none()
    {
        return;
    }
    if jdk_only_dumps_written() {
        return;
    }

    // `--explain-jdk-only` is the one flag that turns redaction OFF. Every
    // writer below takes it explicitly and redacts when it is false, which is
    // the default; there is no path that reaches a writer without deciding.
    let verbose = args.explain_jdk_only;
    let mode = shared.config.compatibility_mode;

    // Each writer does its own locking and its own row snapshotting, inside
    // `vm`, where the registry and the class manager live. The launcher used to
    // copy the rows out here into local mirror types; that mirror is what
    // allowed the two implementations to drift, and copying twice bought
    // nothing — the writers already release every borrow before touching the
    // filesystem, and the report takes ONE class-manager acquisition for both
    // the census and the violation list so the two describe the same instant.
    if let Some(path) = &args.dump_class_origins {
        match shared.dump_class_origins_json(path, verbose) {
            Ok(n) => eprintln!(
                "[cratonvm] wrote {n} class-origin rows to {}",
                absolute_dump_path(path)
            ),
            Err(e) => eprintln!(
                "[cratonvm] warning: could not write class-origin census: {}",
                describe_dump_failure(path, &e)
            ),
        }
    }

    if let Some(path) = &args.dump_native_registry {
        match shared.dump_native_census_json(path, verbose) {
            // `schema 4`, matching the `"schema_version": 4` the writer in
            // `cratonvm_vm::vm::vm_init` actually emits and the number
            // `--help` documents. This line said `schema 3` until 2026-08-17,
            // which is a bad way for the instrument whose job is to be
            // believed to introduce itself. If the writer's version moves
            // again, this literal is the second place to change.
            Ok((intrinsic, bridge, stub)) => eprintln!(
                "[cratonvm] wrote native registry census (schema 4{}) to {} \
                 (intrinsic={intrinsic}, bridge={bridge}, synthetic-stub={stub}){}",
                if verbose {
                    ", image-adjudicated"
                } else {
                    ", no image adjudication — pass --explain-real-jdk"
                },
                absolute_dump_path(path),
                census_invocations_caveat(
                    cratonvm_types::flags::runtime_var("CRATONVM_DISABLE_JIT")
                        .is_ok_and(|v| !v.is_empty() && v != "0"),
                    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                        .is_ok_and(|v| !v.is_empty() && v != "0"),
                ),
            ),
            Err(e) => eprintln!(
                "[cratonvm] warning: could not write native registry JSON: {}",
                describe_dump_failure(path, &e)
            ),
        }
    }

    if let Some(path) = &args.jdk_only_report {
        match shared.dump_jdk_only_report_json(path, verbose) {
            // `violations` is now the union of all five recording sites, so
            // this number moved: it used to count refused registrations plus
            // compatibility-class requests only. The refusal-event total is
            // reported alongside it rather than folded in — they are different
            // units (distinct methods versus events) and adding them would
            // produce a number that means nothing.
            Ok((violations, compatibility_classes)) => {
                eprintln!(
                    "[cratonvm] wrote {} Real-JDK report to {} ({violations} violation(s), \
                     {compatibility_classes} compatibility class(es), {} refusal event(s))",
                    mode.as_str(),
                    absolute_dump_path(path),
                    shared.jdk_only_refusal_counts().total(),
                );
                // The line above is the one number most readers stop at, and
                // `{violations}` is a floor whenever the observation sink filled
                // up. Saying so HERE, and not only in the file's
                // `observation_sink` object, is the difference between a caveat a
                // reader has to go looking for and one they cannot miss: the
                // whole failure mode is that a truncated list reads as a
                // complete one.
                // t1 wave 4 (lane Y2): this VM's resolver sink.
                let sink = &shared.natives.native_shadow_sink;
                if cratonvm_vm::vm::jdk_only_native_shadow_sink_saturated(sink) {
                    // The drop count is what makes this line actionable rather
                    // than merely alarming: it says HOW SHORT the list is, and
                    // it names the knob that fixes it. Before 2026-08-20 the
                    // only advice available here was "narrow the workload",
                    // which is the opposite of what a census run wants and is
                    // how every shadow figure in docs/known-issues/jdk-only/
                    // came to be a floor.
                    let dropped = cratonvm_vm::vm::jdk_only_native_shadow_sink_dropped(sink);
                    let recorded = cratonvm_vm::vm::jdk_only_native_shadow_sink_len(sink);
                    eprintln!(
                        "[cratonvm] warning: the Real-JDK observation sink SATURATED at {} \
                         distinct rows and dropped ~{dropped} more — violations[] names \
                         {recorded} of roughly {} shadows, so the rows are a FLOOR and the \
                         TOTAL is the sum. Re-run with \
                         CRATONVM_NATIVE_SHADOW_SINK_CAP={} to name all of them; do NOT \
                         narrow the workload, that is what censors the census.",
                        cratonvm_vm::vm::jdk_only_native_shadow_cap(),
                        recorded as u64 + dropped,
                        // CLAMPED to the sinks' own ceiling (WORKER-5 NOTE-6
                        // N3). Unclamped this advised `(recorded+dropped)*2`,
                        // which exceeds 65,536 for any workload with more than
                        // ~32,768 shadows — so the VM could print a value it
                        // then adjusts. Since 2026-08-22 an over-ceiling value
                        // clamps LOUDLY rather than silently reverting to the
                        // default, but advising a number the run will change is
                        // still worse than advising the right one.
                        (recorded as u64 + dropped)
                            .saturating_mul(2)
                            .max(8192)
                            .min(65_536),
                    );
                }
                // The JIT fast-path sink is a SECOND bounded collection feeding
                // the same `violations[]`, and until 2026-08-20 it had no
                // saturation signal anywhere — not in the file and not on this
                // line. A reader who saw no warning concluded the list was
                // complete; it was complete for one source of three.
                if cratonvm_vm::jit::helpers::jdk_only_jit_helper_sink_saturated(shared) {
                    eprintln!(
                        "[cratonvm] warning: the Real-JDK JIT fast-path violation sink \
                         SATURATED at {} rows and dropped {} refusal(s) it could not name — \
                         observation_sink.jit_fastpath in the report carries the same two \
                         numbers. Same remedy: raise CRATONVM_NATIVE_SHADOW_SINK_CAP.",
                        cratonvm_vm::jit::helpers::jdk_only_jit_helper_violation_cap(),
                        cratonvm_vm::jit::helpers::jdk_only_jit_helper_sink_dropped(shared),
                    );
                }
                // The THIRD bounded collection. It had no counter at all until
                // 2026-08-22, so it could not warn here and rendered `null` in
                // the file — which `run.sh` read as "nothing truncated", on
                // every strict run there has ever been. All three now answer
                // the same two questions.
                // t1 wave 4 (lane V3): THIS VM's compile sink plus the no-VM
                // remainder, as `observation_sink.jit_compile` composes it
                // (`compile_dropped_for_report`). Since t1 wave 3 (lane Y) a
                // VM's compiles record into its own sinks, so the process-only
                // `cratonvm_jit::jdk_only_jit_sink_dropped()` this line read
                // stayed 0 and the warning never fired for a saturated VM.
                let compile_dropped =
                    shared.jit.jit_cache.jdk_only_sinks().compile_dropped_for_report();
                if compile_dropped > 0 {
                    eprintln!(
                        "[cratonvm] warning: the Real-JDK JIT COMPILE-TIME violation sink \
                         SATURATED at {} rows and dropped {} refusal(s) it could not name — \
                         observation_sink.jit_compile in the report carries the same two \
                         numbers. Same remedy: raise CRATONVM_NATIVE_SHADOW_SINK_CAP.",
                        cratonvm_jit::jdk_only_violation_cap(),
                        compile_dropped,
                    );
                }
            }
            Err(e) => eprintln!(
                "[cratonvm] warning: could not write Real-JDK report: {}",
                describe_dump_failure(path, &e)
            ),
        }
    }
}

/// Claim the right to write the census artefacts; `true` means someone already
/// has.
///
/// Shared by `write_jdk_only_dumps` (the four unwinding exit paths) and
/// `write_jdk_only_dumps_on_exit` (the `System.exit` path), so the two cannot
/// interleave two writes to the same file. First caller wins on purpose — on a
/// failing run that is the failure path, which is the informative one.
pub(crate) fn jdk_only_dumps_written() -> bool {
    static WRITTEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    WRITTEN.swap(true, std::sync::atomic::Ordering::AcqRel)
}

/// The `--dump-*` / `--real-jdk-report` paths, readable from the pre-exit hook.
///
/// The hook is a bare `fn(i32)` installed at the top of `run()`, before `Args`
/// is parsed, so it cannot capture them. This cell is published as soon as they
/// are known and read back on the `System.exit` path.
///
/// **This is launcher state, not VM state.** Contract §2's ban on process
/// globals is about the feature's *per-VM* state — a policy or a violation sink
/// that two VMs in one process could confuse. These are the command line of the
/// one process that parsed them, and they sit alongside the equally
/// process-global `PRE_EXIT_HOOK` and `write_jdk_only_dumps`' `WRITTEN` latch
/// that they exist to cooperate with.
pub(crate) static JDK_ONLY_EXIT_DUMP_PATHS: std::sync::OnceLock<JdkOnlyExitDumpPaths> =
    std::sync::OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct JdkOnlyExitDumpPaths {
    pub(crate) class_origins: Option<String>,
    pub(crate) native_registry: Option<String>,
    pub(crate) report: Option<String>,
    pub(crate) verbose: bool,
}

/// Write the census from the `System.exit` / `Runtime.exit` path.
///
/// `System.exit(N)` never unwinds, so none of `finish_jdk_only`'s four callers
/// runs. Without this a `--real-jdk --real-jdk-report r.json` run of a program
/// whose error handler exits produced no report at all — and those are the runs
/// the report exists for. See
/// `jdk-only-system-exit-census-FIXED-20260804.md`.
///
/// Shares `write_jdk_only_dumps`' `WRITTEN` latch, so a `System.exit` racing a
/// normal shutdown cannot produce two interleaved writes to the same path;
/// first caller wins, and on this path the first caller is the informative one.
///
/// Never blocks: the VM-side writer uses a non-blocking class-manager
/// acquisition and falls back to labelled partial artefacts. See
/// `SharedVm::try_write_jdk_only_dumps_for_exit` for why a timeout-and-retry
/// would be the wrong shape here.
pub(crate) fn write_jdk_only_dumps_on_exit() {
    let Some(paths) = JDK_ONLY_EXIT_DUMP_PATHS.get() else {
        return;
    };
    if paths.class_origins.is_none() && paths.native_registry.is_none() && paths.report.is_none() {
        return;
    }
    if jdk_only_dumps_written() {
        return;
    }
    let Some(shared) = cratonvm_vm::native::jni::process_vm() else {
        return;
    };
    let (wrote, partial) = shared.try_write_jdk_only_dumps_for_exit(
        paths.class_origins.as_deref(),
        paths.native_registry.as_deref(),
        paths.report.as_deref(),
        paths.verbose,
    );
    if wrote {
        if partial {
            eprintln!(
                "[cratonvm] wrote PARTIAL Real-JDK census on System.exit: the class-manager \
                 lock was held, so class-origin rows are absent. Each file records this in its \
                 \"partial_reason\"."
            );
        } else {
            eprintln!("[cratonvm] wrote Real-JDK census on System.exit");
        }
    }
}

/// `-XX:StartFlightRecording`'s `dumponexit` (default true): write the boot
/// recording to its file, once per process.
///
/// Called from every exit arm that runs shutdown hooks, and from `Runtime.halt`
/// as before: the pre-exit hook (`System.exit`, `Runtime.exit`, `Runtime.halt`)
/// and `run()` after the shutdown hooks (`main` returned, uncaught exception).
///
/// gc-common w11-f: before this, only the pre-exit hook dumped, so a program
/// that returned from `main` or died on an uncaught exception, which are the
/// two most common ways a Java program ends, wrote no recording even though
/// `dumponexit=true` was in force. HotSpot writes it on both (its dump is a
/// shutdown hook, and HotSpot runs hooks on both arms).
///
/// The latch keeps a `System.exit` called from a shutdown hook from writing the
/// file twice. It is launcher state, one per process, like
/// [`GC_SUMMARY_PRINTED`].
pub(crate) fn dump_jfr_recording_on_exit(shared: &cratonvm_vm::SharedVm) {
    static DUMPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DUMPED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    match write_jfr_exit_recording(shared) {
        Some(Ok((bytes, filename))) => {
            tracing::info!("wrote {bytes} byte JFR recording to {filename}")
        }
        Some(Err(e)) => tracing::error!("failed to dump JFR recording on exit: {e}"),
        None => {}
    }
}

/// The unlatched body of [`dump_jfr_recording_on_exit`]: `None` when no exit
/// dump was requested, otherwise the byte count and file, or the error text.
pub(crate) fn write_jfr_exit_recording(
    shared: &cratonvm_vm::SharedVm,
) -> Option<Result<(u64, String), String>> {
    let (recording_id, filename) = shared.debug.jfr_dump_on_exit.lock().clone()?;
    let mut fr = shared.debug.flight_recorder.lock();
    let dumped = fr.dump_recording(recording_id, std::path::Path::new(&filename));
    Some(match dumped {
        Ok(bytes) => Ok((bytes, filename)),
        Err(e) => Err(e.to_string()),
    })
}

