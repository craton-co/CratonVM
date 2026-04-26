use rustjvm_types::error::ClassFileError;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use tracing::debug;
use zip::ZipArchive;

/// Represents the classpath used to find `.class` files.
///
/// Supports directory entries, flat JAR entries, and Spring Boot fat JAR
/// entries (nested JARs inside `BOOT-INF/lib/` and classes from
/// `BOOT-INF/classes/`).
pub struct ClassPath {
    entries: Vec<ClassPathEntry>,
}

enum ClassPathEntry {
    Directory(PathBuf),
    /// A JAR file read into memory. The `Mutex` provides interior mutability
    /// since `ZipArchive::by_name` requires `&mut self`, and future-proofs
    /// for multi-threaded access (Phase 5).
    JarFile {
        path: PathBuf,
        archive: Mutex<ZipArchive<Cursor<Vec<u8>>>>,
        /// True if the JAR declares `Multi-Release: true` in its manifest (JEP 238).
        multi_release: bool,
    },
    /// A virtual directory inside a fat JAR (e.g. `BOOT-INF/classes/`).
    /// Entries are stored as a map from relative path to byte content.
    NestedDirectory {
        /// The outer JAR path (for debug/logging).
        parent_jar: PathBuf,
        /// Prefix inside the outer JAR (e.g. `BOOT-INF/classes/`).
        prefix: String,
        /// Cached entries: relative_path (without prefix) → bytes.
        entries_cache: HashMap<String, Vec<u8>>,
    },
    /// A nested JAR extracted from inside a fat JAR (e.g. `BOOT-INF/lib/dep.jar`).
    NestedJar {
        /// The outer JAR path (for debug/logging).
        parent_jar: PathBuf,
        /// Path inside the outer JAR (e.g. `BOOT-INF/lib/spring-core-6.1.0.jar`).
        nested_path: String,
        /// The extracted nested archive.
        archive: Mutex<ZipArchive<Cursor<Vec<u8>>>>,
    },
    /// A JDK 9+ JMOD file (ZIP with 4-byte `JM\x01\x00` prefix).
    /// All entries under `classes/` are pre-extracted into an in-memory cache
    /// at load time so that subsequent lookups are O(1) HashMap gets without
    /// any ZIP decompression.  This is critical for debug-mode performance
    /// where deflate is extremely slow without compiler optimisations.
    JmodFile {
        path: PathBuf,
        /// Pre-extracted class entries: relative path (e.g. `java/lang/Object.class`)
        /// mapped to decompressed bytes.  Built once during `load_jmod`.
        classes_cache: HashMap<String, Vec<u8>>,
        /// The full set of entry names in the JMOD (including non-class entries)
        /// kept for `list_jmod_classes` and resource lookups.
        all_entry_names: Vec<String>,
        /// Lazily-opened archive for non-class resource lookups (rare path).
        archive: Mutex<ZipArchive<Cursor<Vec<u8>>>>,
    },
    /// A JDK 9+ `lib/modules` jimage file (NEW-5): one binary blob holding
    /// every class and resource in the boot layer, accessed through the
    /// [`rustjvm_reader::JImageReader`] perfect-hash index.
    ///
    /// The lookup strategy:
    ///   1. `class_to_module`: pre-built map from internal class name
    ///      (e.g. `java/lang/String`) to its owning module (e.g. `java.base`).
    ///      Populated once during load by iterating the jimage entries.
    ///   2. `find_class` consults this map and then calls
    ///      `JImageReader::find_class(module, name)` to fetch bytes via the
    ///      perfect-hash path — O(1) amortized.
    ///   3. `find_resource` tries every known module for a matching resource,
    ///      stopping on the first hit (matches JDK `ModuleReader` semantics).
    JImageFile {
        path: PathBuf,
        reader: rustjvm_reader::JImageReader,
        /// Map from internal class name (no `.class` suffix) to its owning
        /// module name. Built once during load by walking every entry; used
        /// by `find_class` for O(1) module resolution.
        class_to_module: HashMap<String, String>,
        /// Map from resource path (with `.class` suffix for classes, or the
        /// raw resource path for non-class files) to the list of modules
        /// that contain it. Only non-class resources appear here — classes
        /// use `class_to_module` instead.
        resource_to_modules: HashMap<String, Vec<String>>,
        /// Every module name that contributes at least one entry. Used by
        /// `scan_module_infos` and by the module-layer builder.
        module_names: Vec<String>,
    },
}

impl std::fmt::Debug for ClassPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let paths: Vec<String> = self
            .entries
            .iter()
            .map(|e| match e {
                ClassPathEntry::Directory(p) => format!("dir:{}", p.display()),
                ClassPathEntry::JarFile { path, .. } => format!("jar:{}", path.display()),
                ClassPathEntry::NestedDirectory {
                    parent_jar, prefix, ..
                } => format!("nested-dir:{}!/{}", parent_jar.display(), prefix),
                ClassPathEntry::NestedJar {
                    parent_jar,
                    nested_path,
                    ..
                } => format!("nested-jar:{}!/{}", parent_jar.display(), nested_path),
                ClassPathEntry::JmodFile { path, .. } => format!("jmod:{}", path.display()),
                ClassPathEntry::JImageFile { path, .. } => {
                    format!("jimage:{}", path.display())
                }
            })
            .collect();
        f.debug_struct("ClassPath")
            .field("entries", &paths)
            .finish()
    }
}

impl std::fmt::Debug for ClassPathEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClassPathEntry::Directory(p) => write!(f, "Directory({})", p.display()),
            ClassPathEntry::JarFile { path, .. } => write!(f, "JarFile({})", path.display()),
            ClassPathEntry::NestedDirectory {
                parent_jar, prefix, ..
            } => write!(
                f,
                "NestedDirectory({}!/{})",
                parent_jar.display(),
                prefix
            ),
            ClassPathEntry::NestedJar {
                parent_jar,
                nested_path,
                ..
            } => write!(
                f,
                "NestedJar({}!/{})",
                parent_jar.display(),
                nested_path
            ),
            ClassPathEntry::JmodFile { path, .. } => write!(f, "JmodFile({})", path.display()),
            ClassPathEntry::JImageFile { path, .. } => {
                write!(f, "JImageFile({})", path.display())
            }
        }
    }
}

/// Parsed contents of `META-INF/MANIFEST.MF` relevant to JAR loading.
#[derive(Debug, Default)]
pub struct ManifestInfo {
    /// `Main-Class` header (the launcher class).
    pub main_class: Option<String>,
    /// `Start-Class` header (Spring Boot: the actual application class).
    pub start_class: Option<String>,
    /// `Spring-Boot-Classes` header (default: `BOOT-INF/classes/`).
    pub boot_classes: Option<String>,
    /// `Spring-Boot-Lib` header (default: `BOOT-INF/lib/`).
    pub boot_lib: Option<String>,
    /// `Class-Path` header (space-separated relative paths to additional JARs).
    pub class_path: Option<String>,
    /// `Multi-Release: true` header (JEP 238, Java 9+).
    pub multi_release: bool,
    /// T19.H10: full attribute map from the manifest's main section.
    /// Keys are stored verbatim (case-preserving) so callers like
    /// `Class.getPackage().getImplementationVersion()` can pull arbitrary
    /// `Implementation-*` / `Specification-*` headers without losing
    /// whitespace or casing. Only the main section is captured —
    /// per-entry sections (rare; used by signed JARs) are not parsed.
    pub attributes: std::collections::HashMap<String, String>,
}

impl ManifestInfo {
    /// Parse a `MANIFEST.MF` file's bytes.
    pub fn parse(data: &[u8]) -> Self {
        let text = String::from_utf8_lossy(data);
        let mut info = ManifestInfo::default();
        // MANIFEST.MF uses continuation lines (start with single space),
        // so we first join them.
        let joined = text.replace("\r\n ", "").replace("\n ", "");
        // Per the JAR spec the file is split into sections by blank lines;
        // only the *main* section (the leading section before the first
        // blank line) carries the manifest-wide attributes that
        // `Package.getImplementationVersion()` etc. consult. Per-entry
        // sections after the first blank are skipped.
        for line in joined.lines() {
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim();
                // Cap each attribute to 8 KiB to keep a malicious manifest
                // (large nameplate, suspicious magic strings) from blowing
                // out memory.  Real attributes max out at a few hundred
                // bytes; the few that legitimately hold a base64 blob
                // (signing certs) sit on per-entry sections, which we do
                // not parse.
                if value.len() > 8 * 1024 {
                    continue;
                }
                info.attributes.insert(key.to_string(), value.to_string());
                match key {
                    "Main-Class" => info.main_class = Some(value.to_string()),
                    "Start-Class" => info.start_class = Some(value.to_string()),
                    "Spring-Boot-Classes" => info.boot_classes = Some(value.to_string()),
                    "Spring-Boot-Lib" => info.boot_lib = Some(value.to_string()),
                    "Class-Path" => info.class_path = Some(value.to_string()),
                    "Multi-Release" => info.multi_release = value.eq_ignore_ascii_case("true"),
                    _ => {}
                }
            }
        }
        info
    }

    /// Resolve `Class-Path` manifest entries relative to a JAR file's parent directory.
    ///
    /// Per the JAR specification, `Class-Path` entries are space-separated URIs
    /// resolved relative to the JAR's location (not the current working directory).
    pub fn resolve_class_path(&self, jar_path: &Path) -> Vec<String> {
        let jar_dir = jar_path.parent().unwrap_or(Path::new("."));
        match &self.class_path {
            Some(cp) => cp
                .split_whitespace()
                .map(|entry| jar_dir.join(entry).to_string_lossy().into_owned())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Returns true if this JAR appears to be a Spring Boot fat JAR.
    pub fn is_spring_boot(&self) -> bool {
        self.start_class.is_some()
            || self.boot_classes.is_some()
            || self.boot_lib.is_some()
    }
}

/// Maximum Java version supported by this VM. Multi-release JARs check
/// `META-INF/versions/{N}/` entries descending from this version down to 9.
const MULTI_RELEASE_MAX_VERSION: u32 = 25;

impl ClassPath {
    /// Look up an entry in a multi-release JAR archive.
    ///
    /// Checks `META-INF/versions/{N}/{name}` for N descending from
    /// `MULTI_RELEASE_MAX_VERSION` down to 9, returning the first match.
    /// Falls back to the base entry if no versioned entry is found.
    fn find_in_multi_release_archive(
        archive: &Mutex<ZipArchive<Cursor<Vec<u8>>>>,
        name: &str,
    ) -> Option<Vec<u8>> {
        for ver in (9..=MULTI_RELEASE_MAX_VERSION).rev() {
            let versioned = format!("META-INF/versions/{ver}/{name}");
            if let Some(data) = Self::find_in_archive(archive, &versioned) {
                return Some(data);
            }
        }
        // Fall back to base entry.
        Self::find_in_archive(archive, name)
    }

    /// Create a classpath from a list of path strings.
    ///
    /// Each entry can be a directory or a `.jar` file. Non-existent paths and
    /// invalid JAR files are silently skipped with a debug log message.
    ///
    /// JAR files are automatically scanned for Spring Boot fat JAR structure:
    /// if `BOOT-INF/classes/` or `BOOT-INF/lib/` are detected (via MANIFEST.MF
    /// or directory probing), nested entries are extracted and added to the
    /// classpath.
    pub fn new(paths: &[String]) -> Self {
        let mut entries = Vec::new();
        for p in paths {
            let path = PathBuf::from(p);
            if path.is_dir() {
                entries.push(ClassPathEntry::Directory(path));
            } else if path.extension().is_some_and(|ext| ext == "jar") && path.exists() {
                match fs::read(&path) {
                    Ok(data) => {
                        Self::load_jar_data(&path, data, &mut entries);
                    }
                    Err(e) => {
                        debug!("Failed to read JAR {}: {e}", path.display());
                    }
                }
            } else if path.extension().is_some_and(|ext| ext == "jmod") && path.exists() {
                match Self::load_jmod(&path) {
                    Ok(entry) => entries.push(entry),
                    Err(e) => debug!("Failed to read JMOD {}: {e}", path.display()),
                }
            } else if Self::is_likely_jimage(&path) {
                // NEW-5: the JDK 9+ runtime image at `$JAVA_HOME/lib/modules`
                // is a single jimage blob. Detection is by file name
                // (`modules` under any directory) plus a magic-number
                // verification inside `load_jimage`. The explicit file
                // name check lets users write `-cp /path/to/lib/modules`
                // without having to pass a special flag.
                match Self::load_jimage(&path) {
                    Ok(entry) => entries.push(entry),
                    Err(e) => debug!("Failed to read jimage {}: {e}", path.display()),
                }
            } else {
                debug!("Skipping non-existent classpath entry: {p}");
            }
        }
        Self { entries }
    }

    /// Heuristic: does `path` look like a JDK 9+ jimage file?
    ///
    /// The standard location is `$JAVA_HOME/lib/modules` — a file named
    /// `modules` (no extension) under a `lib` directory. We accept any
    /// file named `modules` that exists and is larger than the jimage
    /// header; the real magic-number check happens inside
    /// [`JImageReader::open`] so callers still get a clean error if the
    /// file is not actually a jimage.
    fn is_likely_jimage(path: &Path) -> bool {
        if !path.exists() {
            return false;
        }
        if path.extension().is_some() {
            return false; // anything with an extension is not `modules`
        }
        match path.file_name().and_then(|n| n.to_str()) {
            Some("modules") => {}
            _ => return false,
        }
        match fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.len() >= 28, // header size
            Err(_) => false,
        }
    }

    /// Load a JAR from raw bytes, auto-detecting fat JAR structure.
    fn load_jar_data(path: &Path, data: Vec<u8>, entries: &mut Vec<ClassPathEntry>) {
        let cursor = Cursor::new(data);
        match ZipArchive::new(cursor) {
            Ok(mut archive) => {
                // Check for MANIFEST.MF to detect Spring Boot fat JAR
                let manifest = Self::read_manifest(&mut archive);
                let is_fat_jar = manifest.is_spring_boot()
                    || Self::probe_fat_jar_structure(&mut archive);

                if is_fat_jar {
                    debug!(
                        "Detected fat JAR: {} (Start-Class: {:?})",
                        path.display(),
                        manifest.start_class
                    );
                    Self::extract_fat_jar_entries(path, &mut archive, &manifest, entries);
                    // Also add the outer JAR itself (for classes at the root,
                    // e.g. the Spring Boot launcher classes in org/springframework/boot/loader/).
                    let mr = manifest.multi_release;
                    let data = archive.into_inner().into_inner();
                    let reloaded = ZipArchive::new(Cursor::new(data)).unwrap();
                    entries.push(ClassPathEntry::JarFile {
                        path: path.to_path_buf(),
                        archive: Mutex::new(reloaded),
                        multi_release: mr,
                    });
                } else {
                    let mr = manifest.multi_release;
                    debug!("Loaded JAR: {}", path.display());
                    entries.push(ClassPathEntry::JarFile {
                        path: path.to_path_buf(),
                        archive: Mutex::new(archive),
                        multi_release: mr,
                    });
                }
            }
            Err(e) => {
                debug!("Failed to open JAR {}: {e}", path.display());
            }
        }
    }

    /// Read `META-INF/MANIFEST.MF` from an archive.
    fn read_manifest(archive: &mut ZipArchive<Cursor<Vec<u8>>>) -> ManifestInfo {
        let result = archive
            .by_name("META-INF/MANIFEST.MF")
            .and_then(|mut entry| {
                let mut data = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut data)?;
                Ok(data)
            });
        match result {
            Ok(data) => ManifestInfo::parse(&data),
            Err(_) => ManifestInfo::default(),
        }
    }

    /// Probe for fat JAR structure by checking if any entry starts with
    /// `BOOT-INF/` or `WEB-INF/classes/`.
    fn probe_fat_jar_structure(archive: &mut ZipArchive<Cursor<Vec<u8>>>) -> bool {
        for i in 0..archive.len().min(100) {
            if let Ok(entry) = archive.by_index_raw(i) {
                let name = entry.name().to_string();
                if name.starts_with("BOOT-INF/") || name.starts_with("WEB-INF/classes/") {
                    return true;
                }
            }
        }
        false
    }

    /// Extract nested entries from a Spring Boot fat JAR.
    ///
    /// 1. All `.class` files under `BOOT-INF/classes/` (or custom path from
    ///    MANIFEST) become a `NestedDirectory` entry.
    /// 2. Each `.jar` file under `BOOT-INF/lib/` (or custom path) is extracted
    ///    into memory and added as a `NestedJar` entry.
    fn extract_fat_jar_entries(
        path: &Path,
        archive: &mut ZipArchive<Cursor<Vec<u8>>>,
        manifest: &ManifestInfo,
        entries: &mut Vec<ClassPathEntry>,
    ) {
        let classes_prefix = manifest
            .boot_classes
            .as_deref()
            .unwrap_or("BOOT-INF/classes/");
        let lib_prefix = manifest
            .boot_lib
            .as_deref()
            .unwrap_or("BOOT-INF/lib/");

        // Ensure prefixes end with /
        let classes_prefix = if classes_prefix.ends_with('/') {
            classes_prefix.to_string()
        } else {
            format!("{classes_prefix}/")
        };
        let lib_prefix = if lib_prefix.ends_with('/') {
            lib_prefix.to_string()
        } else {
            format!("{lib_prefix}/")
        };

        // Also handle WEB-INF for WAR files
        let war_classes_prefix = "WEB-INF/classes/";

        // Phase 1: Collect BOOT-INF/classes/ entries into a NestedDirectory
        let mut classes_cache: HashMap<String, Vec<u8>> = HashMap::new();
        let mut nested_jar_names: Vec<String> = Vec::new();

        for i in 0..archive.len() {
            let name = match archive.by_index_raw(i) {
                Ok(entry) => entry.name().to_string(),
                Err(_) => continue,
            };

            if name.starts_with(&classes_prefix) && name.len() > classes_prefix.len() {
                let relative = &name[classes_prefix.len()..];
                if !relative.is_empty() && !relative.ends_with('/') {
                    if let Ok(mut entry) = archive.by_name(&name) {
                        let mut data = Vec::with_capacity(entry.size() as usize);
                        if entry.read_to_end(&mut data).is_ok() {
                            classes_cache.insert(relative.to_string(), data);
                        }
                    }
                }
            } else if name.starts_with(war_classes_prefix) && name.len() > war_classes_prefix.len()
            {
                let relative = &name[war_classes_prefix.len()..];
                if !relative.is_empty() && !relative.ends_with('/') {
                    if let Ok(mut entry) = archive.by_name(&name) {
                        let mut data = Vec::with_capacity(entry.size() as usize);
                        if entry.read_to_end(&mut data).is_ok() {
                            classes_cache.insert(relative.to_string(), data);
                        }
                    }
                }
            } else if name.starts_with(&lib_prefix)
                && name.ends_with(".jar")
                && name.len() > lib_prefix.len()
            {
                nested_jar_names.push(name);
            }
        }

        if !classes_cache.is_empty() {
            debug!(
                "Fat JAR {}: extracted {} files from {}",
                path.display(),
                classes_cache.len(),
                classes_prefix
            );
            entries.push(ClassPathEntry::NestedDirectory {
                parent_jar: path.to_path_buf(),
                prefix: classes_prefix.to_string(),
                entries_cache: classes_cache,
            });
        }

        // Phase 2: Extract each nested JAR from BOOT-INF/lib/
        let mut nested_count = 0;
        for jar_name in &nested_jar_names {
            if let Ok(mut entry) = archive.by_name(jar_name) {
                let mut jar_data = Vec::with_capacity(entry.size() as usize);
                if entry.read_to_end(&mut jar_data).is_ok() {
                    let cursor = Cursor::new(jar_data);
                    match ZipArchive::new(cursor) {
                        Ok(nested_archive) => {
                            entries.push(ClassPathEntry::NestedJar {
                                parent_jar: path.to_path_buf(),
                                nested_path: jar_name.clone(),
                                archive: Mutex::new(nested_archive),
                            });
                            nested_count += 1;
                        }
                        Err(e) => {
                            debug!(
                                "Fat JAR {}: failed to open nested JAR {}: {e}",
                                path.display(),
                                jar_name
                            );
                        }
                    }
                }
            }
        }

        if nested_count > 0 {
            debug!(
                "Fat JAR {}: loaded {} nested JARs from {}",
                path.display(),
                nested_count,
                lib_prefix
            );
        }
    }

    /// Returns true if this classpath has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of classpath entries.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Read the MANIFEST.MF from the first JAR on this classpath that has one.
    ///
    /// Useful for the CLI to determine `Start-Class` from a Spring Boot fat JAR.
    /// Read the manifest from a single JAR file without constructing a full ClassPath.
    ///
    /// Returns `None` if the JAR cannot be read or has no `MANIFEST.MF`.
    pub fn read_jar_manifest(jar_path: &Path) -> Option<ManifestInfo> {
        let data = fs::read(jar_path).ok()?;
        let cursor = Cursor::new(data);
        let mut archive = ZipArchive::new(cursor).ok()?;
        let info = Self::read_manifest(&mut archive);
        Some(info)
    }

    pub fn read_first_manifest(&self) -> Option<ManifestInfo> {
        for entry in &self.entries {
            if let ClassPathEntry::JarFile { archive, .. } = entry {
                let mut guard = archive.lock();
                let info = Self::read_manifest(&mut guard);
                if info.main_class.is_some() || info.start_class.is_some() {
                    return Some(info);
                }
            }
        }
        None
    }

    /// Add a new path entry at runtime (for URLClassLoader and dynamic class loading).
    ///
    /// If `path` is a directory it is added as a `Directory` entry; if it ends
    /// with `.jar` (or `.zip`) and exists, it is opened as a `JarFile` entry.
    /// Fat JARs are auto-detected and their nested entries are extracted.
    /// Silently skips non-existent paths or unreadable JARs.
    pub fn add_path(&mut self, path: &str) {
        let pb = std::path::PathBuf::from(path);
        if pb.is_dir() {
            debug!("Dynamic classpath: adding directory {path}");
            self.entries.push(ClassPathEntry::Directory(pb));
        } else if pb.extension().is_some_and(|e| e == "jar" || e == "zip") && pb.exists() {
            match std::fs::read(&pb) {
                Ok(data) => {
                    Self::load_jar_data(&pb, data, &mut self.entries);
                }
                Err(e) => {
                    debug!("Dynamic classpath: failed to read {path}: {e}");
                }
            }
        } else if pb.extension().is_some_and(|e| e == "jmod") && pb.exists() {
            match Self::load_jmod(&pb) {
                Ok(entry) => self.entries.push(entry),
                Err(e) => debug!("Dynamic classpath: failed to read JMOD {path}: {e}"),
            }
        } else {
            debug!("Dynamic classpath: skipping non-existent entry {path}");
        }
    }

    /// Find and read a class file by its binary name (e.g., `java/lang/Object`).
    ///
    /// Class names are validated to prevent path traversal attacks. Names containing
    /// `..` or starting with `/` are rejected.
    pub fn find_class(&self, class_name: &str) -> Result<Vec<u8>, ClassFileError> {
        // Reject path traversal attempts, absolute paths, and suspicious patterns.
        // Class binary names use '/' as separator and must not escape the classpath root.
        if class_name.contains("..")
            || class_name.starts_with('/')
            || class_name.starts_with('\\')
            || class_name.contains("\\\\")
            || class_name.contains('\0')
            || class_name.contains(':')       // Windows drive letters (C:)
            || class_name.contains("./")      // current-dir references
            || class_name.contains(".\\")     // Windows current-dir references
        {
            return Err(ClassFileError::ClassNotFound {
                class_name: class_name.to_string(),
            });
        }

        let relative_path = format!("{}.class", class_name);

        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    let full_path = dir.join(Path::new(&relative_path));
                    if full_path.exists() {
                        // Canonicalize and verify the resolved path is under the classpath root.
                        // This catches symlink-based traversal that string checks miss.
                        if let (Ok(canon_dir), Ok(canon_path)) =
                            (fs::canonicalize(dir), fs::canonicalize(&full_path))
                        {
                            if !canon_path.starts_with(&canon_dir) {
                                debug!(
                                    "Path traversal blocked: {} escapes {}",
                                    canon_path.display(),
                                    canon_dir.display()
                                );
                                return Err(ClassFileError::ClassNotFound {
                                    class_name: class_name.to_string(),
                                });
                            }
                        }
                        debug!("Found class {class_name} at {}", full_path.display());
                        return fs::read(&full_path).map_err(|e| ClassFileError::IoError {
                            class_name: class_name.to_string(),
                            source: e,
                        });
                    }
                }
                ClassPathEntry::JarFile { archive, multi_release, .. } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(archive, &relative_path)
                    } else {
                        Self::find_in_archive(archive, &relative_path)
                    };
                    if let Some(data) = found {
                        debug!("Found class {class_name} in JAR");
                        return Ok(data);
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if let Some(data) = entries_cache.get(&relative_path) {
                        debug!("Found class {class_name} in nested directory");
                        return Ok(data.clone());
                    }
                }
                ClassPathEntry::NestedJar { archive, nested_path, .. } => {
                    if let Some(data) = Self::find_in_archive(archive, &relative_path) {
                        debug!("Found class {class_name} in nested JAR {nested_path}");
                        return Ok(data);
                    }
                }
                ClassPathEntry::JmodFile { path, classes_cache, .. } => {
                    // Look up in the pre-extracted classes cache — O(1), no decompression
                    if let Some(data) = classes_cache.get(&relative_path) {
                        debug!("Found class {class_name} in JMOD {}", path.display());
                        return Ok(data.clone());
                    }
                }
                ClassPathEntry::JImageFile {
                    path,
                    reader,
                    class_to_module,
                    ..
                } => {
                    // Two paths:
                    //   1. Fast path — the class_to_module index was built
                    //      at load time, so one HashMap lookup tells us
                    //      exactly which module owns the class.
                    //   2. Fallback — if the class isn't in the index
                    //      (edge case: entry added after load, or a
                    //      synthesized class), iterate known modules.
                    if let Some(module) = class_to_module.get(class_name) {
                        match reader.find_class(module, class_name) {
                            Ok(Some(bytes)) => {
                                debug!(
                                    "Found class {class_name} in jimage {} (module {module})",
                                    path.display()
                                );
                                return Ok(bytes);
                            }
                            Ok(None) => {
                                // Index said module owns it, but the
                                // perfect-hash lookup disagrees — treat as
                                // a broken image and fall through so the
                                // caller sees a clean ClassNotFound.
                            }
                            Err(e) => {
                                return Err(ClassFileError::IoError {
                                    class_name: class_name.to_string(),
                                    source: std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        format!("jimage: {e}"),
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        }

        Err(ClassFileError::ClassNotFound {
            class_name: class_name.to_string(),
        })
    }

    /// Find the filesystem path of the classpath entry that holds the given
    /// class.  Returns `Some(path)` where `path` is a `file:`-style path to
    /// the containing JAR or directory — the same thing HotSpot returns in
    /// `ProtectionDomain.getCodeSource().getLocation().getPath()`.
    ///
    /// For a bare directory, returns the directory's absolute path with a
    /// trailing slash.  For JARs (flat or nested), returns the JAR file's
    /// absolute path (not wrapped in `jar:!/`).  For JMOD/jimage modules,
    /// returns `None` — JDK internals aren't user-visible code.
    pub fn find_class_source_path(&self, class_name: &str) -> Option<String> {
        if class_name.contains("..") || class_name.starts_with('/') || class_name.contains('\0') {
            return None;
        }
        let relative_path = format!("{}.class", class_name);
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    let full_path = dir.join(Path::new(&relative_path));
                    if full_path.exists() {
                        return Some(
                            dir.to_string_lossy().trim_end_matches(['/', '\\']).to_string() + "/",
                        );
                    }
                }
                ClassPathEntry::JarFile { archive, multi_release, path } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(archive, &relative_path).is_some()
                    } else {
                        Self::find_in_archive(archive, &relative_path).is_some()
                    };
                    if found {
                        return Some(path.to_string_lossy().into_owned());
                    }
                }
                ClassPathEntry::NestedDirectory {
                    parent_jar, entries_cache, ..
                } => {
                    if entries_cache.contains_key(&relative_path) {
                        return Some(parent_jar.to_string_lossy().into_owned());
                    }
                }
                ClassPathEntry::NestedJar {
                    parent_jar, archive, nested_path,
                } => {
                    if Self::find_in_archive(archive, &relative_path).is_some() {
                        // Return "$parent_jar!/$nested_path" style (JAR-in-JAR)
                        // so the caller can distinguish nested location
                        // layouts; Quarkus uses the outer jar path for its
                        // appRoot heuristic, so just return the outer JAR
                        // with the nested path appended after an exclamation
                        // (informational only — matches HotSpot's behaviour
                        // for fat JARs).
                        let outer = parent_jar.to_string_lossy();
                        return Some(format!("{outer}!/{nested_path}"));
                    }
                }
                ClassPathEntry::JmodFile { path, classes_cache, .. } => {
                    if classes_cache.contains_key(&relative_path) {
                        return Some(path.to_string_lossy().into_owned());
                    }
                }
                ClassPathEntry::JImageFile { .. } => {
                    // jimage contains modules — not user-visible code.
                }
            }
        }
        None
    }

    /// Find the code source for a given class: returns the containing JAR /
    /// directory URL and the set of signer "certificate" blobs extracted
    /// from `META-INF/*.RSA|.DSA|.EC` signature blocks (if any).
    ///
    /// The returned URL uses `file:` form for directories and JAR paths,
    /// matching HotSpot's `CodeSource.getLocation()`. The certificate
    /// vector holds one entry per PKCS#7 signature block found in the JAR.
    /// Because parsing PKCS#7 would pull in a full X.509/ASN.1 stack,
    /// we expose the *raw* signature-block bytes here — callers are
    /// expected to treat them as opaque identifiers (e.g. SHA-256 for
    /// signer matching). A real production implementation would decode
    /// the PKCS#7 and yield the embedded X.509 certificate chain.
    ///
    /// Returns `None` if the class isn't on this classpath or lives in a
    /// JMOD/jimage module (JDK internals have no user-visible code source).
    pub fn find_class_code_source_info(
        &self,
        class_name: &str,
    ) -> Option<(String, Vec<Vec<u8>>)> {
        if class_name.contains("..") || class_name.starts_with('/') || class_name.contains('\0') {
            return None;
        }
        let relative_path = format!("{}.class", class_name);
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    let full_path = dir.join(Path::new(&relative_path));
                    if full_path.exists() {
                        // Directories are never signed.
                        let abs = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
                        let p = abs.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p).to_string();
                        let p = p.trim_start_matches('/').trim_end_matches('/').to_string();
                        return Some((format!("file:/{p}/"), Vec::new()));
                    }
                }
                ClassPathEntry::JarFile { archive, multi_release, path } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(archive, &relative_path).is_some()
                    } else {
                        Self::find_in_archive(archive, &relative_path).is_some()
                    };
                    if found {
                        let abs = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                        let p = abs.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p).to_string();
                        let p = p.trim_start_matches('/').to_string();
                        let certs = Self::extract_jar_signer_blocks(archive);
                        return Some((format!("file:/{p}"), certs));
                    }
                }
                ClassPathEntry::NestedDirectory {
                    parent_jar, entries_cache, ..
                } => {
                    if entries_cache.contains_key(&relative_path) {
                        let p = parent_jar.to_string_lossy().replace('\\', "/");
                        let p = p.trim_start_matches('/').to_string();
                        return Some((format!("file:/{p}"), Vec::new()));
                    }
                }
                ClassPathEntry::NestedJar { parent_jar, archive, nested_path } => {
                    if Self::find_in_archive(archive, &relative_path).is_some() {
                        let outer = parent_jar.to_string_lossy().replace('\\', "/");
                        let outer = outer.trim_start_matches('/').to_string();
                        let certs = Self::extract_jar_signer_blocks(archive);
                        return Some((format!("jar:file:/{outer}!/{nested_path}"), certs));
                    }
                }
                ClassPathEntry::JmodFile { .. } | ClassPathEntry::JImageFile { .. } => {
                    // JDK internals — no user-visible code source.
                }
            }
        }
        None
    }

    /// Walk a JAR archive for every `META-INF/*.RSA`, `META-INF/*.DSA`,
    /// and `META-INF/*.EC` signature block.  Returns their raw contents
    /// (one `Vec<u8>` per signer).  An empty result means the JAR is
    /// unsigned.
    fn extract_jar_signer_blocks(
        archive: &Mutex<ZipArchive<Cursor<Vec<u8>>>>,
    ) -> Vec<Vec<u8>> {
        let mut guard = archive.lock();
        let mut out: Vec<Vec<u8>> = Vec::new();
        let names: Vec<String> = (0..guard.len())
            .filter_map(|i| guard.by_index_raw(i).ok().map(|e| e.name().to_string()))
            .filter(|n| {
                let upper = n.to_ascii_uppercase();
                upper.starts_with("META-INF/")
                    && (upper.ends_with(".RSA")
                        || upper.ends_with(".DSA")
                        || upper.ends_with(".EC"))
            })
            .collect();
        for name in names {
            if let Ok(mut entry) = guard.by_name(&name) {
                let mut data = Vec::with_capacity(entry.size() as usize);
                if entry.read_to_end(&mut data).is_ok() && !data.is_empty() {
                    out.push(data);
                }
            }
        }
        out
    }

    /// Find a raw resource file by its classpath-relative name.
    ///
    /// The `resource_name` is a forward-slash-separated path (e.g., `scrabble.txt`
    /// or `org/renaissance/jdk/streams/data.txt`). Leading slashes are stripped.
    /// Searches all classpath entries in order; returns `Some(bytes)` on first match.
    pub fn find_resource(&self, resource_name: &str) -> Option<Vec<u8>> {
        let name = resource_name.trim_start_matches('/');
        // Basic path safety
        if name.contains("..") || name.contains('\0') || name.contains('\\') {
            return None;
        }

        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    let full_path = dir.join(Path::new(name));
                    if full_path.exists() {
                        // Canonicalize and verify the resolved path is under the classpath root.
                        if let (Ok(canon_dir), Ok(canon_path)) =
                            (fs::canonicalize(dir), fs::canonicalize(&full_path))
                        {
                            if !canon_path.starts_with(&canon_dir) {
                                debug!(
                                    "Resource path traversal blocked: {} escapes {}",
                                    canon_path.display(),
                                    canon_dir.display()
                                );
                                return None;
                            }
                        }
                        if let Ok(data) = fs::read(&full_path) {
                            debug!("Found resource {name} in directory {}", dir.display());
                            return Some(data);
                        }
                    }
                }
                ClassPathEntry::JarFile { archive, multi_release, .. } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(archive, name)
                    } else {
                        Self::find_in_archive(archive, name)
                    };
                    if let Some(data) = found {
                        debug!("Found resource {name} in JAR");
                        return Some(data);
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if let Some(data) = entries_cache.get(name) {
                        debug!("Found resource {name} in nested directory");
                        return Some(data.clone());
                    }
                }
                ClassPathEntry::NestedJar { archive, nested_path, .. } => {
                    if let Some(data) = Self::find_in_archive(archive, name) {
                        debug!("Found resource {name} in nested JAR {nested_path}");
                        return Some(data);
                    }
                }
                ClassPathEntry::JmodFile { path, classes_cache, archive, .. } => {
                    // Try the pre-extracted classes cache first (covers .class + resources under classes/)
                    if let Some(data) = classes_cache.get(name) {
                        debug!("Found resource {name} in JMOD {} (cached)", path.display());
                        return Some(data.clone());
                    }
                    // Fall back to archive for non-class entries
                    let jmod_name = format!("classes/{}", name);
                    if let Some(data) = Self::find_in_archive(archive, &jmod_name) {
                        debug!("Found resource {name} in JMOD {}", path.display());
                        return Some(data);
                    }
                }
                ClassPathEntry::JImageFile {
                    path,
                    reader,
                    resource_to_modules,
                    class_to_module,
                    ..
                } => {
                    // Classes live in `class_to_module`; non-class
                    // resources live in `resource_to_modules`. Try the
                    // appropriate map based on the file extension.
                    let (full_path_attempts, is_class): (Vec<String>, bool) = if let Some(
                        class_name,
                    ) =
                        name.strip_suffix(".class")
                    {
                        if let Some(module) = class_to_module.get(class_name) {
                            (
                                vec![format!("/{module}/{class_name}.class")],
                                true,
                            )
                        } else {
                            (Vec::new(), true)
                        }
                    } else {
                        // Non-class resource: try every module that
                        // contains this path.
                        match resource_to_modules.get(name) {
                            Some(modules) => (
                                modules
                                    .iter()
                                    .map(|m| format!("/{m}/{name}"))
                                    .collect(),
                                false,
                            ),
                            None => (Vec::new(), false),
                        }
                    };
                    let _ = is_class;
                    for attempt in full_path_attempts {
                        match reader.find_resource(&attempt) {
                            Ok(Some(bytes)) => {
                                debug!(
                                    "Found resource {name} in jimage {} at {attempt}",
                                    path.display()
                                );
                                return Some(bytes);
                            }
                            Ok(None) => continue,
                            Err(_) => continue,
                        }
                    }
                }
            }
        }

        None
    }

    /// Find EVERY classpath entry that contains a resource with the given name
    /// and return a URL string for each match. This is the analogue of the
    /// JDK's `ClassLoader.getResources` enumeration — it walks every entry in
    /// order rather than stopping at the first hit.
    ///
    /// Returned URLs are strings of the form `file:/path/to/dir/<name>` for
    /// directory entries and `jar:file:/path/to/foo.jar!/<name>` for JAR /
    /// nested-JAR / JMOD / jimage entries. Callers can feed these into
    /// `java.net.URL` directly.
    pub fn find_all_resource_urls(&self, resource_name: &str) -> Vec<String> {
        let name = resource_name.trim_start_matches('/');
        if name.contains("..") || name.contains('\0') || name.contains('\\') {
            return Vec::new();
        }
        let mut urls = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    let full_path = dir.join(Path::new(name));
                    if full_path.exists() {
                        if let (Ok(canon_dir), Ok(canon_path)) =
                            (fs::canonicalize(dir), fs::canonicalize(&full_path))
                        {
                            if !canon_path.starts_with(&canon_dir) {
                                continue;
                            }
                        }
                        let abs = fs::canonicalize(&full_path).unwrap_or(full_path);
                        let p = abs.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p);
                        let p = p.trim_start_matches('/');
                        urls.push(format!("file:/{p}"));
                    }
                }
                ClassPathEntry::JarFile { archive, multi_release, path } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(archive, name).is_some()
                    } else {
                        Self::find_in_archive(archive, name).is_some()
                    };
                    if found {
                        let abs = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                        let p = abs.to_string_lossy().replace('\\', "/");
                        // Strip UNC prefix \\?\ that canonicalize produces on Windows.
                        let p = p.strip_prefix("//?/").unwrap_or(&p);
                        let p = p.trim_start_matches('/');
                        urls.push(format!("jar:file:/{p}!/{name}"));
                    }
                }
                ClassPathEntry::NestedDirectory { parent_jar, prefix, entries_cache } => {
                    if entries_cache.contains_key(name) {
                        let p = parent_jar.to_string_lossy().replace('\\', "/");
                        let p = p.trim_start_matches('/');
                        urls.push(format!("jar:file:/{p}!/{prefix}{name}"));
                    }
                }
                ClassPathEntry::NestedJar { parent_jar, archive, nested_path } => {
                    if Self::find_in_archive(archive, name).is_some() {
                        let p = parent_jar.to_string_lossy().replace('\\', "/");
                        let p = p.trim_start_matches('/');
                        urls.push(format!("jar:file:/{p}!/{nested_path}!/{name}"));
                    }
                }
                ClassPathEntry::JmodFile { path, classes_cache, archive, .. } => {
                    let found = classes_cache.contains_key(name) || {
                        let jmod_name = format!("classes/{}", name);
                        Self::find_in_archive(archive, &jmod_name).is_some()
                    };
                    if found {
                        let p = path.to_string_lossy().replace('\\', "/");
                        let p = p.trim_start_matches('/');
                        urls.push(format!("jar:file:/{p}!/{name}"));
                    }
                }
                ClassPathEntry::JImageFile {
                    path, reader, resource_to_modules, class_to_module, ..
                } => {
                    let attempts: Vec<String> = if let Some(class_name) =
                        name.strip_suffix(".class")
                    {
                        if let Some(module) = class_to_module.get(class_name) {
                            vec![format!("/{module}/{class_name}.class")]
                        } else {
                            Vec::new()
                        }
                    } else {
                        match resource_to_modules.get(name) {
                            Some(modules) => modules
                                .iter()
                                .map(|m| format!("/{m}/{name}"))
                                .collect(),
                            None => Vec::new(),
                        }
                    };
                    for attempt in attempts {
                        if matches!(reader.find_resource(&attempt), Ok(Some(_))) {
                            let p = path.to_string_lossy().replace('\\', "/");
                            let p = p.trim_start_matches('/');
                            // Use jrt: scheme for jimage, matching the JDK.
                            urls.push(format!("jrt:/{p}!{attempt}"));
                        }
                    }
                }
            }
        }
        urls
    }

    /// Scan every JAR/directory for `module-info.class` files and return
    /// their raw bytes.  A JAR may contain at most one `module-info.class`
    /// at its root; a directory may contain one too.
    ///
    /// Used by [`ClassManager`](super::ClassManager) during initialisation to
    /// pre-populate the module registry before any class is loaded.
    pub fn scan_module_infos(&self) -> Vec<Vec<u8>> {
        let mut results = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    let path = dir.join("module-info.class");
                    if let Ok(data) = std::fs::read(&path) {
                        debug!("Found module-info.class in directory {}", dir.display());
                        results.push(data);
                    }
                }
                ClassPathEntry::JarFile { archive, multi_release, .. } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(archive, "module-info.class")
                    } else {
                        Self::find_in_archive(archive, "module-info.class")
                    };
                    if let Some(data) = found {
                        debug!("Found module-info.class in JAR");
                        results.push(data);
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if let Some(data) = entries_cache.get("module-info.class") {
                        debug!("Found module-info.class in nested directory");
                        results.push(data.clone());
                    }
                }
                ClassPathEntry::NestedJar { archive, nested_path, .. } => {
                    if let Some(data) = Self::find_in_archive(archive, "module-info.class") {
                        debug!("Found module-info.class in nested JAR {nested_path}");
                        results.push(data);
                    }
                }
                ClassPathEntry::JmodFile { path, classes_cache, .. } => {
                    if let Some(data) = classes_cache.get("module-info.class") {
                        debug!("Found module-info.class in JMOD {}", path.display());
                        results.push(data.clone());
                    }
                }
                ClassPathEntry::JImageFile {
                    path,
                    reader,
                    module_names,
                    ..
                } => {
                    // One module-info per module inside the jimage. Ask
                    // the reader for `/<module>/module-info.class` for
                    // every known module name.
                    for module in module_names {
                        let query = format!("/{module}/module-info.class");
                        match reader.find_resource(&query) {
                            Ok(Some(bytes)) => {
                                debug!(
                                    "Found module-info.class for {module} in jimage {}",
                                    path.display()
                                );
                                results.push(bytes);
                            }
                            Ok(None) => continue,
                            Err(_) => continue,
                        }
                    }
                }
            }
        }
        results
    }

    /// List all class names available on this classpath.
    ///
    /// Returns binary class names (e.g. `com/example/MyClass`) for every
    /// `.class` file found across all entries. `module-info` is excluded.
    pub fn list_class_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    Self::walk_dir_classes(dir, dir, &mut names);
                }
                ClassPathEntry::JarFile { archive, .. } => {
                    Self::list_archive_classes(archive, &mut names);
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    for key in entries_cache.keys() {
                        if let Some(cn) = Self::class_file_to_name(key) {
                            names.push(cn);
                        }
                    }
                }
                ClassPathEntry::NestedJar { archive, .. } => {
                    Self::list_archive_classes(archive, &mut names);
                }
                ClassPathEntry::JmodFile { all_entry_names, .. } => {
                    for name in all_entry_names {
                        if let Some(class_path) = name.strip_prefix("classes/") {
                            if let Some(cn) = Self::class_file_to_name(class_path) {
                                names.push(cn);
                            }
                        }
                    }
                }
                ClassPathEntry::JImageFile {
                    class_to_module, ..
                } => {
                    // Every key in the class-to-module index is exactly
                    // a binary class name.
                    for name in class_to_module.keys() {
                        names.push(name.clone());
                    }
                }
            }
        }
        names
    }

    /// Return the list of JMOD module names on this classpath.
    ///
    /// Module names are extracted from the JMOD file name stem
    /// (e.g. `java.base.jmod` → `"java.base"`).
    /// Only `JmodFile` entries contribute; JARs and directories are skipped.
    pub fn list_jmod_modules(&self) -> Vec<String> {
        let mut modules = Vec::new();
        for entry in &self.entries {
            if let ClassPathEntry::JmodFile { path, .. } = entry {
                if let Some(stem) = path.file_stem() {
                    modules.push(stem.to_string_lossy().into_owned());
                }
            }
        }
        modules
    }

    /// Return the total number of classes available in JMOD entries.
    pub fn jmod_class_count(&self) -> usize {
        self.entries.iter().map(|e| match e {
            ClassPathEntry::JmodFile { classes_cache, .. } => classes_cache.len(),
            _ => 0,
        }).sum()
    }

    /// Return the number of classes known to the jimage entries on this
    /// classpath. Unlike `jmod_class_count`, this is O(1) per entry since
    /// the class-to-module index was built at load time.
    pub fn jimage_class_count(&self) -> usize {
        self.entries
            .iter()
            .map(|e| match e {
                ClassPathEntry::JImageFile {
                    class_to_module, ..
                } => class_to_module.len(),
                _ => 0,
            })
            .sum()
    }

    /// List the module names contributed by jimage entries on this
    /// classpath. Deduplicated, sorted alphabetically.
    pub fn list_jimage_modules(&self) -> Vec<String> {
        let mut modules = Vec::new();
        for entry in &self.entries {
            if let ClassPathEntry::JImageFile { module_names, .. } = entry {
                for m in module_names {
                    if !modules.contains(m) {
                        modules.push(m.clone());
                    }
                }
            }
        }
        modules.sort();
        modules
    }

    /// Load a JDK 9+ `lib/modules` jimage file onto this classpath.
    ///
    /// Opens the file via [`rustjvm_reader::JImageReader`], walks every
    /// entry once to build the class-to-module and resource-to-modules
    /// indexes, then inserts a [`ClassPathEntry::JImageFile`] onto the
    /// classpath. Lookups after this call return bytes directly from
    /// the jimage's perfect-hash index.
    ///
    /// Returns an error string if the file cannot be opened or is not
    /// a valid jimage. Callers that want a typed error should match on
    /// [`rustjvm_reader::JImageError`] via the direct reader API.
    pub fn add_jimage(&mut self, path: impl AsRef<Path>) -> Result<(), String> {
        let path = path.as_ref().to_path_buf();
        let entry = Self::load_jimage(&path)?;
        self.entries.push(entry);
        Ok(())
    }

    /// Internal constructor for a [`ClassPathEntry::JImageFile`]. Reads
    /// the entire jimage, decodes every location record once, and
    /// builds the two lookup indexes. Returns a string error so the
    /// caller can surface it through the same error path as `load_jmod`.
    fn load_jimage(path: &Path) -> Result<ClassPathEntry, String> {
        let reader = rustjvm_reader::JImageReader::open(path)
            .map_err(|e| format!("open jimage {}: {e}", path.display()))?;

        // Walk every entry once to build the indexes.
        let entries = reader
            .iter_entries()
            .map_err(|e| format!("walk jimage {}: {e}", path.display()))?;

        let mut class_to_module: HashMap<String, String> = HashMap::new();
        let mut resource_to_modules: HashMap<String, Vec<String>> = HashMap::new();
        let mut module_set: std::collections::BTreeSet<String> = Default::default();

        for (full_path, _offset, _size) in entries {
            // Full paths look like "/module/a/b/C.class" or
            // "/module/some/resource.txt". Split off the leading
            // "/module/" prefix.
            let without_lead = match full_path.strip_prefix('/') {
                Some(s) => s,
                None => continue,
            };
            let (module, rest) = match without_lead.split_once('/') {
                Some((m, r)) => (m.to_string(), r.to_string()),
                None => continue,
            };
            module_set.insert(module.clone());

            if let Some(class_name) = rest.strip_suffix(".class") {
                // `module-info` is per-module, not a globally unique
                // class name. Skipping it from `class_to_module` avoids
                // one module's module-info silently overwriting another's
                // via the final insert. Callers that need module-info
                // bytes go through `scan_module_infos`, which walks the
                // module list directly and queries the reader for each.
                if class_name != "module-info" {
                    class_to_module.insert(class_name.to_string(), module);
                }
            } else {
                resource_to_modules
                    .entry(rest)
                    .or_default()
                    .push(module);
            }
        }

        debug!(
            "Loaded jimage {}: {} classes, {} non-class resources, {} modules",
            path.display(),
            class_to_module.len(),
            resource_to_modules.len(),
            module_set.len()
        );

        Ok(ClassPathEntry::JImageFile {
            path: path.to_path_buf(),
            reader,
            class_to_module,
            resource_to_modules,
            module_names: module_set.into_iter().collect(),
        })
    }

    fn walk_dir_classes(base: &Path, dir: &Path, out: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    Self::walk_dir_classes(base, &path, out);
                } else if path.extension().is_some_and(|e| e == "class") {
                    if let Ok(rel) = path.strip_prefix(base) {
                        let s = rel.to_string_lossy().replace('\\', "/");
                        if let Some(cn) = Self::class_file_to_name(&s) {
                            out.push(cn);
                        }
                    }
                }
            }
        }
    }

    fn list_archive_classes(archive: &Mutex<ZipArchive<Cursor<Vec<u8>>>>, out: &mut Vec<String>) {
        let mut guard = archive.lock();
        for i in 0..guard.len() {
            if let Ok(entry) = guard.by_index_raw(i) {
                let name = entry.name().to_string();
                if let Some(cn) = Self::class_file_to_name(&name) {
                    out.push(cn);
                }
            }
        }
    }

    fn class_file_to_name(path: &str) -> Option<String> {
        let path = path.strip_suffix(".class")?;
        if path == "module-info" || path.ends_with("/module-info") {
            return None;
        }
        Some(path.to_string())
    }

    /// Helper: try to read a named entry from a mutex-guarded ZipArchive.
    fn find_in_archive(
        archive: &Mutex<ZipArchive<Cursor<Vec<u8>>>>,
        name: &str,
    ) -> Option<Vec<u8>> {
        let mut guard = archive.lock();
        let result = guard.by_name(name).and_then(|mut zip_entry| {
            let mut data = Vec::with_capacity(zip_entry.size() as usize);
            zip_entry.read_to_end(&mut data)?;
            Ok(data)
        });
        match result {
            Ok(data) => Some(data),
            Err(_) => None,
        }
    }

    /// JMOD file header prefix: `JM` (0x4A 0x4D).
    const JMOD_MAGIC_PREFIX: [u8; 2] = [0x4A, 0x4D];

    /// Known JMOD major versions we support. JDK 9-25 all use major version 1.
    const JMOD_SUPPORTED_MAJOR: u8 = 0x01;

    /// Maximum JMOD minor version we accept (currently 0, future-proofed to 0xFF).
    const JMOD_MAX_MINOR: u8 = 0xFF;

    /// Load a JDK 9+ JMOD file. JMOD is a ZIP archive with a 4-byte header:
    ///   - Bytes 0-1: Magic `JM` (0x4A 0x4D)
    ///   - Byte 2: Major version (currently 0x01 for all JDK 9-25)
    ///   - Byte 3: Minor version (currently 0x00)
    ///
    /// Class files inside are stored under `classes/` (e.g. `classes/java/lang/Object.class`).
    ///
    /// All entries under `classes/` are pre-extracted into an in-memory HashMap
    /// so that `find_class` is a simple HashMap lookup with no decompression.
    /// This is critical for debug-mode performance where deflate is extremely
    /// slow (~30s for 200 classes vs <2s with pre-extraction).
    fn load_jmod(path: &Path) -> Result<ClassPathEntry, String> {
        let data = fs::read(path).map_err(|e| format!("failed to read: {e}"))?;
        if data.len() < 4 {
            return Err("file too small to be a valid JMOD".to_string());
        }
        // Validate magic prefix
        if data[..2] != Self::JMOD_MAGIC_PREFIX {
            return Err(format!(
                "not a valid JMOD file (expected magic 0x4A4D, got 0x{:02X}{:02X})",
                data[0], data[1]
            ));
        }
        // Validate major version
        let major = data[2];
        let minor = data[3];
        if major != Self::JMOD_SUPPORTED_MAJOR {
            return Err(format!(
                "unsupported JMOD major version {major} (expected {}); \
                 this may be from a future JDK",
                Self::JMOD_SUPPORTED_MAJOR
            ));
        }
        if minor > Self::JMOD_MAX_MINOR {
            return Err(format!(
                "unsupported JMOD minor version {minor} (max {})",
                Self::JMOD_MAX_MINOR
            ));
        }
        debug!(
            "JMOD {} version {}.{}", path.display(), major, minor
        );
        // Strip the 4-byte JMOD magic prefix to get standard ZIP data
        let zip_data = data[4..].to_vec();
        let cursor = Cursor::new(zip_data.clone());
        let mut archive = ZipArchive::new(cursor)
            .map_err(|e| format!("failed to parse ZIP inside JMOD: {e}"))?;

        let total_entries = archive.len();

        // Pre-extract all class entries into an in-memory cache.
        let mut classes_cache = HashMap::new();
        let mut all_entry_names = Vec::with_capacity(total_entries);

        for i in 0..total_entries {
            let name = match archive.by_index_raw(i) {
                Ok(entry) => entry.name().to_string(),
                Err(_) => continue,
            };
            all_entry_names.push(name.clone());

            if let Some(relative) = name.strip_prefix("classes/") {
                if !relative.is_empty() && !relative.ends_with('/') {
                    if let Ok(mut entry) = archive.by_name(&name) {
                        let mut buf = Vec::with_capacity(entry.size() as usize);
                        if entry.read_to_end(&mut buf).is_ok() {
                            classes_cache.insert(relative.to_string(), buf);
                        }
                    }
                }
            }
        }

        debug!(
            "Loaded JMOD {} ({} entries, {} classes pre-cached)",
            path.display(),
            total_entries,
            classes_cache.len()
        );

        // Re-open the archive for rare non-class resource lookups
        let cursor2 = Cursor::new(zip_data);
        let archive2 = ZipArchive::new(cursor2)
            .map_err(|e| format!("failed to re-parse ZIP inside JMOD: {e}"))?;

        Ok(ClassPathEntry::JmodFile {
            path: path.to_path_buf(),
            classes_cache,
            all_entry_names,
            archive: Mutex::new(archive2),
        })
    }


}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn empty_classpath_finds_nothing() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object").is_err());
    }

    #[test]
    fn nonexistent_jar_skipped() {
        let cp = ClassPath::new(&["nonexistent.jar".to_string()]);
        assert!(cp.is_empty());
    }

    #[test]
    fn load_class_from_jar() {
        // Create a temporary JAR file with a fake .class entry
        let dir = std::env::temp_dir().join("rustjvm_test_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("test.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip_writer = zip::ZipWriter::new(file);

        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip_writer
            .start_file("com/example/Hello.class", options)
            .unwrap();
        zip_writer
            .write_all(b"\xCA\xFE\xBA\xBE_fake_class_data")
            .unwrap();
        zip_writer.finish().unwrap();

        // Load from the JAR
        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty());

        let data = cp.find_class("com/example/Hello").unwrap();
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.len() > 4);

        // Not found in JAR
        assert!(cp.find_class("com/example/Missing").is_err());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_format() {
        let cp = ClassPath::new(&[]);
        let debug = format!("{cp:?}");
        assert!(debug.contains("ClassPath"));
    }

    // -- Path traversal protection tests --

    #[test]
    fn path_traversal_double_dot_rejected() {
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("java/lang/../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn path_traversal_absolute_path_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("/etc/passwd").is_err());
    }

    #[test]
    fn path_traversal_backslash_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("\\\\server\\share").is_err());
    }

    #[test]
    fn path_traversal_null_byte_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object\0.evil").is_err());
    }

    #[test]
    fn valid_class_name_format_accepted() {
        // Valid names should not be rejected (they'll fail with ClassNotFound
        // because there's no classpath entry, but NOT due to validation)
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("java/lang/Object");
        assert!(
            matches!(result, Err(ClassFileError::ClassNotFound { .. })),
            "expected ClassNotFound, got {result:?}"
        );
    }

    #[test]
    fn find_resource_from_directory() {
        let dir = std::env::temp_dir().join("rustjvm_test_resource_dir");
        let _ = fs::create_dir_all(&dir);
        let resource_path = dir.join("hello.txt");
        fs::write(&resource_path, b"hello world").unwrap();

        let cp = ClassPath::new(&[dir.to_string_lossy().into_owned()]);
        let data = cp.find_resource("hello.txt").expect("resource should be found");
        assert_eq!(data, b"hello world");

        assert!(cp.find_resource("missing.txt").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_from_jar() {
        let dir = std::env::temp_dir().join("rustjvm_test_resource_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("resources.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip_writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip_writer.start_file("data/words.txt", options).unwrap();
        zip_writer.write_all(b"word1\nword2\nword3").unwrap();
        zip_writer.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_resource("data/words.txt").expect("resource in JAR");
        assert_eq!(data, b"word1\nword2\nword3");

        // Leading slash is stripped
        let data2 = cp.find_resource("/data/words.txt").expect("leading slash stripped");
        assert_eq!(data2, b"word1\nword2\nword3");

        assert!(cp.find_resource("data/missing.txt").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_path_traversal_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("../etc/passwd").is_none());
        assert!(cp.find_resource("foo\0bar").is_none());
    }

    // -- Spring Boot fat JAR tests --

    /// Helper: create a Spring Boot fat JAR with:
    /// - META-INF/MANIFEST.MF (with Start-Class)
    /// - BOOT-INF/classes/com/example/App.class
    /// - BOOT-INF/lib/dep.jar (containing org/dep/Util.class)
    /// - org/springframework/boot/loader/JarLauncher.class (launcher at root)
    fn create_fat_jar(jar_path: &Path) {
        let file = fs::File::create(jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // MANIFEST.MF
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(
            b"Manifest-Version: 1.0\r\n\
              Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
              Start-Class: com.example.App\r\n\
              Spring-Boot-Classes: BOOT-INF/classes/\r\n\
              Spring-Boot-Lib: BOOT-INF/lib/\r\n",
        )
        .unwrap();

        // Application class in BOOT-INF/classes/
        zip.start_file("BOOT-INF/classes/com/example/App.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_app_class").unwrap();

        // Resource in BOOT-INF/classes/
        zip.start_file("BOOT-INF/classes/application.properties", opts)
            .unwrap();
        zip.write_all(b"server.port=8080").unwrap();

        // Nested dependency JAR in BOOT-INF/lib/
        let mut dep_jar_buf = Vec::new();
        {
            let cursor = Cursor::new(&mut dep_jar_buf);
            let mut dep_zip = zip::ZipWriter::new(cursor);
            dep_zip
                .start_file("org/dep/Util.class", opts)
                .unwrap();
            dep_zip
                .write_all(b"\xCA\xFE\xBA\xBE_util_class")
                .unwrap();
            dep_zip
                .start_file("META-INF/services/org.dep.SPI", opts)
                .unwrap();
            dep_zip.write_all(b"org.dep.SpiImpl").unwrap();
            dep_zip.finish().unwrap();
        }
        zip.start_file("BOOT-INF/lib/dep-1.0.jar", opts).unwrap();
        zip.write_all(&dep_jar_buf).unwrap();

        // Launcher class at JAR root
        zip.start_file(
            "org/springframework/boot/loader/JarLauncher.class",
            opts,
        )
        .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_launcher").unwrap();

        zip.finish().unwrap();
    }

    #[test]
    fn fat_jar_detects_spring_boot_structure() {
        let dir = std::env::temp_dir().join("rustjvm_test_fatjar_detect");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);

        // Should have multiple entries: NestedDirectory + NestedJar(s) + outer JarFile
        assert!(
            cp.entry_count() >= 3,
            "expected >=3 entries, got {}",
            cp.entry_count()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_class_from_boot_inf_classes() {
        let dir = std::env::temp_dir().join("rustjvm_test_fatjar_classes");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("com/example/App")
            .expect("should find App.class from BOOT-INF/classes/");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_app_class"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_class_from_nested_jar() {
        let dir = std::env::temp_dir().join("rustjvm_test_fatjar_nested");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("org/dep/Util")
            .expect("should find Util.class from BOOT-INF/lib/dep-1.0.jar");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_util_class"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_launcher_from_root() {
        let dir = std::env::temp_dir().join("rustjvm_test_fatjar_root");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("org/springframework/boot/loader/JarLauncher")
            .expect("should find JarLauncher from JAR root");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_launcher"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_resource_from_boot_inf_classes() {
        let dir = std::env::temp_dir().join("rustjvm_test_fatjar_resource");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("application.properties")
            .expect("should find application.properties from BOOT-INF/classes/");
        assert_eq!(data, b"server.port=8080");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_resource_from_nested_jar() {
        let dir = std::env::temp_dir().join("rustjvm_test_fatjar_spi");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("META-INF/services/org.dep.SPI")
            .expect("should find SPI file from nested JAR");
        assert_eq!(data, b"org.dep.SpiImpl");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_class_not_found_returns_error() {
        let dir = std::env::temp_dir().join("rustjvm_test_fatjar_missing");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(cp.find_class("com/example/Missing").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_parsing() {
        let manifest = b"Manifest-Version: 1.0\r\n\
            Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
            Start-Class: com.example.DemoApplication\r\n\
            Spring-Boot-Version: 3.2.0\r\n\
            Spring-Boot-Classes: BOOT-INF/classes/\r\n\
            Spring-Boot-Lib: BOOT-INF/lib/\r\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.main_class.as_deref(),
            Some("org.springframework.boot.loader.JarLauncher")
        );
        assert_eq!(
            info.start_class.as_deref(),
            Some("com.example.DemoApplication")
        );
        assert!(info.is_spring_boot());
    }

    #[test]
    fn manifest_continuation_lines() {
        // MANIFEST.MF allows continuation lines starting with a single space
        let manifest = b"Manifest-Version: 1.0\r\n\
            Main-Class: org.springframework.boot.loader.JarLa\r\n \
            uncher\r\n\
            Start-Class: com.example.App\r\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.main_class.as_deref(),
            Some("org.springframework.boot.loader.JarLauncher")
        );
    }

    #[test]
    fn plain_jar_not_treated_as_fat_jar() {
        let dir = std::env::temp_dir().join("rustjvm_test_plain_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("plain.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_foo").unwrap();
        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        // Plain JAR should have exactly 1 entry (JarFile)
        assert_eq!(cp.entry_count(), 1);

        let data = cp.find_class("com/example/Foo").unwrap();
        assert!(data.ends_with(b"_foo"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dynamic_add_path_fat_jar() {
        let dir = std::env::temp_dir().join("rustjvm_test_dynamic_fatjar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let mut cp = ClassPath::new(&[]);
        assert!(cp.is_empty());

        cp.add_path(&jar_path.to_string_lossy());
        assert!(!cp.is_empty());

        // Should find classes from nested structure
        assert!(cp.find_class("com/example/App").is_ok());
        assert!(cp.find_class("org/dep/Util").is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // JMOD file tests
    // -----------------------------------------------------------------------

    #[test]
    fn jmod_magic_validation() {
        // A file with wrong magic should fail
        let dir = std::env::temp_dir().join("rustjvm_test_jmod");
        let _ = fs::create_dir_all(&dir);
        let bad_path = dir.join("bad.jmod");
        fs::write(&bad_path, b"NOT_JMOD_DATA").unwrap();
        assert!(ClassPath::load_jmod(&bad_path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_class_from_synthetic_jmod() {
        // Create a synthetic JMOD file: 4-byte magic + ZIP with classes/ prefix
        let dir = std::env::temp_dir().join("rustjvm_test_jmod2");
        let _ = fs::create_dir_all(&dir);
        let jmod_path = dir.join("test.jmod");

        // Build the ZIP portion in memory
        let mut zip_buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut zip_buf);
            let mut zip_writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip_writer
                .start_file("classes/java/lang/Object.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_fake_object")
                .unwrap();
            zip_writer
                .start_file("classes/java/lang/String.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_fake_string")
                .unwrap();
            zip_writer
                .start_file("classes/module-info.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_module_info")
                .unwrap();
            zip_writer.finish().unwrap();
        }

        // Write JMOD = magic + ZIP data
        let mut jmod_data = Vec::new();
        jmod_data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
        jmod_data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
        jmod_data.push(0x00); // minor version
        jmod_data.extend_from_slice(&zip_buf);
        fs::write(&jmod_path, &jmod_data).unwrap();

        // Load JMOD as classpath entry
        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty());

        // Find classes (the prefix "classes/" should be stripped automatically)
        let obj_bytes = cp.find_class("java/lang/Object").unwrap();
        assert_eq!(&obj_bytes[..4], b"\xCA\xFE\xBA\xBE");

        let str_bytes = cp.find_class("java/lang/String").unwrap();
        assert_eq!(&str_bytes[..4], b"\xCA\xFE\xBA\xBE");

        // Unknown class not found
        assert!(cp.find_class("java/lang/Missing").is_err());

        // list_class_names should find both (but not module-info)
        let names = cp.list_class_names();
        assert!(names.contains(&"java/lang/Object".to_string()));
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(!names.iter().any(|n| n.contains("module-info")));

        // module-info should be found by scan_module_infos
        let modules = cp.scan_module_infos();
        assert_eq!(modules.len(), 1);

        // Resource lookup should work via the classes/ prefix
        let resource = cp.find_resource("java/lang/Object.class");
        assert!(resource.is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_real_jdk_jmod() {
        // Skip this test if no JDK is available
        let java_home = std::env::var("JAVA_HOME").ok()
            .or_else(|| {
                let candidate = std::path::PathBuf::from("C:/Program Files/Java/jdk-25");
                if candidate.exists() { Some(candidate.to_string_lossy().into_owned()) } else { None }
            });
        let Some(java_home) = java_home else {
            eprintln!("Skipping load_real_jdk_jmod: no JAVA_HOME set");
            return;
        };
        let jmod_path = PathBuf::from(&java_home).join("jmods").join("java.base.jmod");
        if !jmod_path.exists() {
            eprintln!("Skipping load_real_jdk_jmod: {} not found", jmod_path.display());
            return;
        }

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty(), "JMOD classpath should not be empty");

        // Must find java.lang.Object
        let obj_bytes = cp.find_class("java/lang/Object").unwrap();
        assert_eq!(&obj_bytes[..4], b"\xCA\xFE\xBA\xBE", "Object.class should have class file magic");
        assert!(obj_bytes.len() > 100, "Object.class should be non-trivial");

        // Must find java.lang.String
        let str_bytes = cp.find_class("java/lang/String").unwrap();
        assert_eq!(&str_bytes[..4], b"\xCA\xFE\xBA\xBE");
        assert!(str_bytes.len() > 1000, "String.class should be substantial");

        // Must find java.util.HashMap
        let hm_bytes = cp.find_class("java/util/HashMap").unwrap();
        assert_eq!(&hm_bytes[..4], b"\xCA\xFE\xBA\xBE");

        // Class listing should have thousands of classes
        let names = cp.list_class_names();
        assert!(names.len() > 1000, "java.base should have >1000 classes, got {}", names.len());
        assert!(names.contains(&"java/lang/Object".to_string()));
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(names.contains(&"java/util/HashMap".to_string()));
        assert!(names.contains(&"java/util/ArrayList".to_string()));
        assert!(names.contains(&"java/io/InputStream".to_string()));

        // module-info.class should be found
        let modules = cp.scan_module_infos();
        assert_eq!(modules.len(), 1, "java.base.jmod should have exactly 1 module-info");
    }

    // --- Phase 80.5: Path Traversal Fix Tests ---

    #[test]
    fn find_class_rejects_dotdot_traversal() {
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("../../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn find_class_rejects_absolute_path() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("/etc/passwd").is_err());
        assert!(cp.find_class("\\Windows\\System32\\cmd").is_err());
    }

    #[test]
    fn find_class_rejects_null_byte() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object\0.evil").is_err());
    }

    #[test]
    fn find_class_rejects_windows_drive() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("C:Windows/System32/cmd").is_err());
    }

    #[test]
    fn find_class_rejects_current_dir_reference() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("./java/lang/Object").is_err());
        assert!(cp.find_class(".\\java\\lang\\Object").is_err());
    }

    #[test]
    fn find_resource_rejects_dotdot_traversal() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("../../../etc/passwd").is_none());
    }

    #[test]
    fn find_resource_rejects_null_byte() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("file\0.txt").is_none());
    }

    #[test]
    fn find_class_canonicalize_with_real_dir() {
        // Create a temp directory with a class file, verify canonicalization works
        let tmp = std::env::temp_dir().join("rustjvm_test_80_5");
        let _ = std::fs::create_dir_all(&tmp);
        let class_dir = tmp.join("com").join("test");
        let _ = std::fs::create_dir_all(&class_dir);
        // Write a minimal (invalid but loadable) class file
        let _ = std::fs::write(class_dir.join("Foo.class"), b"CAFEBABE_fake");
        let mut cp = ClassPath::new(&[]);
        cp.add_path(tmp.to_str().unwrap());
        // Normal lookup should work
        let result = cp.find_class("com/test/Foo");
        assert!(result.is_ok());
        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── JMOD integration tests ────────────────────────────────────────
    // These require a real JDK 9+ installation. Run with:
    //   cargo test -p rustjvm-classloading -- --ignored

    /// Helper: find java.base.jmod on this machine.
    fn find_java_base_jmod() -> Option<PathBuf> {
        // Try JAVA_HOME first
        if let Ok(val) = std::env::var("JAVA_HOME") {
            let p = PathBuf::from(&val).join("jmods").join("java.base.jmod");
            if p.exists() {
                return Some(p);
            }
        }
        // Try common Windows paths
        for dir in &[
            r"C:\Program Files\Java",
            r"C:\Program Files\Eclipse Adoptium",
        ] {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let jmod = entry.path().join("jmods").join("java.base.jmod");
                    if jmod.exists() {
                        return Some(jmod);
                    }
                }
            }
        }
        // Try running java to find it
        let output = std::process::Command::new("java")
            .args(["-XshowSettings:properties", "-version"])
            .output()
            .ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("java.home") {
                if let Some(value) = rest.trim().strip_prefix('=') {
                    let jmod = PathBuf::from(value.trim()).join("jmods").join("java.base.jmod");
                    if jmod.exists() {
                        return Some(jmod);
                    }
                }
            }
        }
        None
    }

    #[test]
    #[ignore] // requires JDK on host
    fn load_jmod_validates_magic_and_version() {
        let jmod_path = find_java_base_jmod();
        assert!(jmod_path.is_some(), "No JDK found — cannot test JMOD loading");
        let jmod_path = jmod_path.unwrap();

        // Should load successfully
        let entry = ClassPath::load_jmod(&jmod_path);
        assert!(
            entry.is_ok(),
            "Failed to load {}: {:?}",
            jmod_path.display(),
            entry.err()
        );

        // Verify it's a JmodFile variant with a populated classes_cache
        match entry.unwrap() {
            ClassPathEntry::JmodFile { classes_cache, all_entry_names, .. } => {
                assert!(
                    classes_cache.len() > 100,
                    "java.base should have >100 classes, got {}",
                    classes_cache.len()
                );
                assert!(
                    all_entry_names.len() > classes_cache.len(),
                    "all_entry_names should include non-class entries too"
                );
                // Should contain java.lang.Object
                assert!(
                    classes_cache.contains_key("java/lang/Object.class"),
                    "java.base should contain java/lang/Object.class"
                );
                // Should contain java.lang.String
                assert!(
                    classes_cache.contains_key("java/lang/String.class"),
                    "java.base should contain java/lang/String.class"
                );
                eprintln!(
                    "java.base.jmod: {} classes, {} total entries",
                    classes_cache.len(),
                    all_entry_names.len()
                );
            }
            other => panic!("Expected JmodFile, got {other:?}"),
        }
    }

    #[test]
    #[ignore] // requires JDK on host
    fn find_class_from_jmod_returns_valid_classfile() {
        let jmod_path = find_java_base_jmod();
        if jmod_path.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmod_path = jmod_path.unwrap();

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);

        // Find java.lang.Object
        let bytes = cp.find_class("java/lang/Object").unwrap();
        // Verify it starts with the class file magic
        assert_eq!(
            &bytes[..4],
            &[0xCA, 0xFE, 0xBA, 0xBE],
            "java.lang.Object should start with CAFEBABE"
        );
        // Check class file version is reasonable (major >= 45 for any JDK)
        let major = u16::from_be_bytes([bytes[6], bytes[7]]);
        assert!(
            major >= 45 && major <= 80,
            "class file major version {major} out of expected range"
        );
        eprintln!(
            "java.lang.Object: {} bytes, class file version {major}",
            bytes.len()
        );

        // Find java.util.HashMap
        let bytes = cp.find_class("java/util/HashMap").unwrap();
        assert_eq!(&bytes[..4], &[0xCA, 0xFE, 0xBA, 0xBE]);

        // Non-existent class should fail
        assert!(cp.find_class("java/lang/NonExistent").is_err());
    }

    #[test]
    #[ignore] // requires JDK on host
    fn list_jmod_modules_returns_module_names() {
        let jmod_path = find_java_base_jmod();
        if jmod_path.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmod_path = jmod_path.unwrap();

        // Load just java.base
        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        let modules = cp.list_jmod_modules();
        assert_eq!(modules, vec!["java.base"]);

        // Load all jmods from the JDK
        let jmods_dir = jmod_path.parent().unwrap();
        let mut all_jmods: Vec<String> = fs::read_dir(jmods_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jmod"))
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        all_jmods.sort();

        let cp = ClassPath::new(&all_jmods);
        let modules = cp.list_jmod_modules();
        assert!(modules.contains(&"java.base".to_string()));
        assert!(modules.contains(&"java.logging".to_string()));
        assert!(
            modules.len() >= 10,
            "Expected at least 10 modules, got {}",
            modules.len()
        );
        eprintln!("Found {} modules: {:?}", modules.len(), &modules[..5.min(modules.len())]);
    }

    #[test]
    fn load_jmod_rejects_bad_magic() {
        let dir = std::env::temp_dir().join("rustjvm_test_bad_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("bad.jmod");

        // Write a file with wrong magic
        fs::write(&path, b"BAAD\x00\x00\x00\x00").unwrap();
        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("magic"),
            "Error should mention magic: {err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_jmod_rejects_unsupported_major_version() {
        let dir = std::env::temp_dir().join("rustjvm_test_future_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("future.jmod");

        // Write JMOD header with major version 99 (unsupported)
        // followed by minimal ZIP data (will fail ZIP parse, but version check comes first)
        let mut data = vec![0x4A, 0x4D, 99, 0x00]; // JM + major=99 + minor=0
        data.extend_from_slice(&[0; 100]); // padding
        fs::write(&path, &data).unwrap();

        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("major version"),
            "Error should mention major version: {err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_jmod_rejects_too_small_file() {
        let dir = std::env::temp_dir().join("rustjvm_test_tiny_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("tiny.jmod");

        fs::write(&path, b"JM").unwrap(); // Only 2 bytes
        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("too small"),
            "Error should mention file too small: {err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jmod_class_count_empty_classpath() {
        let cp = ClassPath::new(&[]);
        assert_eq!(cp.jmod_class_count(), 0);
    }

    #[test]
    fn list_jmod_modules_empty_classpath() {
        let cp = ClassPath::new(&[]);
        assert!(cp.list_jmod_modules().is_empty());
    }

    // -- ManifestInfo parsing tests --

    #[test]
    fn manifest_multi_release_true() {
        let data = b"Multi-Release: true\nMain-Class: com.example.Main\n";
        let info = ManifestInfo::parse(data);
        assert!(info.multi_release);
        assert_eq!(info.main_class.as_deref(), Some("com.example.Main"));
    }

    #[test]
    fn manifest_multi_release_false_by_default() {
        let data = b"Main-Class: com.example.Main\n";
        let info = ManifestInfo::parse(data);
        assert!(!info.multi_release);
    }

    #[test]
    fn manifest_multi_release_case_insensitive() {
        let data = b"Multi-Release: TRUE\n";
        let info = ManifestInfo::parse(data);
        assert!(info.multi_release);
    }

    // -- Multi-release JAR tests --

    #[test]
    fn multi_release_jar_prefers_versioned_class() {
        let dir = std::env::temp_dir().join("rustjvm_test_mr_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // MANIFEST.MF with Multi-Release
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Base class
        zip.start_file("com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        // Versioned class (Java 11)
        zip.start_file("META-INF/versions/11/com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v11").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Hello").unwrap();
        // Should get the versioned class (v11), not the base.
        assert_eq!(&data[4..], b"_v11");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_release_jar_highest_version_wins() {
        let dir = std::env::temp_dir().join("rustjvm_test_mr_jar_highest");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr2.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Base class
        zip.start_file("com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        // Java 11 version
        zip.start_file("META-INF/versions/11/com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v11").unwrap();

        // Java 17 version
        zip.start_file("META-INF/versions/17/com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v17").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Foo").unwrap();
        // Should get the highest available version (v17), since our VM targets Java 25.
        assert_eq!(&data[4..], b"_v17");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_multi_release_jar_ignores_versioned() {
        let dir = std::env::temp_dir().join("rustjvm_test_no_mr");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("no_mr.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // No Multi-Release header
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Main-Class: com.example.Main\n").unwrap();

        zip.start_file("com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        zip.start_file("META-INF/versions/17/com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v17").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Hello").unwrap();
        // Without Multi-Release flag, should get the base version.
        assert_eq!(&data[4..], b"_base");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_release_jar_falls_back_to_base() {
        let dir = std::env::temp_dir().join("rustjvm_test_mr_fallback");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr_fallback.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Only base class (no versioned entry)
        zip.start_file("com/example/Base.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base_only").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Base").unwrap();
        assert_eq!(&data[4..], b"_base_only");

        let _ = fs::remove_dir_all(&dir);
    }

    // =======================================================================
    // NEW-5 — jimage integration
    // =======================================================================

    /// Produce a temp-file path unique to this test run so concurrent
    /// tests don't collide.
    fn jimage_temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("rustjvm_jimage_test");
        let _ = fs::create_dir_all(&dir);
        dir.join(format!(
            "{tag}_{}.img",
            std::process::id().wrapping_mul(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0))
        ))
    }

    fn sample_jimage_resources(
    ) -> Vec<(&'static str, &'static str, &'static str, &'static str, &'static [u8])> {
        vec![
            (
                "java.base",
                "java/lang",
                "String",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_string".as_ref(),
            ),
            (
                "java.base",
                "java/util",
                "HashMap",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_hashmap".as_ref(),
            ),
            (
                "java.desktop",
                "javax/swing",
                "JFrame",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_jframe".as_ref(),
            ),
            (
                "java.base",
                "",
                "module-info",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_base_module_info".as_ref(),
            ),
            (
                "java.desktop",
                "",
                "module-info",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_desktop_module_info".as_ref(),
            ),
        ]
    }

    /// Write a synthetic jimage to disk and load it through
    /// `ClassPath::add_jimage`. The resulting classpath must resolve
    /// every class via `find_class`.
    #[test]
    fn jimage_find_class_round_trip() {
        let data = rustjvm_reader::jimage::test_builder::build_simple(
            &sample_jimage_resources(),
        );
        let path = jimage_temp_path("find_class");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).expect("add_jimage");

        // `module-info` is scoped per-module and not resolvable via
        // the flat `find_class(name)` API (see `load_jimage`). All
        // other sample classes round-trip exactly.
        for (_module, parent, base, _ext, bytes) in sample_jimage_resources() {
            if parent.is_empty() && base == "module-info" {
                continue;
            }
            let internal = format!("{parent}/{base}");
            let got = cp.find_class(&internal).expect("class present");
            assert_eq!(
                got.as_slice(),
                bytes,
                "bytes for {internal} must round-trip"
            );
        }

        let _ = fs::remove_file(&path);
    }

    /// The `lib/modules` auto-detection in `ClassPath::new` should pick
    /// up a file literally named `modules` without an extension.
    #[test]
    fn jimage_auto_detected_by_filename() {
        let data = rustjvm_reader::jimage::test_builder::build_simple(
            &sample_jimage_resources(),
        );
        let dir = std::env::temp_dir()
            .join(format!("rustjvm_jimage_auto_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("modules");
        fs::write(&path, &data).unwrap();

        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
        assert!(
            !cp.is_empty(),
            "ClassPath::new should auto-detect a file named `modules`"
        );
        let bytes = cp
            .find_class("java/lang/String")
            .expect("class present via auto-detected jimage");
        assert!(bytes.starts_with(b"\xCA\xFE\xBA\xBE"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `scan_module_infos` should return a module-info entry for every
    /// distinct module in the jimage.
    #[test]
    fn jimage_scan_module_infos_returns_every_module() {
        let data = rustjvm_reader::jimage::test_builder::build_simple(
            &sample_jimage_resources(),
        );
        let path = jimage_temp_path("scan_module_infos");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let infos = cp.scan_module_infos();
        assert_eq!(
            infos.len(),
            2,
            "exactly two modules in the sample jimage ({} found)",
            infos.len()
        );
        // Both should start with the class-file magic. The exact bytes
        // reflect what our sample resources wrote.
        for info in &infos {
            assert!(info.starts_with(b"\xCA\xFE\xBA\xBE"));
        }
        let _ = fs::remove_file(&path);
    }

    /// The class-to-module index exposed via `jimage_class_count` must
    /// reflect every `.class` entry in the image (module-infos count).
    #[test]
    fn jimage_class_count_matches_resources() {
        let data = rustjvm_reader::jimage::test_builder::build_simple(
            &sample_jimage_resources(),
        );
        let path = jimage_temp_path("class_count");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        // 3 regular classes (module-info entries are not counted
        // because they're scoped per-module, not global).
        assert_eq!(cp.jimage_class_count(), 3);
        let modules = cp.list_jimage_modules();
        assert_eq!(modules, vec!["java.base".to_string(), "java.desktop".to_string()]);

        let _ = fs::remove_file(&path);
    }

    /// `list_class_names` should include every class in the jimage.
    #[test]
    fn jimage_list_class_names_includes_every_entry() {
        let data = rustjvm_reader::jimage::test_builder::build_simple(
            &sample_jimage_resources(),
        );
        let path = jimage_temp_path("list_class_names");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let mut names = cp.list_class_names();
        names.sort();
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(names.contains(&"java/util/HashMap".to_string()));
        assert!(names.contains(&"javax/swing/JFrame".to_string()));
        // module-info is deliberately excluded from `class_to_module`
        // because it is scoped per-module rather than globally unique.
        assert!(
            !names.contains(&"module-info".to_string()),
            "module-info must not appear as a regular class name"
        );

        let _ = fs::remove_file(&path);
    }

    /// Missing-class lookups against a jimage classpath must return
    /// ClassNotFound and not leak a panic.
    #[test]
    fn jimage_missing_class_returns_clean_error() {
        let data = rustjvm_reader::jimage::test_builder::build_simple(
            &sample_jimage_resources(),
        );
        let path = jimage_temp_path("missing");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let err = cp.find_class("no/such/Class").unwrap_err();
        assert!(matches!(err, ClassFileError::ClassNotFound { .. }));

        let _ = fs::remove_file(&path);
    }

    /// `add_jimage` on a non-existent path must return an error, not
    /// a silent no-op.
    #[test]
    fn jimage_add_nonexistent_returns_error() {
        let mut cp = ClassPath::new(&[]);
        let err = cp
            .add_jimage("/nonexistent/rustjvm/jimage/file")
            .unwrap_err();
        assert!(
            err.contains("open jimage"),
            "error message should mention open failure: {err}"
        );
    }

    /// `add_jimage` on a bogus file (not a valid jimage) must return
    /// an error without corrupting the classpath.
    #[test]
    fn jimage_add_bogus_file_returns_error() {
        let path = jimage_temp_path("bogus");
        fs::write(&path, b"this is not a jimage file").unwrap();

        let mut cp = ClassPath::new(&[]);
        let err = cp.add_jimage(&path).unwrap_err();
        assert!(
            err.contains("open jimage") || err.contains("jimage"),
            "error should reference jimage: {err}"
        );
        assert!(cp.is_empty(), "classpath must be unchanged on failure");

        let _ = fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // T19_H10: ManifestInfo.attributes — generic attribute capture so
    // `Class.getPackage().getImplementationVersion()` and friends can read
    // arbitrary `Implementation-*` / `Specification-*` headers without
    // expanding `ManifestInfo` for every new field.
    // -----------------------------------------------------------------------

    #[test]
    fn t19_h10_manifest_attributes_captures_implementation_fields() {
        // Use \n-only line separators (no continuation prefix) so the
        // parser captures every attribute as its own entry.
        let manifest = b"Manifest-Version: 1.0\n\
Implementation-Title: keycloak-common\n\
Implementation-Version: 26.2.4\n\
Implementation-Vendor: Red Hat, Inc.\n\
Specification-Title: Keycloak Common\n\
Specification-Version: 26.2\n\
Specification-Vendor: Keycloak\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(info.attributes.get("Implementation-Title").map(String::as_str), Some("keycloak-common"));
        assert_eq!(info.attributes.get("Implementation-Version").map(String::as_str), Some("26.2.4"));
        assert_eq!(info.attributes.get("Implementation-Vendor").map(String::as_str), Some("Red Hat, Inc."));
        assert_eq!(info.attributes.get("Specification-Title").map(String::as_str), Some("Keycloak Common"));
        assert_eq!(info.attributes.get("Specification-Version").map(String::as_str), Some("26.2"));
        assert_eq!(info.attributes.get("Specification-Vendor").map(String::as_str), Some("Keycloak"));
    }

    #[test]
    fn t19_h10_manifest_attributes_returns_none_for_missing() {
        let manifest = b"Manifest-Version: 1.0\nMain-Class: foo.Bar\n";
        let info = ManifestInfo::parse(manifest);
        assert!(info.attributes.get("Implementation-Version").is_none(),
            "absent attribute must yield None");
        assert_eq!(info.attributes.get("Manifest-Version").map(String::as_str), Some("1.0"));
    }

    #[test]
    fn t19_h10_manifest_attributes_skips_per_entry_sections() {
        // The `\n\n` separator ends the main section. Anything after must
        // not contaminate `info.attributes`.
        let manifest = b"Manifest-Version: 1.0\n\
Implementation-Version: 1.0\n\
\n\
Name: foo/Bar.class\n\
Implementation-Version: 999.999\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(info.attributes.get("Implementation-Version").map(String::as_str), Some("1.0"),
            "main-section attribute wins; per-entry section is ignored");
        assert!(info.attributes.get("Name").is_none(),
            "per-entry `Name` header must not leak into main attributes");
    }

    #[test]
    fn t19_h10_manifest_attributes_caps_oversized_value() {
        // Build a manifest with one valid line + one 16 KiB-value line.
        // The big line must be silently dropped (returned as None) so a
        // hostile signed jar cannot blow our heap.
        let big = "X".repeat(16 * 1024);
        let raw = format!("Manifest-Version: 1.0\nFoo: {big}\nBar: ok\n");
        let info = ManifestInfo::parse(raw.as_bytes());
        assert!(info.attributes.get("Foo").is_none(),
            "oversized value must be dropped");
        assert_eq!(info.attributes.get("Bar").map(String::as_str), Some("ok"),
            "subsequent attributes must still be captured");
    }
}
