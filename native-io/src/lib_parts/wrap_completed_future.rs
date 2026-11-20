#[allow(unused_imports)]
use super::super::*;

pub(crate) fn wrap_completed_future(
    ctx: &mut dyn NativeContext,
    value: Value,
) -> Result<Value, MethodCallFailed> {
    // `value` is used again on the fallback path below, and BOTH the attempt
    // and the fallback allocation run code that can collect — a moving young GC
    // there would relocate it (the native stale-local family). Pin across the
    // whole body and re-read at the point of use, exactly as
    // `completed_executor_future` does for its result.
    let value_obj = match value {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let value_pin = value_obj.map(|o| ctx.pin_native_root(o));
    let real = ctx.invoke(
        "java/util/concurrent/CompletableFuture",
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        &[value],
    );
    let outcome = match real {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(v),
        // Fallback only — see the doc comment. Layout: [0] = result, [1] = done.
        _ => match try_alloc_synthetic(ctx, "java/util/concurrent/CompletedFuture", 2) {
            Ok(future) => {
                let value = match (value_pin, value_obj) {
                    (Some(pin), Some(o)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
                    _ => value,
                };
                ctx.set_field(future, 0, value);
                ctx.set_field(future, 1, Value::Int(1)); // done
                Ok(Value::Object(Some(future)))
            }
            Err(e) => Err(e),
        },
    };
    if let Some(pin) = value_pin {
        ctx.unpin_native_roots(pin);
    }
    outcome
}

pub(crate) fn native_completed_future_cancel(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_completed_future_is_cancelled(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_completed_future_is_done(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_completed_future_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))
}

pub(crate) fn afc_io_exception(
    ctx: &mut dyn NativeContext,
    message: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let msg = ctx.create_string(message);
    let msg_root = ctx.add_global_root(msg);
    let msg_now = ctx.resolve_global_root(msg_root).unwrap_or(msg);
    let constructed = cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/io/IOException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg_now))],
    );

    let exc = match constructed {
        Ok(Some(exc)) => exc,
        Err(thrown) => {
            let _ = ctx.remove_global_root(msg_root);
            return Err(thrown);
        }
        Ok(None) => {
            let exc = try_alloc_synthetic(ctx, "java/io/IOException", 2)?;
            let msg_now = ctx.resolve_global_root(msg_root).unwrap_or(msg);
            ctx.set_field_by_name(exc, "detailMessage", Value::Object(Some(msg_now)));
            exc
        }
    };
    let _ = ctx.remove_global_root(msg_root);
    Ok(exc)
}


pub(crate) type NotifyResult = Result<notify::Event, notify::Error>;

pub(crate) struct WatchServiceState {
    pub(crate) watcher: notify::RecommendedWatcher,
    pub(crate) rx: mpsc::Receiver<NotifyResult>,
    /// Canonicalized path → accumulated events since last drain.
    pub(crate) queued: HashMap<PathBuf, Vec<(i32, String)>>,
    /// Registered paths for this service (for `close()` fan-out).
    pub(crate) registered: Vec<PathBuf>,
    /// The `WatchService` object's address (see [`IoWeakRow`]), gc-common w15-c.
    pub(crate) owner: usize,
}

impl IoWeakRow for WatchServiceState {
    fn owner(&self) -> usize {
        self.owner
    }
    fn set_owner(&mut self, owner: usize) {
        self.owner = owner;
    }
}

impl WatchServiceState {
    /// A fresh platform watcher and its event channel, owned by the object at
    /// `owner`. An `IOException` when the platform refuses a watcher.
    pub(crate) fn open(owner: usize) -> Result<Self, MethodCallFailed> {
        let (tx, rx) = mpsc::channel::<NotifyResult>();
        let watcher = notify::RecommendedWatcher::new(
            move |res: NotifyResult| {
                // Silently drop on disconnect — the watch service is closing.
                let _ = tx.send(res);
            },
            notify::Config::default(),
        )
        .map_err(|e| RuntimeError::IOException {
            message: format!("WatchService: platform watcher init: {e}"),
        })?;
        Ok(Self {
            watcher,
            rx,
            queued: HashMap::new(),
            registered: Vec::new(),
            owner,
        })
    }
}

// GC-stable-key-fix (sibling of br_buf_table / isr_pending): the live
// platform watcher + event queues are cross-call state keyed by the
// WatchService object. The raw `as_ptr() as usize` address is NOT stable —
// a moving young-gen GC relocates the WatchService between `register()` /
// `poll()` / `take()` / `close()` calls, after which the key no longer
// finds the watcher (events vanish, close() leaks the watcher thread). Key
// on the header-stable identity-hash instead.
//
// gc-common w15-c (`common-w14e-reader-and-watch-side-tables-keyed-by-bare-identity-hash`):
// the key is `(vm_identity, identity_hash_code)` ([`io_side_key`]), so one VM's
// `close()` (or `newWatchService()`, which REPLACED the row) can no longer tear
// down another VM's watcher; and each row names its service (`owner`), so
// `gc_sweep_io_side_tables` closes the watcher of a service that died without
// `close()` (its thread and OS handles used to live as long as the process),
// and `forget_vm_io_side_tables` closes a disposed VM's. An unreachable
// service is unobservable: its keys hold no reference back to it and deliver
// events only through its `poll`/`take`.
//
// gc-common w29-c: keyed by the full [`IoRowKey`] (bucket plus the service's
// current address). A second live service with the same identity hash used
// to REPLACE the first one's watcher at `newWatchService()` (closing it), and
// either one's `close()` tore down the other's.
pub(crate) static WATCH_SERVICES: OnceLock<Mutex<HashMap<IoRowKey, WatchServiceState>>> = OnceLock::new();

pub(crate) fn watch_services() -> &'static Mutex<HashMap<IoRowKey, WatchServiceState>> {
    WATCH_SERVICES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Release the platform watchers of rows a sweep, a teardown or `close()` took
/// out of [`WATCH_SERVICES`]. Called with NO table lock held: dropping a
/// watcher signals (and on macOS joins) its event thread.
///
/// A watcher's `Drop` can panic (`notify`'s inotify/kqueue backends `unwrap`
/// the send to their event loop). From the collector's epilogue that panic
/// would unwind through a stop-the-world pause, so each drop is contained and
/// logged instead. The worst case is a watcher thread left running, which is
/// what used to happen to every unclosed service anyway.
pub(crate) fn close_dropped_watch_services(rows: Vec<WatchServiceState>) {
    for row in rows {
        let closed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(row)));
        if closed.is_err() {
            tracing::warn!(
                target: "cratonvm::native::watch",
                "WatchService: releasing a platform watcher panicked; its thread may outlive it",
            );
        }
    }
}

/// Normalize a path the same way we do on registration so lookups match.
pub(crate) fn normalize_watch_path(p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    fs::canonicalize(&pb).unwrap_or(pb)
}

/// Convert a notify `EventKind` to one of our `EVENT_CREATE`/`EVENT_MODIFY`/
/// `EVENT_DELETE` constants, or `None` for events we don't surface.
pub(crate) fn classify_event_kind(kind: &EventKind) -> Option<i32> {
    match kind {
        EventKind::Create(CreateKind::File)
        | EventKind::Create(CreateKind::Folder)
        | EventKind::Create(CreateKind::Any) => Some(EVENT_CREATE),
        EventKind::Remove(RemoveKind::File)
        | EventKind::Remove(RemoveKind::Folder)
        | EventKind::Remove(RemoveKind::Any) => Some(EVENT_DELETE),
        EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Modify(ModifyKind::Metadata(_))
        | EventKind::Modify(ModifyKind::Name(_))
        | EventKind::Modify(ModifyKind::Any) => Some(EVENT_MODIFY),
        _ => None,
    }
}

/// Pull every currently-available event out of the notify receiver and
/// partition it into the per-path queues. Called lazily on `poll`/`take`.
pub(crate) fn drain_into_queues(state: &mut WatchServiceState) {
    loop {
        match state.rx.try_recv() {
            Ok(Ok(event)) => {
                let kind = match classify_event_kind(&event.kind) {
                    Some(k) => k,
                    None => continue,
                };
                for path in &event.paths {
                    // Attribute the event to the watched *directory*; the
                    // Java-visible "context" of the event is the file's
                    // basename. `notify` reports absolute paths.
                    let parent = path.parent().map(PathBuf::from).unwrap_or_default();
                    let parent = fs::canonicalize(&parent).unwrap_or(parent);
                    // A registration for the parent directory collects the
                    // file's basename; a registration for the path itself
                    // collects its own basename.
                    let target = if state.registered.iter().any(|p| p == &parent) {
                        Some(parent)
                    } else if state.registered.iter().any(|p| p == path) {
                        Some(path.clone())
                    } else {
                        None
                    };
                    if let Some(t) = target {
                        let name = path
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        state.queued.entry(t).or_default().push((kind, name));
                    }
                }
            }
            Ok(Err(_)) | Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => {
                break;
            }
        }
    }
}

/// SBR wave 5, lane fs2: run the receiver's own bytecode for `method`, with
/// the same re-entrancy guard `native-builtins`' `yield_to_retired_receiver`
/// uses (a nested entry for the same method runs the native instead).
pub(crate) fn watch_yield_to_bytecode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    thread_local! {
        static YIELDING: std::cell::RefCell<Vec<(String, String)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    let nested = YIELDING.with(|y| {
        y.borrow()
            .iter()
            .any(|(m, d)| m == method && d == descriptor)
    });
    if nested {
        return None;
    }
    YIELDING.with(|y| {
        y.borrow_mut()
            .push((method.to_string(), descriptor.to_string()))
    });
    let result = ctx.invoke_virtual_bytecode_only(this, method, descriptor, &args[1..]);
    YIELDING.with(|y| {
        y.borrow_mut().pop();
    });
    Some(result)
}

/// SBR wave 5, lane fs2: under `--real-jdk`, a `newWatchService` /
/// `Path.register` call whose RECEIVER's triple is retired
/// (`LinuxFileSystem.newWatchService`, `UnixPath.register` x2 in
/// `RETIRED_SHADOW_SBR_FS2_TRIPLES`) runs that receiver's bytecode even when
/// it arrived through the interface rows `java/nio/file/FileSystem` /
/// `java/nio/file/Path`, which bind these same callbacks and are not shadows
/// (an interface method has no body to prefer). The same question
/// `native-builtins`' `yield_to_retired_receiver` asks for the provider rows;
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` flips it with the table.
pub(crate) fn watch_yield_if_retired(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    if !ctx.is_jdk_only() {
        return None;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let class = ctx.class_name_arc_of_id(ctx.class_id_of_object(this))?;
    if !cratonvm_native_api::retired_shadow::triple_is_retired_shadow(&class, method, descriptor) {
        return None;
    }
    watch_yield_to_bytecode(ctx, this, args, method, descriptor)
}

/// SBR wave 5, lane fs2: under `--real-jdk`, a `WatchService` / `WatchKey` /
/// `WatchEvent` receiver that this family did NOT mint -- one built by the
/// JDK's own constructors (the real `LinuxWatchService`, its `LinuxWatchKey`s
/// and `AbstractWatchKey.Event`s once the Linux shadows are retired) -- runs
/// its own bytecode. The natives below read a private slot map appended above
/// the class's declared fields (`watch_base`); a real object has no such map,
/// so a native reading one would decode the JDK's own fields (`watcher`,
/// `dir`, `state`, ...) as this VM's slots.
///
/// Decided per OBJECT, not per class: `AbstractWatchKey$Event`,
/// `AbstractWatchService` and `AbstractWatchKey` stay registered (a Windows
/// `--real-jdk` VM still mints its watch objects on those classes), so the
/// question "is this one of ours" is the only one both platforms answer
/// correctly. A minted object carries `width` slots above its class's
/// declared fields; a constructed one carries none.
pub(crate) fn watch_yield_if_real(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    width: usize,
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    if !ctx.is_jdk_only() {
        return None;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let cid = ctx.class_id_of_object(this);
    let base = cratonvm_native_api::appended_slots::base_for_class_id(&*ctx, cid);
    if base == 0 || ctx.object_num_fields(this) >= base + width {
        // Ours: a synthetic interface-stamped object (no declared fields) or a
        // concrete mint with the slot map appended.
        return None;
    }
    watch_yield_to_bytecode(ctx, this, args, method, descriptor)
}

/// The kind a row of the watch family carries on `class` (see [`register_watch_service`]): `Intrinsic` for the
/// interface rows and the Windows classes of a Windows build, `Bridge` for the rows mirrored onto another
/// platform's classes (`LinuxWatchService`, ...), which are inert there and which `--real-jdk` retires.
pub(crate) fn watch_family_kind(class: &str) -> cratonvm_native_api::NativeKind {
    let other_platform = ["Linux", "Unix", "Bsd", "Polling", "Mac"].iter().any(|p| class.contains(p));
    if cfg!(windows) && !other_platform {
        cratonvm_native_api::NativeKind::Intrinsic
    } else {
        cratonvm_native_api::NativeKind::Bridge
    }
}

/// `concrete_receiver::mirror_class_registrations` with the kind [`watch_family_kind`] gives `to`.
pub(crate) fn mirror_watch_family(r: &mut NativeMethodRegistry, since: usize, from: &str, to: &str) {
    let rows: Vec<(String, String)> = r
        .registrations_since(since)
        .filter(|(c, _, _, _)| *c == from)
        .map(|(_, m, d, _)| (m.to_string(), d.to_string()))
        .collect();
    for (method, descriptor) in rows {
        let Some(id) = r.resolve_id(from, &method, &descriptor) else {
            continue;
        };
        let Some(callback) = r.callback_of(id) else {
            continue;
        };
        r.register(to, &method, &descriptor, callback, watch_family_kind(to));
    }
}

pub(crate) fn register_watch_service(r: &mut NativeMethodRegistry) {
    // Windows: the whole family is the VM's own WatchService model (a notify-backed service minted without
    // running `WindowsWatchService`'s constructor), and the JDK bodies over it are the IOCP /
    // `ReadDirectoryChangesW` poller thread this VM has no natives for (`RETIRED_SHADOW_L4_WINFS_TRIPLES`
    // names the blocker: `newWatchService` and `register` are held out of the retire tables on purpose).
    // Since the FFM merge concrete JDK bytecode beats a `Bridge`, so `WindowsFileSystem.newWatchService()`
    // ran the real constructor and Spring Boot's `FileWatcher` failed 15/15. A reviewed `Intrinsic`, every
    // row of the family (the mirrors below copy the kind), so the minted objects and their methods stay one
    // model. Linux runs the JDK's own `LinuxWatchService` (retired shadows) and keeps the `Bridge`.
        // Where this registrar's rows start, so the mirrors at its foot cannot see
    // another crate's. See `concrete_receiver::mirror_class_registrations`.
    let __rows_before = r.registration_count();
    let ws = "java/nio/file/WatchService";

    // FileSystems.getDefault().newWatchService() → WatchService
    r.register(
        "java/nio/file/FileSystem",
        "newWatchService",
        "()Ljava/nio/file/WatchService;",
        native_ws_new, watch_family_kind("java/nio/file/FileSystem"),
    );

    // Path.register(WatchService, WatchEvent.Kind...) → WatchKey
    r.register(
        "java/nio/file/Path",
        "register",
        "(Ljava/nio/file/WatchService;[Ljava/nio/file/WatchEvent$Kind;)Ljava/nio/file/WatchKey;",
        native_ws_register, watch_family_kind("java/nio/file/FileSystem"),
    );
    // `Watchable.register(WatchService, Kind[], Modifier[])` is the interface's
    // primitive; the 2-arg form above is a default that delegates to it. Code
    // that calls the 3-arg form directly (or reaches it through a `Watchable`-
    // typed reference) must land on the same implementation instead of the
    // interface's abstract body. The modifiers array is accepted and ignored —
    // the only standard modifiers are `com.sun.nio.file.*` extensions.
    r.register(
        "java/nio/file/Path",
        "register",
        "(Ljava/nio/file/WatchService;[Ljava/nio/file/WatchEvent$Kind;[Ljava/nio/file/WatchEvent$Modifier;)Ljava/nio/file/WatchKey;",
        native_ws_register, watch_family_kind("java/nio/file/FileSystem"),
    );

    // WatchService.poll() → WatchKey (or null)
    r.register(ws, "poll", "()Ljava/nio/file/WatchKey;", native_ws_poll, watch_family_kind("java/nio/file/FileSystem"));

    // WatchService.poll(long, TimeUnit) → WatchKey (or null after timeout).
    //
    // This overload used to be missing entirely. `java.nio.file.WatchService`
    // is an interface, so a call to it fell through to the abstract
    // declaration and threw `AbstractMethodError: ... has no Code attribute`
    // — fatal for the canonical watch loop, which is written as
    // `watchService.poll(quietPeriod, MILLISECONDS)` precisely so it can do
    // periodic work while idle (Spring Boot's SSL bundle `FileWatcher`).
    r.register(
        ws,
        "poll",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey;",
        native_ws_poll_timed, watch_family_kind("java/nio/file/FileSystem"),
    );

    // WatchService.take() → WatchKey (blocking)
    r.register(ws, "take", "()Ljava/nio/file/WatchKey;", native_ws_take, watch_family_kind("java/nio/file/FileSystem"));

    // WatchService.close() → void
    r.register(ws, "close", "()V", native_ws_close, watch_family_kind("java/nio/file/FileSystem"));

    // WatchKey.pollEvents() → List<WatchEvent>
    r.register(
        "java/nio/file/WatchKey",
        "pollEvents",
        "()Ljava/util/List;",
        native_wk_poll_events, watch_family_kind("java/nio/file/FileSystem"),
    );

    // WatchKey.reset() → boolean
    r.register("java/nio/file/WatchKey", "reset", "()Z", native_wk_reset, watch_family_kind("java/nio/file/FileSystem"));

    // WatchKey.cancel() → void
    r.register("java/nio/file/WatchKey", "cancel", "()V", native_wk_cancel, watch_family_kind("java/nio/file/FileSystem"));

    // WatchKey.isValid() → boolean
    r.register(
        "java/nio/file/WatchKey",
        "isValid",
        "()Z",
        native_wk_is_valid, watch_family_kind("java/nio/file/FileSystem"),
    );

    // WatchKey.watchable() → Watchable (the Path that was registered)
    r.register(
        "java/nio/file/WatchKey",
        "watchable",
        "()Ljava/nio/file/Watchable;",
        native_wk_watchable, watch_family_kind("java/nio/file/FileSystem"),
    );

    // WatchEvent.kind() → WatchEvent.Kind
    r.register(
        "java/nio/file/WatchEvent",
        "kind",
        "()Ljava/nio/file/WatchEvent$Kind;",
        native_we_kind, watch_family_kind("java/nio/file/FileSystem"),
    );

    // WatchEvent.context() → Object (Path)
    r.register(
        "java/nio/file/WatchEvent",
        "context",
        "()Ljava/lang/Object;",
        native_we_context, watch_family_kind("java/nio/file/FileSystem"),
    );

    // WatchEvent.count() → int. Every event we surface is reported once; we
    // never coalesce repeats into a single event with count > 1.
    r.register("java/nio/file/WatchEvent", "count", "()I", |ctx, args| {
        watch_real_yield!(ctx, args, WE_NUM_FIELDS, "count", "()I");
        Ok(Some(Value::Int(1)))
    }, watch_family_kind("java/nio/file/FileSystem"));

    // The registration half of the three fabricated-receiver fixes in this
    // family (`native_ws_new`, `native_ws_register`, `native_ws_poll_inner`).
    //
    // Dispatch keys on the receiver's runtime class (`H11-1`), so once the
    // three mints name the concrete `sun.nio.fs.*` classes, the interface rows
    // above stop being reachable for them. Both halves have to move together
    // or the family simply goes quiet -- and quietly, because an interface row
    // that nothing reaches looks exactly like an interface row that nothing
    // needs.
    //
    // `AbstractWatchService` and `AbstractWatchKey` are in the lists because
    // they are where the concrete classes inherit `poll`/`take`/`close` and
    // `pollEvents`/`reset`/`watchable` FROM -- all `final`, all with `Code`.
    // A registration on the concrete class wins at step 1; these are the belt
    // to that pair of braces, and cost nothing when the concrete row answers.
    for target in WS_IMPLS
        .iter()
        .chain(["sun/nio/fs/AbstractWatchService"].iter())
    {
        mirror_watch_family(r, __rows_before, ws, target);
    }
    // `newWatchService` above is the SAME species again, one carrier further
    // out: registered under the interface `java/nio/file/FileSystem`, and
    // invisible to a concrete `WindowsFileSystem`/`UnixFileSystem` receiver
    // the moment `p57_alloc_default_filesystem` stamps one (2026-09-17 --
    // see `cratonvm_native_api::filesystem_layout`'s doc). No mirror loop in
    // THIS function's own `native-builtins` sibling
    // (`register_phase57_nio_file`) can reach it either: that one mirrors
    // rows registered inside itself, and this row is registered here, in a
    // different crate, by a registrar `native-builtins` cannot call into.
    // Harmless when `filesystem_layout::impl_class()` is never the
    // receiver's actual stamp (non-Windows, or a platform/image the layout
    // resolver refused) -- the mirrored row simply sits on a class name
    // nothing ever mints under.
    mirror_watch_family(
        r,
        __rows_before,
        "java/nio/file/FileSystem",
        cratonvm_native_api::filesystem_layout::impl_class(),
    );
    // `filesystem_layout::resolve` refuses unconditionally on non-Windows
    // (its own doc: unverified field layout), so on Linux the mirror above
    // targets a class name (`sun/nio/fs/UnixFileSystem`) nothing ever mints
    // under -- same "harmless" shape the comment above already describes.
    // But `--real-jdk` on Linux does NOT fall back to the synthetic
    // `java/nio/file/FileSystem` stub the interface registration above would
    // still reach: `p57_alloc_default_filesystem`'s `None` arm takes
    // `p57_alloc_default_filesystem_unix` there, which mints the REAL
    // `sun/nio/fs/LinuxFileSystem` by running its own constructor. That
    // receiver's exact class matches neither the interface registration nor
    // the `impl_class()` mirror, so `newWatchService` went unreached and real
    // `LinuxFileSystem.newWatchService()` bytecode ran instead, straight into
    // `LinuxWatchService`'s real constructor and the `inotifyInit()` native
    // nothing has implemented since `register_watch_service_real` was
    // retired (`watch.rs`'s own account) -- `UnsatisfiedLinkError` at
    // `sun/nio/fs/LinuxWatchService.inotifyInit()I`, not the earlier
    // `NoClassDefFoundError`/`InstantiationError` species this whole family
    // was built to close. MEASURED: `--dump-native-registry` under
    // `--real-jdk` shows `newWatchService` registered on
    // `sun/nio/fs/UnixFileSystem` (0 invocations) and absent from
    // `sun/nio/fs/LinuxFileSystem` before this mirror; `RJdkWatchService`
    // threw the `UnsatisfiedLinkError` above before this fix and passes
    // after it.
    mirror_watch_family(
        r,
        __rows_before,
        "java/nio/file/FileSystem",
        "sun/nio/fs/LinuxFileSystem",
    );
    // The two `Path.register(...)` rows above are the SAME species this
    // comment already names for `WatchService`/`WatchKey`/`WatchEvent` --
    // registered under the interface `java/nio/file/Path`, and invisible to a
    // concrete `WindowsPath`/`UnixPath` receiver the moment `p57_alloc_path`
    // stamps one (`fix(nio): stamp java.nio.file.Path receivers as the real
    // platform class`, 2026-09-16). MEASURED, not theoretical:
    // `dir.register(ws, ENTRY_CREATE)` against a carrier-fixed build with this
    // line absent throws `NullPointerException:
    // WindowsWatchService$Poller.register(...) because "this.poller" is null`
    // from REAL `WindowsPath.register` bytecode -- the interface's own default
    // 2-arg body delegating to the 3-arg body, landing on a receiver whose
    // `poller` this VM never wires (a sibling gap to `path_layout`'s own
    // never-populated `fs` slot, not the same bug). Mirroring restores the
    // native, which builds a working synthetic `WatchKey` without touching
    // `WindowsWatchService`'s internals at all.
    mirror_watch_family(
        r,
        __rows_before,
        "java/nio/file/Path",
        cratonvm_native_api::path_layout::impl_class(),
    );
    for target in WK_IMPLS
        .iter()
        .chain(["sun/nio/fs/AbstractWatchKey"].iter())
    {
        mirror_watch_family(
            r,
            __rows_before,
            "java/nio/file/WatchKey",
            target,
        );
    }
    for target in WE_IMPLS {
        mirror_watch_family(
            r,
            __rows_before,
            "java/nio/file/WatchEvent",
            target,
        );
    }

    // NO `StandardWatchEventKinds` CONSTANTS ARE REGISTERED HERE, and none can
    // usefully be (E40-1 §2, answering E36-1 N5).
    //
    // What stood here until 2026-08-13 was a comment describing four
    // registrations plus a `let kinds = "java/nio/file/StandardWatchEventKinds";`
    // binding that nothing read — the mirror of E21-1 §2d's defect. That one
    // claimed a removal that had not happened; this one claimed a presence that
    // no longer existed, and cost the same reader-hour.
    //
    // The four rows the comment described live in
    // `native-builtins/src/phases_late/nio_file.rs` (`register_p66_watch_service`),
    // are field-shaped (`Ljava/nio/file/WatchEvent$Kind;` in the descriptor
    // slot), and are dead: `getstatic` never consults the native registry in
    // this VM, and `StandardWatchEventKinds.ENTRY_CREATE` compiles to a real
    // `getstatic` (measured). They are also type-wrong — each body answers a
    // `java/lang/String`. E36-1 §4d's verdict on them is DELETE.
    //
    // The reader of the constants is `watch_event_kind_object` below, and it
    // does not go through the registry at all: it reads the class's STATIC
    // (`static_field_index_by_name` + `get_static_field`), so in real-JDK mode
    // `event.kind() == StandardWatchEventKinds.ENTRY_CREATE` is an identity
    // comparison against the JDK's own
    // `StandardWatchEventKinds$StdWatchEventKind` singleton. Only when that
    // read comes back empty does it fall back to a synthetic one-field `Kind`
    // carrying the `EVENT_*` bit. `watch_event_kind_bit` accepts all three
    // shapes; `watch_event_kind_bit_translates_all_three_kind_shapes` and
    // `watch_event_kind_object_round_trips_through_the_bit` pin that.
    //
    }

// ---------------------------------------------------------------------------
// WatchEvent.Kind <-> EVENT_* bit translation.
//
// Two representations reach these natives:
//
//   * the REAL JDK `java.nio.file.StandardWatchEventKinds$StdWatchEventKind`
//     singletons (real-JDK mode — the overwhelmingly common case), whose
//     identity is what user code compares against and whose only useful
//     accessor is `name()`;
//   * the synthetic one-field `java/nio/file/WatchEvent$Kind` this file
//     allocates when the real class is unavailable, which carries the bit
//     directly in slot 0.
//
// `Path.register`'s kinds array used to be decoded by reading slot 0 as an
// `Int` unconditionally. On a real `StdWatchEventKind` slot 0 is the `name`
// String, so the decode yielded mask 0 and EVERY event was then filtered out
// by `detect_events` — a watch that registered successfully and reported
// nothing, forever.
// ---------------------------------------------------------------------------

/// Translate one `WatchEvent.Kind` object to an `EVENT_*` bit, or 0 if it is
/// not one of the three entry kinds we surface.
pub(crate) fn watch_event_kind_bit(ctx: &mut dyn NativeContext, kind: ObjectRef) -> i32 {
    // Synthetic kind: the bit lives in slot 0.
    if ctx.object_num_fields(kind) > 0 {
        if let Value::Int(k) = ctx.get_field(kind, 0) {
            if k & (EVENT_CREATE | EVENT_DELETE | EVENT_MODIFY) != 0 {
                return k;
            }
        }
    }
    // Real kind (or anything else implementing the interface): ask for the
    // name. Fall back to reading the receiver as a String for the legacy
    // synthetic surface that handed out bare name Strings.
    // GC: `name()` runs Java, and the `or_else` fallback reads `kind` back —
    // the closure runs AFTER that call returns. Not reported by the audit's
    // parameter rule, which asks whether the use is in a LATER statement, and
    // this one shares a statement with the call it follows.
    let pin = ctx.pin_native_root(kind);
    let name = match ctx.invoke_virtual(kind, "name", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    let kind = ctx.read_native_pin(pin, kind);
    let name = name.or_else(|| ctx.read_string(kind));
    ctx.unpin_native_roots(pin);
    match name.as_deref() {
        Some("ENTRY_CREATE") => EVENT_CREATE,
        Some("ENTRY_DELETE") => EVENT_DELETE,
        Some("ENTRY_MODIFY") => EVENT_MODIFY,
        _ => 0,
    }
}

/// The `WatchEvent.Kind` object for `bit` — the real JDK singleton when the
/// class is loadable, else a synthetic one-field stand-in.
pub(crate) fn watch_event_kind_object(
    ctx: &mut dyn NativeContext,
    bit: i32,
) -> Result<Value, MethodCallFailed> {
    let field = match bit {
        EVENT_CREATE => "ENTRY_CREATE",
        EVENT_DELETE => "ENTRY_DELETE",
        EVENT_MODIFY => "ENTRY_MODIFY",
        _ => "OVERFLOW",
    };
    if let Ok(cid) = ctx.ensure_class_initialized("java/nio/file/StandardWatchEventKinds") {
        if let Some(idx) = ctx.static_field_index_by_name(cid, field) {
            let v = ctx.get_static_field(cid, idx);
            if matches!(v, Value::Object(Some(_))) {
                return Ok(v);
            }
        }
    }
    let k = try_alloc_synthetic(ctx, "java/nio/file/WatchEvent$Kind", 1)?;
    ctx.set_field(k, 0, Value::Int(bit));
    Ok(Value::Object(Some(k)))
}

/// Build the `WatchEvent.context()` value for `name` (a basename) relative to
/// the directory `watchable` that the key was registered for.
///
/// The JDK's context is a *relative* `Path` naming the entry inside the
/// watched directory, and the canonical consumer does
/// `directory.resolve((Path) event.context())`. Deriving it from the caller's
/// own `Path` object (`dir.resolve(name).getFileName()`) keeps it the same
/// `Path` implementation the caller already holds — a synthetic stand-in
/// would blow up in that `resolve` with a `ClassCastException`.
pub(crate) fn watch_context_path(
    ctx: &mut dyn NativeContext,
    watchable: Option<ObjectRef>,
    name: &str,
) -> Result<Value, MethodCallFailed> {
    if let Some(dir) = watchable {
        // `create_string` allocates, so `dir` must be re-read afterwards.
        let dir_pin = ctx.pin_native_root(dir);
        let name_str = ctx.create_string(name);
        let name_pin = ctx.pin_native_root(name_str);
        let dir_now = ctx.read_native_pin(dir_pin, dir);
        let name_now = ctx.read_native_pin(name_pin, name_str);
        let resolved = ctx.invoke_virtual(
            dir_now,
            "resolve",
            "(Ljava/lang/String;)Ljava/nio/file/Path;",
            &[Value::Object(Some(name_now))],
        );
        let out = match resolved {
            Ok(Some(Value::Object(Some(abs)))) => {
                let abs_pin = ctx.pin_native_root(abs);
                let file_name =
                    ctx.invoke_virtual(abs, "getFileName", "()Ljava/nio/file/Path;", &[]);
                match file_name {
                    Ok(Some(v @ Value::Object(Some(_)))) => Some(v),
                    // `getFileName()` unavailable: hand back the absolute
                    // path. `Path.resolve(absolute)` returns the argument,
                    // so the consumer still computes the right file.
                    _ => Some(Value::Object(Some(ctx.read_native_pin(abs_pin, abs)))),
                }
            }
            _ => None,
        };
        ctx.unpin_native_roots(dir_pin);
        if let Some(v) = out {
            return Ok(v);
        }
    }
    // No watchable (or the real Path surface refused): fall back to a synthetic
    // Path. NOT the historical two-slot shape — `path_layout` decides where a
    // Path's String and FileSystem live, because `native-builtins` produces this
    // same carrier and the two must not disagree about it.
    Ok(Value::Object(Some(alloc_synthetic_path(ctx, name)?)))
}

/// Allocate a synthetic `java/nio/file/Path` carrying `text`.
///
/// **The slot map comes from `cratonvm_native_api::path_layout`, not from a
/// literal here.** A Path is stamped with the interface and reported by
/// `getClass()` as `sun.nio.fs.UnixPath` / `WindowsPath`, whose real layouts put
/// the path String at a different index than the two hard-coded constants this
/// crate and `native-builtins` both used to write. `native-builtins` produces
/// the same carrier; a second literal here is how the two would drift, which is
/// the whole reason the map lives in `native-api`.
pub(crate) fn alloc_synthetic_path(
    ctx: &mut dyn NativeContext,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let slots = cratonvm_native_api::path_layout::resolve(ctx);
    let path_obj = try_alloc_synthetic(ctx, "java/nio/file/Path", slots.width)?;
    let pin = ctx.pin_native_root(path_obj);
    let stored = cratonvm_native_api::path_layout::stored_form(text);
    let path_s = ctx.create_string(&stored);
    let path_obj = ctx.read_native_pin(pin, path_obj);
    ctx.set_field(path_obj, slots.string, Value::Object(Some(path_s)));
    // The encoded bytes as well as the String: real `UnixPath.compareTo` (so
    // `equals`), `hashCode` and `initOffsets` all read them, and publishing one
    // slot without the other turns a wrong answer into an NPE.
    if let Some(bytes_slot) = slots.bytes {
        let raw = stored.as_bytes();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, raw.len());
        ctx.write_byte_array_from(arr, 0, raw);
        let path_obj = ctx.read_native_pin(pin, path_obj);
        ctx.set_field(path_obj, bytes_slot, Value::Object(Some(arr)));
    }
    let path_obj = ctx.read_native_pin(pin, path_obj);
    ctx.unpin_native_roots(pin);
    Ok(path_obj)
}

/// `java.nio.file.ClosedWatchServiceException` — what the JDK throws from
/// `poll`/`take` on a closed service. A watch loop is written as
/// `catch (ClosedWatchServiceException ex) { running = false; }`, so throwing
/// anything else (this used to be an `IOException`) escapes the loop's own
/// shutdown handling and kills the thread with an uncaught exception instead.
///
/// Round 15 wave 3, lane jdkonly3: built by [`nio_native::construct_thrown`]
/// (rooted across `<init>`, the forwarded reference thrown), not the
/// `new_object` and `invoke` pair, which threw the pre-constructor address.
pub(crate) fn closed_watch_service_exception(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    nio_native::construct_thrown(
        ctx,
        "java/nio/file/ClosedWatchServiceException",
        "()V",
        &[],
        || {
            RuntimeError::IllegalStateException {
                message: "WatchService is closed".into(),
            }
            .into()
        },
    )
}

/// Pins `this` across [`ws_require_open_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `this` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub(crate) fn ws_require_open(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let w5_pin = ctx.pin_native_root(*this);
    let w5_out = ws_require_open_body(ctx, *this);
    *this = ctx.read_native_pin(w5_pin, *this);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// `Ok(())` while the service is open; the JDK's `ClosedWatchServiceException`
/// once `close()` has run.
pub(crate) fn ws_require_open_body(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if matches!(ws_get(ctx, this, WS_FIELD_OPEN), Value::Int(1)) {
        Ok(())
    } else {
        Err(closed_watch_service_exception(ctx))
    }
}

/// Create a real `notify::RecommendedWatcher` + sender→receiver pair and
/// remember it in `WATCH_SERVICES` keyed by the WatchService object's
/// stable address. If the platform watcher cannot be created we return
/// an IOException — the Java side treats WatchService setup as a
/// checked operation so throwing here is spec-compliant.
pub(crate) fn native_ws_new(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = watch_yield_if_retired(
        ctx,
        args,
        "newWatchService",
        "()Ljava/nio/file/WatchService;",
    ) {
        return r;
    }
    // `java.nio.file.WatchService` is an INTERFACE. Minting an instance of it
    // is an `InstantiationError` for `new` by JVMS 6.5, and this line did it in
    // both modes until 2026-08-21 -- MEASURED by `probes/W4Abstract.java`
    // against `sun.nio.fs.LinuxWatchService` on the oracle. See `WS_IMPLS`.
    let ws = crate::concrete_receiver::alloc_concrete(
        ctx,
        WS_IMPLS,
        "java/nio/file/WatchService",
        WS_NUM_FIELDS,
    )
    .obj;
    // GC: the registration array's allocation moves `ws`, which is then
    // stored through, identity-hashed as the watch-service table key, and
    // returned. A stale identity hash keys the table to an object nobody can
    // find again.
    let ws_pin = ctx.pin_native_root(ws);
    let regs = ctx.new_array(ArrayElementType::Reference, 64);
    let ws = ctx.read_native_pin(ws_pin, ws);
    ctx.unpin_native_roots(ws_pin);
    ws_set(ctx, ws, WS_FIELD_REGS, Value::Object(Some(regs)));
    ws_set(ctx, ws, WS_FIELD_COUNT, Value::Int(0));
    ws_set(ctx, ws, WS_FIELD_OPEN, Value::Int(1));

    // `ws` is current: nothing since its pin was re-read can move it.
    let state = WatchServiceState::open(io_side_owner(ws))?;
    ws_install(ctx, ws, state);
    Ok(Some(Value::Object(Some(ws))))
}

/// File `state` as `ws`'s platform watcher. A row already under the key (a
/// dead service's at the same address with the same identity hash that no
/// sweep has reached yet) is replaced, and its watcher closed once the lock is
/// released. `ws` must be CURRENT.
pub(crate) fn ws_install(ctx: &dyn NativeContext, ws: ObjectRef, state: WatchServiceState) {
    // GC-stable-key-fix: identity-hash, not the raw heap address; per VM since
    // gc-common w15-c; plus the current address since w29-c.
    let key = io_side_row_key(ctx, ws);
    let replaced = watch_services().lock().insert(key, state);
    close_dropped_watch_services(replaced.into_iter().collect());
}

pub(crate) fn native_ws_register(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let register_desc = if args.len() > 3 {
        "(Ljava/nio/file/WatchService;[Ljava/nio/file/WatchEvent$Kind;[Ljava/nio/file/WatchEvent$Modifier;)Ljava/nio/file/WatchKey;"
    } else {
        "(Ljava/nio/file/WatchService;[Ljava/nio/file/WatchEvent$Kind;)Ljava/nio/file/WatchKey;"
    };
    if let Some(r) = watch_yield_if_retired(ctx, args, "register", register_desc) {
        return r;
    }
    let path_obj = obj_arg92(args, 0)?;
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let path_obj_pin = ctx.pin_native_root(path_obj);
    let mut watcher = obj_arg92(args, 1)?;
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let watcher_pin = ctx.pin_native_root(watcher);
    // A CLOSED service refuses REGISTRATION too, not just `poll`/`take`, and it
    // refuses it with `ClosedWatchServiceException`.
    //
    // MEASURED, `regression-suite/probes/W4Watch.java`, Linux/JDK 25.0.4:
    //
    //     dir.register(closedWatchService, ENTRY_CREATE)
    //       HotSpot    java.nio.file.ClosedWatchServiceException
    //       CratonVM   java.io.IOException
    //
    // `ws_require_open` and `closed_watch_service_exception` already existed --
    // `poll` and `take` both call them, and both matched the oracle in the same
    // run. This entry point simply never called it, so the closed state was
    // discovered further down by whichever path check happened to fail and was
    // reported as an I/O error.
    //
    // The comment on `closed_watch_service_exception` says why the TYPE is
    // load-bearing: a watch loop is written as
    // `catch (ClosedWatchServiceException ex) { running = false; }`, so any
    // other type escapes the loop's own shutdown handling. A registration
    // racing a `close()` on another thread is exactly when that happens.
    //
    // Checked FIRST, matching `LinuxWatchService.register`, which runs
    // `checkOpen()` before it looks at the path at all -- so a closed service
    // and a missing directory report the closed service, on both VMs.
    ws_require_open(ctx, &mut watcher)?;
    let kinds_arr = match args.get(2) {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("WatchService.register: null kinds".into()),
            }
            .into())
        }
    };
    let path_obj = ctx.read_native_pin(path_obj_pin, path_obj);
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let path_obj_pin = ctx.pin_native_root(path_obj);

    // Read path string
    let path_str = ctx
        .read_string(path_obj)
        .or_else(|| path_slot_string(ctx, path_obj))
        .or_else(|| {
            // A real `WindowsPath` / `UnixPath` (the JDK's own provider ran):
            // its string form is `toString()`, whatever its field layout.
            match ctx.invoke_virtual(path_obj, "toString", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                _ => None,
            }
        })
        .unwrap_or_default();
    if path_str.is_empty() {
        return Err(RuntimeError::IOException {
            message: "WatchService.register: empty path".into(),
        }
        .into());
    }
    if !Path::new(&path_str).exists() {
        return Err(RuntimeError::IOException {
            message: format!("WatchService.register: no such file or directory: {path_str}"),
        }
        .into());
    }

    // MEASURED (Sweep18): a WatchService may only watch a DIRECTORY. HotSpot
    // throws `NotDirectoryException` for a regular file; this accepted it and
    // returned a live key that could never fire, which is the "fabricated
    // success" shape rather than a refusal.
    if !Path::new(&path_str).is_dir() {
        return Err(RuntimeError::NotDirectoryException {
            path: path_str.clone(),
        }
        .into());
    }

    // Read the event kind bitmask. `watch_event_kind_bit` handles both the
    // real `StandardWatchEventKinds` singletons and the synthetic stand-ins.
    let kinds_len = ctx.array_length(kinds_arr);
    // MEASURED (Sweep18): registering with NO event kinds is an
    // `IllegalArgumentException` on HotSpot — `AbstractWatchService.register`
    // rejects an empty set before it reaches the OS. This returned a valid key
    // for a watch that can never fire.
    if kinds_len == 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "no events to register".into(),
        }
        .into());
    }
    let mut event_mask = 0i32;
    for i in 0..kinds_len {
        if let Value::Object(Some(kind)) = ctx.get_array_element(kinds_arr, i) {
            event_mask |= watch_event_kind_bit(ctx, kind);
        }
    }
    if kinds_len > 0 && event_mask == 0 {
        // Every kind failed to decode. Registering with mask 0 is a watch
        // that can never fire, which is indistinguishable from "the OS never
        // reported anything" — surface it and watch everything instead.
        tracing::warn!(
            target: "cratonvm::native::watch",
            kinds_len,
            "WatchService.register: none of the requested WatchEvent.Kind values \
             could be decoded; watching CREATE|DELETE|MODIFY instead",
        );
        event_mask = EVENT_CREATE | EVENT_DELETE | EVENT_MODIFY;
    }

    // Install the OS-level watch on the real path. Non-recursive matches
    // java.nio.file.Path.register's documented semantics (the JDK's
    // default watch is on the directory itself, not its descendants).
    let canonical = normalize_watch_path(&path_str);
    {
        // GC-stable-key-fix: identity-hash, not the raw heap address; per VM
        // since gc-common w15-c. Key and owner are read BEFORE the table lock
        // is taken (they used to be read inside it).
        let watcher = ctx.read_native_pin(watcher_pin, watcher);
        let watcher_key = io_side_row_key(ctx, watcher);
        let mut services = watch_services().lock();
        let state = services
            .get_mut(&watcher_key)
            .ok_or_else(|| RuntimeError::IOException {
                message: "WatchService.register: service is closed or unknown".into(),
            })?;
        state
            .watcher
            .watch(&canonical, RecursiveMode::NonRecursive)
            .map_err(|e| RuntimeError::IOException {
                message: format!("WatchService.register: {e}"),
            })?;
        if !state.registered.iter().any(|p| p == &canonical) {
            state.registered.push(canonical.clone());
        }
    }

    let canonical_str = canonical.to_string_lossy().into_owned();

    // Re-registering a directory that already has a key on this service
    // REPLACES that key's event set and returns the SAME key — the JDK is
    // explicit about this ("If this path is already registered ... the event
    // set is replaced"). Minting a second key instead split one directory's
    // events across two keys: `detect_events` drains the shared per-path
    // queue, so whichever key was scanned first consumed everything and the
    // other never fired. Spring Boot's `FileWatcher` keys its
    // registration map on the WatchKey, so the duplicate key's callbacks
    // simply never ran (`shouldNotFailIfDirectoryIsRegisteredMultipleTimes`).
    let count = match ws_get(ctx, watcher, WS_FIELD_COUNT) {
        Value::Int(n) => n.max(0) as usize,
        _ => 0,
    };
    if let Value::Object(Some(regs)) = ws_get(ctx, watcher, WS_FIELD_REGS) {
        for i in 0..count.min(ctx.array_length(regs)) {
            let Value::Object(Some(existing)) = ctx.get_array_element(regs, i) else {
                continue;
            };
            let same_path = match wk_get(ctx, existing, WK_FIELD_PATH) {
                Value::Object(Some(s)) => {
                    ctx.read_string(s).as_deref() == Some(canonical_str.as_str())
                }
                _ => false,
            };
            if same_path {
                wk_set(ctx, existing, WK_FIELD_EVENTS, Value::Int(event_mask));
                wk_set(ctx, existing, WK_FIELD_VALID, Value::Int(1));
                let path_obj = ctx.read_native_pin(path_obj_pin, path_obj);
                wk_set(
                    ctx,
                    existing,
                    WK_FIELD_WATCHABLE,
                    Value::Object(Some(path_obj)),
                );
                return Ok(Some(Value::Object(Some(existing))));
            }
        }
    }

    // Create the WatchKey. We store the *canonicalized* path so lookups in
    // the poll path find the matching entry irrespective of how the caller
    // wrote the path, and the caller's own `Path` object so `watchable()`
    // hands back exactly what was registered.
    //
    // `create_string`/`alloc_synthetic` are GC points, so `path_obj` and
    // `watcher` are pinned and re-read: a moving young collection between the
    // allocations relocates them and a raw native local would then write the
    // key into freed memory (the Family-1 stale-ObjectRef defect).
    let path_obj_pin = ctx.pin_native_root(path_obj);
    let watcher_pin = ctx.pin_native_root(watcher);
    // Same defect, same shape: `java.nio.file.WatchKey` is an INTERFACE.
    let wk = crate::concrete_receiver::alloc_concrete(
        ctx,
        WK_IMPLS,
        "java/nio/file/WatchKey",
        WK_NUM_FIELDS,
    )
    .obj;
    let wk_pin = ctx.pin_native_root(wk);
    let path_s = ctx.create_string(&canonical_str);
    let wk = ctx.read_native_pin(wk_pin, wk);
    wk_set(ctx, wk, WK_FIELD_PATH, Value::Object(Some(path_s)));
    wk_set(ctx, wk, WK_FIELD_EVENTS, Value::Int(event_mask));
    wk_set(ctx, wk, WK_FIELD_VALID, Value::Int(1));
    wk_set(ctx, wk, WK_FIELD_PENDING, Value::Object(None));
    let path_obj_now = ctx.read_native_pin(path_obj_pin, path_obj);
    wk_set(
        ctx,
        wk,
        WK_FIELD_WATCHABLE,
        Value::Object(Some(path_obj_now)),
    );

    // Attach to the service's Java-side registration array.
    let watcher = ctx.read_native_pin(watcher_pin, watcher);
    if let Value::Object(Some(regs)) = ws_get(ctx, watcher, WS_FIELD_REGS) {
        if count < ctx.array_length(regs) {
            ctx.set_array_element(regs, count, Value::Object(Some(wk)));
            ws_set(ctx, watcher, WS_FIELD_COUNT, Value::Int((count + 1) as i32));
        }
    }
    let wk = ctx.read_native_pin(wk_pin, wk);
    ctx.unpin_native_roots(path_obj_pin);

    Ok(Some(Value::Object(Some(wk))))
}

/// Drain the notify channel for the WatchService that owns `wk`, then
/// consume every queued event that targets `wk`'s registered path. Only
/// events whose kind matches the mask the caller registered for are
/// returned; the rest are dropped.
pub(crate) fn detect_events(
    ctx: &mut dyn NativeContext,
    service: ObjectRef,
    wk: ObjectRef,
) -> Vec<(i32, String)> {
    let path_str = match wk_get(ctx, wk, WK_FIELD_PATH) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => return Vec::new(),
    };
    let event_mask = match wk_get(ctx, wk, WK_FIELD_EVENTS) {
        Value::Int(n) => n,
        _ => 0,
    };
    let canonical = normalize_watch_path(&path_str);

    // GC-stable-key-fix: identity-hash, not the raw heap address; per VM since
    // gc-common w15-c. `service` is current (the caller re-read it).
    let service_key = io_side_row_key(ctx, service);
    let mut services = watch_services().lock();
    let state = match services.get_mut(&service_key) {
        Some(s) => s,
        None => return Vec::new(),
    };
    drain_into_queues(state);
    let raw = state.queued.remove(&canonical).unwrap_or_default();
    raw.into_iter()
        .filter(|(k, _)| k & event_mask != 0)
        .collect()
}

/// Scan every registered key for pending OS events, materialize them into the
/// key's `pending` array, and return the first signalled key (`None` when the
/// service is idle).
///
/// Every allocation below (`new_array`, `alloc_synthetic`, `create_string`,
/// and the `invoke_virtual` inside `watch_context_path`) is a GC point, so the
/// service, its registration array, and the key under construction are pinned
/// and re-read across them rather than carried in raw native locals.
pub(crate) fn ws_signalled_key(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let count = match ws_get(ctx, this, WS_FIELD_COUNT) {
        Value::Int(n) => n.max(0) as usize,
        _ => 0,
    };
    let regs = match ws_get(ctx, this, WS_FIELD_REGS) {
        Value::Object(Some(a)) => a,
        _ => return Ok(None),
    };

    let this_pin = ctx.pin_native_root(this);
    let regs_pin = ctx.pin_native_root(regs);
    let mut signalled: Option<ObjectRef> = None;

    for i in 0..count {
        let regs_now = ctx.read_native_pin(regs_pin, regs);
        if i >= ctx.array_length(regs_now) {
            break;
        }
        let Value::Object(Some(wk)) = ctx.get_array_element(regs_now, i) else {
            continue;
        };
        if !matches!(wk_get(ctx, wk, WK_FIELD_VALID), Value::Int(1)) {
            continue;
        }
        let this_now = ctx.read_native_pin(this_pin, this);
        let events = detect_events(ctx, this_now, wk);
        if events.is_empty() {
            continue;
        }

        let wk_pin = ctx.pin_native_root(wk);
        let pending = ctx.new_array(ArrayElementType::Reference, events.len());
        let pending_pin = ctx.pin_native_root(pending);
        for (j, (kind, name)) in events.iter().enumerate() {
            // Same defect: `java.nio.file.WatchEvent` is an INTERFACE, and
            // the JDK's one implementation is `AbstractWatchKey.Event`.
            let we = crate::concrete_receiver::alloc_concrete(
                ctx,
                WE_IMPLS,
                "java/nio/file/WatchEvent",
                WE_NUM_FIELDS,
            )
            .obj;
            let we_pin = ctx.pin_native_root(we);
            let wk_now_for_watchable = ctx.read_native_pin(wk_pin, wk);
            let watchable = match wk_get(ctx, wk_now_for_watchable, WK_FIELD_WATCHABLE) {
                Value::Object(Some(p)) => Some(p),
                _ => None,
            };
            let context = watch_context_path(ctx, watchable, name)?;
            let we_now = ctx.read_native_pin(we_pin, we);
            we_set(ctx, we_now, WE_FIELD_KIND, Value::Int(*kind));
            we_set(ctx, we_now, WE_FIELD_CONTEXT, context);
            let pending_now = ctx.read_native_pin(pending_pin, pending);
            ctx.set_array_element(pending_now, j, Value::Object(Some(we_now)));
        }
        let wk_now = ctx.read_native_pin(wk_pin, wk);
        let pending_now = ctx.read_native_pin(pending_pin, pending);
        wk_set(
            ctx,
            wk_now,
            WK_FIELD_PENDING,
            Value::Object(Some(pending_now)),
        );
        signalled = Some(wk_now);
        break;
    }

    let signalled = signalled.map(|wk| {
        // Nothing allocates between here and the unpin, but read the key back
        // through its own pin anyway so the returned reference can never be a
        // pre-GC address.
        let pin = ctx.pin_native_root(wk);
        ctx.read_native_pin(pin, wk)
    });
    ctx.unpin_native_roots(this_pin);
    Ok(signalled)
}

pub(crate) fn native_ws_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(
        ctx,
        args,
        WS_NUM_FIELDS,
        "poll",
        "()Ljava/nio/file/WatchKey;"
    );
    let mut this = obj_arg92(args, 0)?;
    ws_require_open(ctx, &mut this)?;
    match ws_signalled_key(ctx, this)? {
        Some(wk) => Ok(Some(Value::Object(Some(wk)))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// `WatchService.poll(long, TimeUnit)` — wait up to the given duration for a
/// key to be signalled, then return it (or `null` on timeout).
///
/// This overload had no native at all before, so the call fell through to the
/// interface's abstract declaration and threw `AbstractMethodError`. It is the
/// overload a watch loop actually uses (`poll(quietPeriod, MILLISECONDS)`) —
/// `poll()` returns immediately and `take()` blocks forever, neither of which
/// lets a loop do periodic work.
pub(crate) fn native_ws_poll_timed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(
        ctx,
        args,
        WS_NUM_FIELDS,
        "poll",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey;"
    );
    let this = obj_arg92(args, 0)?;
    let timeout = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let unit = args.get(2).copied();

    let this_pin = ctx.pin_native_root(this);
    let millis = watch_timeout_millis(ctx, timeout, unit);
    let mut this = ctx.read_native_pin(this_pin, this);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);

    let result = loop {
        if let Err(e) = ws_require_open(ctx, &mut this) {
            break Err(e);
        }
        match ws_signalled_key(ctx, this) {
            Err(e) => break Err(e),
            Ok(Some(wk)) => break Ok(Some(Value::Object(Some(wk)))),
            Ok(None) => {}
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            break Ok(Some(Value::Object(None)));
        }
        // Sleep in short slices inside a timed blocking region: the thread is
        // GC-safe while parked (a stop-the-world collector must not wait on
        // it) and `close()` becomes visible within one slice.
        let slice = (deadline - now).min(std::time::Duration::from_millis(20));
        let mut blocked = [Value::Object(Some(this))];
        // Interpreter round i3 wave 80, lane L6 (page
        // `i59-L6-blocking-regions-not-yet-classified`): JDK 25
        // `AbstractWatchService.poll(long, TimeUnit)` is
        // `pendingKeys.poll(timeout, unit)` on a `LinkedBlockingDeque`, a
        // timed `ConditionObject.awaitNanos`: a timed park.
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::TimedParked);
        std::thread::sleep(slice);
        ctx.end_blocking_region_refs(&mut blocked);
        if let Value::Object(Some(cur)) = blocked[0] {
            this = cur;
        }
    };
    ctx.unpin_native_roots(this_pin);
    result
}

/// Blocking `WatchService.take()`. Polls the underlying `notify` channel in
/// short bursts so we can cooperatively respond to close/interrupt without
/// holding the WATCH_SERVICES lock across a `recv()` call (which would
/// deadlock every other native entering the table).
pub(crate) fn native_ws_take(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(
        ctx,
        args,
        WS_NUM_FIELDS,
        "take",
        "()Ljava/nio/file/WatchKey;"
    );
    let this = obj_arg92(args, 0)?;
    let this_pin = ctx.pin_native_root(this);
    let mut this = this;
    let result = loop {
        if let Err(e) = ws_require_open(ctx, &mut this) {
            break Err(e);
        }
        // Drain whatever the OS has delivered so far, then try to return a key.
        {
            // GC-stable-key-fix: identity-hash, not the raw heap address; per
            // VM since gc-common w15-c. `this` is current (re-read above).
            let this_key = io_side_row_key(ctx, this);
            let mut services = watch_services().lock();
            if let Some(state) = services.get_mut(&this_key) {
                drain_into_queues(state);
            }
        }
        match ws_signalled_key(ctx, this) {
            Err(e) => break Err(e),
            Ok(Some(wk)) => break Ok(Some(Value::Object(Some(wk)))),
            Ok(None) => {}
        }
        this = ctx.read_native_pin(this_pin, this);
        // Nothing pending — wait a short slice for the next OS event.
        // Using recv_timeout here would require holding the services lock;
        // instead we sleep briefly and re-drain. 50ms is small enough that
        // close() / interrupt become visible promptly. The sleep runs inside a
        // blocking region so a concurrent stop-the-world GC is not held off
        // for the whole slice.
        let mut blocked = [Value::Object(Some(this))];
        // Interpreter round i3 wave 80, lane L6: `AbstractWatchService.take()`
        // is `pendingKeys.take()`, an untimed `ConditionObject.await`: a park
        // (WAITING), where the timed region read TIMED_WAITING.
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
        std::thread::sleep(std::time::Duration::from_millis(50));
        ctx.end_blocking_region_refs(&mut blocked);
        if let Value::Object(Some(cur)) = blocked[0] {
            this = cur;
        }
    };
    ctx.unpin_native_roots(this_pin);
    result
}

/// Convert a `(timeout, TimeUnit)` pair to whole milliseconds.
///
/// `TimeUnit.toMillis` is real JDK bytecode, so ask the unit itself; the name
/// table is the fallback for a synthetic `TimeUnit` surface. A null unit means
/// the caller already passed milliseconds.
pub(crate) fn watch_timeout_millis(ctx: &mut dyn NativeContext, timeout: i64, unit: Option<Value>) -> u64 {
    if timeout <= 0 {
        return 0;
    }
    let Some(Value::Object(Some(u))) = unit else {
        return timeout as u64;
    };
    // gc-common w20-f: a `toMillis` that ran and failed (Java) left the
    // `name()` fallback dispatching on the unit's pre-call address. Pinned.
    let u_pin = ctx.pin_native_root(u);
    let to_millis = ctx.invoke_virtual(u, "toMillis", "(J)J", &[Value::Long(timeout)]);
    let u: ObjectRef = ctx.read_native_pin(u_pin, u);
    ctx.unpin_native_roots(u_pin);
    if let Ok(Some(Value::Long(ms))) = to_millis {
        return ms.max(0) as u64;
    }
    let name = match ctx.invoke_virtual(u, "name", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    let ms = match name.as_deref() {
        Some("NANOSECONDS") => timeout / 1_000_000,
        Some("MICROSECONDS") => timeout / 1_000,
        Some("SECONDS") => timeout.saturating_mul(1_000),
        Some("MINUTES") => timeout.saturating_mul(60_000),
        Some("HOURS") => timeout.saturating_mul(3_600_000),
        Some("DAYS") => timeout.saturating_mul(86_400_000),
        // MILLISECONDS, or an unrecognized unit: treat the value as millis.
        _ => timeout,
    };
    ms.max(0) as u64
}

pub(crate) fn native_ws_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(ctx, args, WS_NUM_FIELDS, "close", "()V");
    let this = obj_arg92(args, 0)?;
    // A `WatchService` that did not come from `native_ws_new` has none of our
    // slots (the real `java.nio.file.WatchService` is an interface, so its
    // class layout declares zero fields). Writing slot 2 on such a receiver is
    // an out-of-bounds field write the heap guard drops with a warning; skip
    // it instead of relying on the guard.
    if ctx.object_num_fields(this) > watch_base(ctx, this, WS_NUM_FIELDS) + WS_FIELD_OPEN {
        ws_set(ctx, this, WS_FIELD_OPEN, Value::Int(0));
        // Closing a service cancels every key it created (JDK contract).
        let count = match ws_get(ctx, this, WS_FIELD_COUNT) {
            Value::Int(n) => n.max(0) as usize,
            _ => 0,
        };
        if let Value::Object(Some(regs)) = ws_get(ctx, this, WS_FIELD_REGS) {
            for i in 0..count.min(ctx.array_length(regs)) {
                if let Value::Object(Some(wk)) = ctx.get_array_element(regs, i) {
                    wk_set(ctx, wk, WK_FIELD_VALID, Value::Int(0));
                }
            }
        }
    }
    // Dropping the WatchServiceState releases the platform watcher and its
    // background thread, which in turn hangs up the mpsc sender so any
    // concurrent `take()` observes the OPEN=0 flag and returns.
    // GC-stable-key-fix: identity-hash, not the raw heap address; per VM since
    // gc-common w15-c, so this can no longer close another VM's watcher. The
    // watcher is released AFTER the table lock (it used to be dropped inside
    // the `remove` statement, with the lock held).
    let this_key = io_side_row_key(ctx, this);
    let removed = watch_services().lock().remove(&this_key);
    close_dropped_watch_services(removed.into_iter().collect());
    Ok(None)
}

pub(crate) fn native_wk_poll_events(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(ctx, args, WK_NUM_FIELDS, "pollEvents", "()Ljava/util/List;");
    let this = obj_arg92(args, 0)?;
    let pending = match wk_get(ctx, this, WK_FIELD_PENDING) {
        Value::Object(Some(a)) => Some(a),
        _ => None,
    };
    // "Retrieves and removes all pending events for this watch key" — the
    // events must not be handed out twice.
    wk_set(ctx, this, WK_FIELD_PENDING, Value::Object(None));

    let this_pin = ctx.pin_native_root(this);
    let pending_pin = pending.map(|p| (ctx.pin_native_root(p), p));
    let len = pending.map(|p| ctx.array_length(p)).unwrap_or(0);

    // A REAL `java.util.ArrayList`, not a synthetic 2-slot stand-in: the
    // caller iterates the result with a for-each loop, which runs the real
    // `ArrayList$Itr` bytecode against the real `elementData`/`size`/`modCount`
    // layout. The synthetic object had neither the right slot count nor the
    // right slot meanings for that.
    let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[]) {
        Ok(Some(Value::Object(Some(l)))) => Some(l),
        _ => None,
    };
    if let Some(list) = list {
        let list_pin = ctx.pin_native_root(list);
        for i in 0..len {
            let Some((pin, fallback)) = pending_pin else {
                break;
            };
            let arr = ctx.read_native_pin(pin, fallback);
            let elem = ctx.get_array_element(arr, i);
            let list_now = ctx.read_native_pin(list_pin, list);
            let _ = ctx.invoke_virtual(list_now, "add", "(Ljava/lang/Object;)Z", &[elem]);
        }
        let list = ctx.read_native_pin(list_pin, list);
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(Some(list))));
    }

    // Fallback for a VM configuration without a usable real ArrayList: the
    // historical 2-field synthetic list.
    let list = try_alloc_synthetic(ctx, "java/util/ArrayList", 2)?;
    // GC: the `None` arm below ALLOCATES, so `list` is a pre-allocation
    // address by the time it is stored through. The function pins `this` and
    // the pending array and not this one.
    let list_pin = ctx.pin_native_root(list);
    let arr = match pending_pin {
        Some((pin, fallback)) => ctx.read_native_pin(pin, fallback),
        None => ctx.new_array(ArrayElementType::Reference, 0),
    };
    let list = ctx.read_native_pin(list_pin, list);
    ctx.set_field(list, 0, Value::Object(Some(arr)));
    ctx.set_field(list, 1, Value::Int(len as i32));
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_wk_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(ctx, args, WK_NUM_FIELDS, "reset", "()Z");
    let this = obj_arg92(args, 0)?;
    let valid = matches!(wk_get(ctx, this, WK_FIELD_VALID), Value::Int(1));
    if valid {
        // Re-arm: drop whatever `pollEvents()` did not consume. The previous
        // body installed a fresh 64-element array here, which `pollEvents()`
        // then reported as 64 pending (null) events.
        wk_set(ctx, this, WK_FIELD_PENDING, Value::Object(None));
    }
    Ok(Some(Value::Int(if valid { 1 } else { 0 })))
}

/// `WatchKey.watchable()` — the object the key was created for, i.e. the
/// `Path` handed to `Path.register`.
pub(crate) fn native_wk_watchable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(
        ctx,
        args,
        WK_NUM_FIELDS,
        "watchable",
        "()Ljava/nio/file/Watchable;"
    );
    let this = obj_arg92(args, 0)?;
    if let Value::Object(Some(p)) = wk_get(ctx, this, WK_FIELD_WATCHABLE) {
        return Ok(Some(Value::Object(Some(p))));
    }
    // No stored watchable (a key from an older layout): rebuild a synthetic
    // Path from the canonical path string.
    let name = match wk_get(ctx, this, WK_FIELD_PATH) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    Ok(Some(Value::Object(Some(alloc_synthetic_path(ctx, &name)?))))
}

pub(crate) fn native_wk_cancel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(ctx, args, WK_NUM_FIELDS, "cancel", "()V");
    let this = obj_arg92(args, 0)?;
    wk_set(ctx, this, WK_FIELD_VALID, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_wk_is_valid(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(ctx, args, WK_NUM_FIELDS, "isValid", "()Z");
    let this = obj_arg92(args, 0)?;
    let valid = matches!(wk_get(ctx, this, WK_FIELD_VALID), Value::Int(1));
    Ok(Some(Value::Int(if valid { 1 } else { 0 })))
}

pub(crate) fn native_we_kind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(
        ctx,
        args,
        WE_NUM_FIELDS,
        "kind",
        "()Ljava/nio/file/WatchEvent$Kind;"
    );
    let this = obj_arg92(args, 0)?;
    let bit = match we_get(ctx, this, WE_FIELD_KIND) {
        Value::Int(k) => k,
        _ => 0,
    };
    // The real `StandardWatchEventKinds` constants are singletons that callers
    // compare by identity (`event.kind() == ENTRY_CREATE`); a freshly minted
    // synthetic Kind would never match one.
    Ok(Some(watch_event_kind_object(ctx, bit)?))
}

pub(crate) fn native_we_context(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    watch_real_yield!(ctx, args, WE_NUM_FIELDS, "context", "()Ljava/lang/Object;");
    let this = obj_arg92(args, 0)?;
    Ok(Some(we_get(ctx, this, WE_FIELD_CONTEXT)))
}

// ---------------------------------------------------------------------------
// 92.3: DatagramChannel (UDP)
// ---------------------------------------------------------------------------

pub(crate) fn register_datagram_channel(r: &mut NativeMethodRegistry) {
    // Where this registrar's own rows start, so the mirror at the foot of the
    // function can only ever see rows written below this line. See
    // `mirror_class_registrations` for why filtering on the class name alone
    // would be wrong.
    let __rows_before = r.registration_count();
    let dc = "java/nio/channels/DatagramChannel";

    // `provider()` — the platform `SelectorProvider`, not `null`.
    //
    // `AbstractSelectableChannel.provider()` reads a `private final
    // SelectorProvider provider` field that this VM's synthetic channel never
    // populates, so the accessor answered `null` and the documented route
    // `dc.provider().openDatagramChannel()` was an NPE at a site that no longer
    // names the cause. `Selector.provider()` was repaired the same way and the
    // static `SelectorProvider.provider()` already resolves here — MEASURED,
    // `sun.nio.ch.EPollSelectorProvider`, the same object HotSpot's instance
    // accessor hands back (`probes/ResidualProbe.java`).
    r.register(
        dc,
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
        |ctx, _args| {
            ctx.invoke(
                "java/nio/channels/spi/SelectorProvider",
                "provider",
                "()Ljava/nio/channels/spi/SelectorProvider;",
                &[],
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // open() → DatagramChannel
    r.register(
        dc,
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
        native_dc_open, cratonvm_native_api::NativeKind::Bridge,
    );
    // JDK 25's JNDI DNS client selects IPv4 explicitly before it binds its
    // temporary UDP channel: `DatagramChannel.open(StandardProtocolFamily.INET)`.
    // Leaving that overload on real `DatagramChannelImpl` bypasses the bridge
    // above and its local-address state, so `getLocalAddress()` returns null
    // after `bind(null)`. The synthetic channel is IPv4/wildcard-backed already;
    // accept the protocol-family argument and use the same allocation path.
    r.register(
        dc,
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
        native_dc_open, cratonvm_native_api::NativeKind::Bridge,
    );
    // `DatagramChannel.open(ProtocolFamily)` delegates to this abstract
    // SelectorProvider method. Registering only the public static factory is
    // insufficient when real-JDK bytecode is selected, because the provider's
    // concrete EPoll implementation then allocates a separate channel whose
    // local-address fields CratonVM does not maintain. Keep the provider
    // result on the same fd-table-backed DatagramChannel path.
    //
    // BOTH descriptors, and this is the point. `openDatagramChannel()` is a
    // SEPARATE METHOD, not a default-argument form of the one below it, and it
    // is the one netty takes: `NioDatagramChannel()` calls
    // `DEFAULT_SELECTOR_PROVIDER.openDatagramChannel()`. While only the
    // `(ProtocolFamily)` row existed, the no-arg call fell through to real
    // `DatagramChannelImpl` construction, which this VM cannot complete — so the
    // channel arrived ALREADY CLOSED (`isOpen()==false`,
    // `socket().isClosed()==true`), and every `DatagramSocket` accessor on a
    // fresh netty datagram channel reported "Socket is closed" while
    // `DefaultDatagramChannelConfig.setBroadcast` NPE'd on the null local
    // address a closed adaptor returns.
    //
    // Registered on the concrete platform providers as well as on the abstract
    // base and `SelectorProviderImpl`, matching what `socket_channel.rs` does
    // for `openServerSocketChannel`/`openSocketChannel`: native dispatch may key
    // on the receiver's concrete class (`sun.nio.ch.EPollSelectorProvider` on
    // Linux, `WEPollSelectorProvider` on Windows) or on the declaring class of
    // the resolved method, and the pair covers both.
    for prov in [
        "java/nio/channels/spi/SelectorProvider",
        "sun/nio/ch/SelectorProviderImpl",
        "sun/nio/ch/EPollSelectorProvider",
        "sun/nio/ch/WEPollSelectorProvider",
    ] {
        r.register(
            prov,
            "openDatagramChannel",
            "()Ljava/nio/channels/DatagramChannel;",
            native_dc_open, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            prov,
            "openDatagramChannel",
            "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
            native_dc_open, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // bind(SocketAddress) → DatagramChannel
    r.register(
        dc,
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
        native_dc_bind, cratonvm_native_api::NativeKind::Bridge,
    );

    // `DnsClient` uses a connected datagram channel for its resolver traffic.
    // The abstract JDK declaration has no Code attribute, so it must be
    // bridged explicitly instead of falling through to bytecode dispatch.
    r.register(
        dc,
        "connect",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
        native_dc_connect, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dc,
        "disconnect",
        "()Ljava/nio/channels/DatagramChannel;",
        native_dc_disconnect, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(dc, "isConnected", "()Z", native_dc_is_connected, cratonvm_native_api::NativeKind::Bridge);
    r.register(dc, "write", "(Ljava/nio/ByteBuffer;)I", native_dc_write, cratonvm_native_api::NativeKind::Bridge);
    r.register(dc, "read", "(Ljava/nio/ByteBuffer;)I", native_dc_read, cratonvm_native_api::NativeKind::Bridge);
    // The Gathering/ScatteringByteChannel pair. Absent until now, so the JDK's
    // own `DatagramChannelImpl` bytecode ran against a synthetic receiver and
    // died on a null `writeLock`/`readLock`. W7-9 section 6.
    r.register(
        dc,
        "write",
        "([Ljava/nio/ByteBuffer;II)J",
        native_dc_write_gathering, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dc,
        "read",
        "([Ljava/nio/ByteBuffer;II)J",
        native_dc_read_scattering, cratonvm_native_api::NativeKind::Bridge,
    );
    // The one-argument forms the interfaces also declare, defined by the JDK as
    // the three-argument ones over the whole array.
    r.register(dc, "write", "([Ljava/nio/ByteBuffer;)J", |ctx, args| {
        let srcs = obj_arg92(args, 1)?;
        let n = ctx.array_length(srcs) as i32;
        native_dc_write_gathering(ctx, &[args[0], args[1], Value::Int(0), Value::Int(n)])
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(dc, "read", "([Ljava/nio/ByteBuffer;)J", |ctx, args| {
        let dsts = obj_arg92(args, 1)?;
        let n = ctx.array_length(dsts) as i32;
        native_dc_read_scattering(ctx, &[args[0], args[1], Value::Int(0), Value::Int(n)])
    }, cratonvm_native_api::NativeKind::Bridge);

    // send(ByteBuffer, SocketAddress) → int
    // Was deferred to datagram.rs, which resolved the channel through its own
    // registry that open()/bind() never populated — every send threw
    // "send: no socket id". It now lives here with the rest of the
    // fd_table-backed family; datagram.rs resolves through `dc_fd` too.
    r.register(
        dc,
        "send",
        "(Ljava/nio/ByteBuffer;Ljava/net/SocketAddress;)I",
        native_dc_send, cratonvm_native_api::NativeKind::Bridge,
    );

    // receive(ByteBuffer) → SocketAddress
    r.register(
        dc,
        "receive",
        "(Ljava/nio/ByteBuffer;)Ljava/net/SocketAddress;",
        native_dc_receive, cratonvm_native_api::NativeKind::Bridge,
    );

    // close() → void
    r.register(dc, "close", "()V", native_dc_close, cratonvm_native_api::NativeKind::Bridge);
    // The two protected halves of the same teardown. `close()` is `final` on
    // `AbstractInterruptibleChannel` and drives `implCloseChannel()`, which
    // `AbstractSelectableChannel` implements (also `final`) in terms of the
    // abstract `implCloseSelectableChannel()`. `socket_channel.rs` registers
    // both spellings for the stream channels and this family did not. Both
    // point at the same body, which is what the JDK's own chain does.
    //
    // These rows are load-bearing, and it took a control build to know it: with
    // them and the `init_channel_locks` seeding in `native_dc_open` present,
    // `CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN=0` reads 0/4000 on
    // `probes/ChannelCloseDevirtProbe.java`; with this file reverted to `dev`
    // and the same screen off, 3487/4000. So the JDK `close()` bytecode does
    // reach them -- `implCloseChannel` is dispatched virtually from
    // `AbstractInterruptibleChannel.close()`, and the receiver walk finds the
    // registration here.
    r.register(dc, "implCloseChannel", "()V", native_dc_close, cratonvm_native_api::NativeKind::Bridge);
    r.register(dc, "implCloseSelectableChannel", "()V", native_dc_close, cratonvm_native_api::NativeKind::Bridge);

    // isOpen() → boolean
    r.register(dc, "isOpen", "()Z", native_dc_is_open, cratonvm_native_api::NativeKind::Bridge);

    // configureBlocking(boolean) → SelectableChannel
    r.register(
        dc,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        native_dc_configure_blocking, cratonvm_native_api::NativeKind::Bridge,
    );

    // isBlocking() → boolean. Must be registered alongside the
    // `configureBlocking` above — see `native_dc_is_blocking` for why the two
    // other implementations of this signature (slot-3 readers) cannot observe
    // this family's state.
    r.register(dc, "isBlocking", "()Z", native_dc_is_blocking, cratonvm_native_api::NativeKind::Bridge);

    // getLocalAddress() → SocketAddress
    r.register(
        dc,
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
        native_dc_local_addr, cratonvm_native_api::NativeKind::Bridge,
    );
    // `NetworkChannel` is the inherited contract the real-JDK resolver can
    // resolve for this accessor. Keep that owner on the same bridge too.
    r.register(
        "java/nio/channels/NetworkChannel",
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
        native_dc_local_addr, cratonvm_native_api::NativeKind::Bridge,
    );
    // Package-private `localAddress()` — the SAME bridge `SocketChannel` and
    // `ServerSocketChannel` already register (see `socket_channel.rs`), and for
    // the same reason: it is declared on `sun.nio.ch.DatagramChannelImpl`, not
    // on the abstract `java.nio.channels.DatagramChannel` this VM actually
    // instantiates, while `channel.socket()` hands back the REAL
    // `sun.nio.ch.DatagramSocketAdaptor` whose JDK bytecode calls it.
    //
    // Without this row every DatagramSocket accessor that goes through the
    // adaptor dies with
    //   NoSuchMethodError: java.nio.channels.DatagramChannel.localAddress()
    // — `isBound()`, and through it `getLocalAddress()` answering null, which
    // is what NPEs `DefaultDatagramChannelConfig.setBroadcast` (netty
    // `NioDatagramChannelTest.testBindMultiple`) and makes `isBroadcast()`
    // report `SocketException: Socket is closed` on an open socket
    // (`testGetOptions`, `testNioChannelOption`).
    //
    // NOTE THE DESCRIPTOR. `DatagramChannelImpl.localAddress()` returns the
    // NARROWER `java.net.InetSocketAddress`, unlike `SocketChannelImpl`'s
    // `java.net.SocketAddress`. Registering the SocketChannel spelling here
    // resolves nothing -- the NoSuchMethodError just comes back naming
    // `()Ljava/net/InetSocketAddress;`. Register both spellings so either
    // resolution wins; `native_dc_local_addr` already builds a real
    // `java.net.InetSocketAddress`, so both are type-correct.
    r.register(
        dc,
        "localAddress",
        "()Ljava/net/InetSocketAddress;",
        native_dc_local_addr, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dc,
        "localAddress",
        "()Ljava/net/SocketAddress;",
        native_dc_local_addr, cratonvm_native_api::NativeKind::Bridge,
    );
    // The remote twin of the pair above, and it had NO registration at all —
    // so `socket().getRemoteSocketAddress()` and `socket().getPort()` both died
    // with `NoSuchMethodError: DatagramChannel.remoteAddress()`. Same two
    // spellings, same reason. See `native_dc_remote_addr`.
    r.register(
        dc,
        "remoteAddress",
        "()Ljava/net/InetSocketAddress;",
        native_dc_remote_addr, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dc,
        "remoteAddress",
        "()Ljava/net/SocketAddress;",
        native_dc_remote_addr, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dc,
        "getRemoteAddress",
        "()Ljava/net/SocketAddress;",
        native_dc_remote_addr, cratonvm_native_api::NativeKind::Bridge,
    );

    // The generic `SocketOption` surface. It has to land together with the
    // no-arg `openDatagramChannel()` above: with the factory bridged and this
    // still missing, netty's `NioDatagramChannelTest` goes from 1 passing / 3
    // failing to 0/4, because a channel that now opens successfully gets far
    // enough to reach `getOption`/`setOption`/`supportedOptions` — and those
    // were the abstract declarations. Two descriptors for `setOption`: real
    // bytecode emits the COVARIANT `…)Ljava/nio/channels/DatagramChannel;`,
    // while a caller holding a `NetworkChannel` emits the other.
    r.register(
        dc,
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/DatagramChannel;",
        dc_set_option, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dc,
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
        dc_set_option, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dc,
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
        dc_get_option, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dc,
        "supportedOptions",
        "()Ljava/util/Set;",
        dc_supported_options, cratonvm_native_api::NativeKind::Bridge,
    );

    // socket() → DatagramSocket
    r.register(
        dc,
        "socket",
        "()Ljava/net/DatagramSocket;",
        native_dc_socket, cratonvm_native_api::NativeKind::Bridge,
    );

    // DatagramSocket-surface adaptor methods.
    //
    // These are registered on TWO receivers, and the pair is the point.
    //
    //  * `java/nio/channels/DatagramChannel`, because `socket()` used to answer
    //    the channel itself, so `datagramChannel.socket().<datagramSocketMethod>()`
    //    resolved these `java/net/DatagramSocket` methods against THIS class.
    //    The real abstract `DatagramChannel` declares none of them, so they
    //    reached native lookup here.
    //  * `sun/nio/ch/DatagramSocketAdaptor`, because `socket()` now answers the
    //    real adaptor (see `native_dc_socket` for why that had to change). The
    //    adaptor's own bodies would NOT do: its `setSoTimeout` writes a private
    //    Java field and nothing else, and its four option setters delegate to
    //    `DatagramChannelImpl.setOption`, which THIS crate does not register —
    //    `net_channels`'s `setOption` is backed by a different, unpopulated
    //    socket registry. Letting the adaptor's bytecode run would therefore
    //    turn every one of these into a silent no-op against the channel's
    //    real UDP fd, which is the regression the double registration prevents.
    //
    // Concretely, Tomcat's `NioReceiver.configureDatagramChannel()` calls
    // `socket().{setSendBufferSize,setReceiveBufferSize,setReuseAddress,
    // setSoTimeout,setTrafficClass}` and `ReceiverBase.bindUdp()` calls
    // `socket().bind(addr)` — the void `DatagramSocket.bind(SocketAddress)`.
    // Buffer/option setters are best-effort against the channel's UDP fd;
    // `bind(SocketAddress)V` performs the real bind so UDP receive works.
    // `dc_receiver_channel` is what makes one body serve both receivers.
    let dsa = "sun/nio/ch/DatagramSocketAdaptor";

    r.register(
        dc,
        "setSendBufferSize",
        "(I)V",
        native_dc_set_send_buffer_size, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dsa,
        "setSendBufferSize",
        "(I)V",
        native_dc_set_send_buffer_size, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dc,
        "setReceiveBufferSize",
        "(I)V",
        native_dc_set_recv_buffer_size, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dsa,
        "setReceiveBufferSize",
        "(I)V",
        native_dc_set_recv_buffer_size, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(dc, "setReuseAddress", "(Z)V", native_dc_set_reuse_address, cratonvm_native_api::NativeKind::Bridge);
    r.register(dsa, "setReuseAddress", "(Z)V", native_dc_set_reuse_address, cratonvm_native_api::NativeKind::Bridge);
    // `isConnected()Z` is a `DatagramSocket` method whose name and descriptor
    // happen to match `DatagramChannel`'s, so it worked while `socket()`
    // answered the channel. The adaptor's own body reads
    // `DatagramChannelImpl.remoteAddress()`, which is not registered anywhere,
    // so without this row the swap would turn a working answer into a
    // NoSuchMethodError.
    r.register(dsa, "isConnected", "()Z", native_dc_is_connected, cratonvm_native_api::NativeKind::Bridge);
    // Was accepted and discarded on the grounds that a non-blocking channel
    // ignores SO_TIMEOUT — but this method is reached through
    // `channel.socket()`, i.e. by callers using the BLOCKING DatagramSocket
    // surface, where the timeout is the only thing stopping `receive()` from
    // blocking forever. Apply it to the channel's UDP fd like the sibling
    // buffer/reuse setters above.
    r.register(dc, "setSoTimeout", "(I)V", native_dc_set_so_timeout, cratonvm_native_api::NativeKind::Bridge);
    r.register(dsa, "setSoTimeout", "(I)V", native_dc_set_so_timeout, cratonvm_native_api::NativeKind::Bridge);
    r.register(dc, "setTrafficClass", "(I)V", native_dc_set_traffic_class, cratonvm_native_api::NativeKind::Bridge);
    r.register(dsa, "setTrafficClass", "(I)V", native_dc_set_traffic_class, cratonvm_native_api::NativeKind::Bridge);

    // bind(SocketAddress)V — the void `DatagramSocket.bind`. Delegates to the
    // channel's own real bind (close old fd, open a fresh UDP fd bound to the
    // requested address) and discards the channel return value.
    r.register(
        dc,
        "bind",
        "(Ljava/net/SocketAddress;)V",
        native_dc_socket_bind, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        dsa,
        "bind",
        "(Ljava/net/SocketAddress;)V",
        native_dc_socket_bind, cratonvm_native_api::NativeKind::Bridge,
    );

    // Every row above that names the abstract public class, again on the
    // CONCRETE class `native_dc_open` now mints.
    //
    // This is the registration half of the fabricated-receiver fix and it is
    // not optional. Native dispatch keys on the receiver's runtime class
    // (`H11-1`, measured twice), and the one fallback walk runs only when the
    // receiver's own class declares neither the method nor a registration —
    // which a real `sun.nio.ch.DatagramChannelImpl` does NOT satisfy, because
    // it declares `read`/`write`/`send`/`receive`/`close`/`bind`/… with `Code`.
    // Moving the receiver without moving the registration would therefore not
    // fall back to the rows above; it would run the JDK's own bytecode against
    // a channel whose `<init>` this VM never ran.
    //
    // The `sun/nio/ch/DatagramSocketAdaptor` and `SelectorProvider` rows above
    // are deliberately NOT mirrored: they are registered on their own classes,
    // and the filter is on `dc`.
    crate::concrete_receiver::mirror_class_registrations(
        r,
        __rows_before,
        dc,
        "sun/nio/ch/DatagramChannelImpl",
    );

    }

/// The `DatagramChannel` a `DatagramSocket`-surface native was reached through.
///
/// Since [`native_dc_socket`] answers a real `sun.nio.ch.DatagramSocketAdaptor`
/// rather than the channel, these bodies see one of two receivers. The adaptor
/// keeps the channel in its private `dc` field, so one lookup normalises both.
///
/// The non-object arm is the CHANNEL arm, not an error path: a channel has no
/// `dc` field, and `get_field_by_name` answers `Int(0)` for a field the class
/// does not declare rather than failing. The fallback is therefore "the
/// receiver already is the channel", which is also what the pre-adaptor
/// `socket()` handed out and what [`native_dc_socket`]'s own fallback still
/// returns when the adaptor cannot be built.
pub(crate) fn dc_receiver_channel(ctx: &dyn NativeContext, receiver: ObjectRef) -> ObjectRef {
    match ctx.get_field_by_name(receiver, "dc") {
        Value::Object(Some(channel)) => channel,
        _ => receiver,
    }
}

pub(crate) fn native_dc_set_send_buffer_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = dc_receiver_channel(ctx, obj_arg92(args, 0)?);
    let size = args.get(1).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    if let Some(fd) = dc_fd(ctx, this) {
        let _ = ctx.fd_table().udp_set_send_buffer_size(fd, size);
    }
    Ok(None)
}

pub(crate) fn native_dc_set_recv_buffer_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = dc_receiver_channel(ctx, obj_arg92(args, 0)?);
    let size = args.get(1).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    if let Some(fd) = dc_fd(ctx, this) {
        let _ = ctx.fd_table().udp_set_recv_buffer_size(fd, size);
    }
    Ok(None)
}

pub(crate) fn native_dc_set_reuse_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = dc_receiver_channel(ctx, obj_arg92(args, 0)?);
    let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
    if let Some(fd) = dc_fd(ctx, this) {
        let _ = ctx.fd_table().udp_set_reuse_address(fd, on);
    }
    Ok(None)
}

/// `DatagramSocket.bind(SocketAddress)`, reached through `socket()`.
///
/// Rebuilds the argument list rather than forwarding `args` untouched: slot 0
/// may be the adaptor, and `native_dc_bind` keys the fd table on the channel.
pub(crate) fn native_dc_socket_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let channel = dc_receiver_channel(ctx, obj_arg92(args, 0)?);
    let addr = args.get(1).copied().unwrap_or(Value::Object(None));
    native_dc_bind(ctx, &[Value::Object(Some(channel)), addr])?;
    Ok(None)
}

pub(crate) fn native_dc_set_so_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let receiver = obj_arg92(args, 0)?;
    let this = dc_receiver_channel(ctx, receiver);
    let requested = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    // W7-8-fabricated-success-io-sweep.md recorded the `.max(0)` that stood
    // here as UNMEASURED, on the grounds that "not a method
    // `java.nio.channels.DatagramChannel` declares, so there is no javadoc to
    // quote". That framing was wrong, and this method's two registrations are
    // what refute it: the only way to reach this body is
    // `channel.socket().setSoTimeout(..)`, i.e. a caller using the
    // **DatagramSocket** surface, and that class does specify the answer.
    //
    // JDK 25 `java.net.DatagramSocket.setSoTimeout(int)`:
    //     @throws IllegalArgumentException if {@code timeout} is negative
    // and the implementation `socket()` actually returns,
    // `sun.nio.ch.DatagramSocketAdaptor.setSoTimeout` (JDK 25 src.zip:231):
    //     if (isClosed()) throw new SocketException("Socket is closed");
    //     if (timeout < 0) throw new IllegalArgumentException("timeout < 0");
    //
    // `.max(0)` turned `setSoTimeout(-1)` — the usual spelling of a
    // miscomputed deadline — into `setSoTimeout(0)`, and `0` is not a small
    // timeout, it is **no timeout**: the next `receive()` blocks forever
    // where the caller asked it to be bounded. That is the failure the
    // refusal exists to prevent, laundered into the one value that cannot be
    // distinguished from a healthy configuration.
    //
    // The message is HotSpot's own. NOT added here: the `isClosed()` refusal
    // that precedes it upstream. `dc_fd` answering `None` covers "closed"
    // AND "never bound", so raising `SocketException` on it would refuse a
    // channel the JDK accepts; the ordering consequence is stated rather
    // than guessed at — on a CLOSED channel with a negative timeout this
    // raises `IllegalArgumentException` where HotSpot raises
    // `SocketException`, which is one wrong exception type in place of a
    // silent success, not a new silence.
    if requested < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "timeout < 0".to_string(),
        }
        .into());
    }
    let millis = requested as u64;
    // JDK contract: 0 means "no timeout" (block indefinitely).
    let timeout = if millis == 0 {
        None
    } else {
        Some(std::time::Duration::from_millis(millis))
    };
    if let Some(fd) = dc_fd(ctx, this) {
        let _ = ctx.fd_table().udp_set_read_timeout(fd, timeout);
    }
    // Keep `sun.nio.ch.DatagramSocketAdaptor.timeout` in step when that is the
    // receiver. Its `getSoTimeout()` is NOT intercepted — it reads that private
    // field — so without this write an accepted timeout would read back as a
    // permanent `0`, i.e. "no timeout", which is the exact value this method's
    // refusal above exists to stop a caller from being handed by accident.
    if receiver != this {
        ctx.set_field_by_name(receiver, "timeout", Value::Int(requested));
    }
    Ok(None)
}

// ESCALATED wave 4 (2026-07-28) — this IS implementable, but not from this
// crate. `socket2` (already a dependency of `native-api`, `features =
// ["all"]`) exposes `SockRef::set_tos(u32)`, which is exactly IP_TOS /
// the JDK's `setTrafficClass`. What is missing is the fd-table accessor:
// `native-api/src/fd_table.rs` needs
//
//     pub fn udp_set_tos(&self, fd: FdId, tos: u32) -> Result<(), io::Error>
//
// written like its neighbour `udp_set_send_buffer_size` (match
// `FileEntry::UdpSocket(s)` → `socket2::SockRef::from(s).set_tos(tos)`).
// The `FileEntry` enum and `get_entry` are both private to that module and
// native-io does not depend on socket2, so the option cannot be reached
// from here. Once the accessor lands this becomes the same three lines as
// `setSoTimeout` above: `if let Some(fd) = dc_fd(ctx, this) { let _ =
// ctx.fd_table().udp_set_tos(fd, (tc & 0xff) as u32); }`.
//
// Until then, accepting and discarding is spec-legal rather than a silent
// failure: `DatagramSocket.setTrafficClass` is documented as advisory
// ("the underlying platform may ignore the value"), and the JDK's own
// contract only requires an IllegalArgumentException for values outside
// 0..=255 — which real callers (Tomcat's `NioReceiver`) never pass.
//
// IMPLEMENTED wave 4 (2026-07-28): `FdTable::udp_set_tos` was added for
// exactly this, so the escalation above is resolved and the body is now
// the same shape as `setSoTimeout`. The advisory-ness of IP_TOS is a
// statement about the network, not a licence to skip the syscall — "the
// platform may ignore it" and "we never asked" are different claims, and
// only the second was true before. The spec'd IllegalArgumentException is
// now enforced rather than waived on the grounds that today's callers
// happen not to trip it.
pub(crate) fn native_dc_set_traffic_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = dc_receiver_channel(ctx, obj_arg92(args, 0)?);
    let tc = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if !(0..=255).contains(&tc) {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("tc is not in range 0 -- 255: {tc}"),
        }
        .into());
    }
    if let Some(fd) = dc_fd(ctx, this) {
        let _ = ctx.fd_table().udp_set_tos(fd, tc as u32);
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// DatagramChannel — the generic `SocketOption` surface
// ---------------------------------------------------------------------------

/// Every `setOption` value Java has requested on a `DatagramChannel`, by option
/// name.
///
/// The individual setters above (`setReuseAddress`, `setReceiveBufferSize`, …)
/// write straight through to the fd and keep nothing, which is fine for a
/// write-only surface. `getOption` is a READER, and two of the options it must
/// answer cannot be read back off the socket at all in the shape the JDK
/// promises: `IP_MULTICAST_IF`'s value is a `NetworkInterface`, and the socket
/// only knows an address. So the requested value is recorded here as well, and
/// used when the socket cannot answer.
///
/// Keyed like every other `DatagramChannel` side table — see [`DcKey`] for why
/// identity hash plus VM identity, and why no collector hook is needed: the
/// values are ints and Strings, never `ObjectRef`s. Weak on the channel like
/// the rest (gc-common w16-c): [`DcOptionRow::owner`].
pub(crate) fn dc_option_state() -> &'static Mutex<HashMap<DcKey, DcOptionRow>> {
    static OPTS: OnceLock<Mutex<HashMap<DcKey, DcOptionRow>>> = OnceLock::new();
    OPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One channel's requested options, and the channel's address.
pub(crate) struct DcOptionRow {
    pub(crate) owner: usize,
    pub(crate) values: HashMap<String, i32>,
}

impl IoWeakRow for DcOptionRow {
    fn owner(&self) -> usize {
        self.owner
    }
    fn set_owner(&mut self, owner: usize) {
        self.owner = owner;
    }
}

/// `channel` must be CURRENT: it is (re)stamped as the row's weak owner.
pub(crate) fn dc_option_set(ctx: &dyn NativeContext, channel: ObjectRef, name: &str, value: i32) {
    let key = dc_key(ctx, channel);
    let owner = io_side_owner(channel);
    let mut options = dc_option_state().lock();
    let row = options.entry(key).or_insert_with(|| DcOptionRow {
        owner,
        values: HashMap::new(),
    });
    row.owner = owner;
    row.values.insert(name.to_string(), value);
}

pub(crate) fn dc_option_get(ctx: &dyn NativeContext, channel: ObjectRef, name: &str) -> Option<i32> {
    let key = dc_key(ctx, channel);
    dc_option_state()
        .lock()
        .get(&key)?
        .values
        .get(name)
        .copied()
}

/// Drop a closed channel's recorded options, so the identity hash it releases
/// cannot hand a later channel this one's settings — the same staleness
/// `dc_set_blocking` and `dc_socket_cache_clear` are reset for in `native_dc_close`.
pub(crate) fn dc_option_clear(ctx: &dyn NativeContext, channel: ObjectRef) {
    let key = dc_key(ctx, channel);
    dc_option_state().lock().remove(&key);
}

/// The option name carried by a `java.net.SocketOption` argument. Shares the
/// stream-channel reader, which knows that not every `SocketOption` the JDK's
/// own adaptors pass declares a `name` FIELD — see
/// `socket_channel::socket_option_name`.
pub(crate) fn dc_option_name(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> String {
    let option = match arg {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    crate::socket_channel::socket_option_name(ctx, option)
}

/// The first IPv4 address of a `java.net.NetworkInterface`, as the socket-level
/// `IP_MULTICAST_IF` value.
///
/// The JDK option is typed `SocketOption<NetworkInterface>` while the socket
/// option is an address, so the interface has to be resolved. Answering `None`
/// (IPv6-only or unresolvable interface) means the OS write is skipped and only
/// the Java-visible record is kept — which is still the value `getOption` will
/// return, so the round-trip the caller can observe stays consistent.
pub(crate) fn dc_interface_ipv4(ctx: &mut dyn NativeContext, ni: ObjectRef) -> Option<std::net::Ipv4Addr> {
    let addresses =
        match ctx.invoke_virtual(ni, "getInetAddresses", "()Ljava/util/Enumeration;", &[]) {
            Ok(Some(Value::Object(Some(e)))) => e,
            _ => return None,
        };
    // GC: every call in this loop runs the enumeration's Java, so the
    // enumeration itself does not survive its own `hasMoreElements`.
    let addr_pin = ctx.pin_native_root(addresses);
    let mut addresses = addresses;
    // Bounded: an interface with a pathological address list must not spin.
    for _ in 0..64 {
        addresses = ctx.read_native_pin(addr_pin, addresses);
        match ctx.invoke_virtual(addresses, "hasMoreElements", "()Z", &[]) {
            Ok(Some(Value::Int(1))) => {}
            _ => {
                ctx.unpin_native_roots(addr_pin);
                return None;
            }
        }
        addresses = ctx.read_native_pin(addr_pin, addresses);
        let addr = match ctx.invoke_virtual(addresses, "nextElement", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(a)))) => a,
            _ => {
                ctx.unpin_native_roots(addr_pin);
                return None;
            }
        };
        let text = match ctx.invoke_virtual(addr, "getHostAddress", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => continue,
        };
        if let Ok(v4) = text.parse::<std::net::Ipv4Addr>() {
            ctx.unpin_native_roots(addr_pin);
            return Some(v4);
        }
    }
    ctx.unpin_native_roots(addr_pin);
    None
}

/// `DatagramChannel.setOption(SocketOption, Object)`.
///
/// The individual `DatagramSocket`-surface setters registered above cover the
/// four options Tomcat's `NioReceiver` uses by name. This is the GENERIC
/// surface — the one `sun.nio.ch.DatagramSocketAdaptor` and netty's
/// `NioChannelOption` go through — and it had no registration at all, so it
/// resolved to the abstract declaration:
///
/// ```text
///   AbstractMethodError: java/nio/channels/DatagramChannel.setOption(
///       Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/DatagramChannel;
///       has no Code attribute
/// ```
pub(crate) fn dc_set_option(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // GC: `dc_option_name` reads the `SocketOption`'s name through Java, so
    // `this` is a pre-call address by the time the fd is read out of it. The
    // pin covers the whole body — every branch below stores through `this` and
    // two of them return it.
    let this = dc_receiver_channel(ctx, obj_arg92(args, 0)?);
    let base_pin = ctx.pin_native_root(this);
    let name = dc_option_name(ctx, args.get(1));
    let this = ctx.read_native_pin(base_pin, this);
    if name == "SO_REUSEPORT" && !cfg!(target_os = "linux") {
        // `supportedOptions()` does not list it here (see `dc_supported_options`),
        // and the JDK's `setOption` refuses an unlisted option.
        ctx.unpin_native_roots(base_pin);
        return Err(RuntimeError::UnsupportedOperationException {
            message: "'SO_REUSEPORT' not supported".to_string(),
        }
        .into());
    }
    let fd = dc_fd(ctx, this);

    // `IP_MULTICAST_IF` is the one option whose value is neither a Boolean nor
    // an Integer, so it cannot go through `socket_option_value`.
    if name == "IP_MULTICAST_IF" {
        let ni = match args.get(2) {
            Some(Value::Object(Some(ni))) => *ni,
            // The JDK rejects a null interface; nothing here can act on one
            // either, and silently recording "interface 0" would make
            // `getOption` answer a different object than was set.
            _ => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "IP_MULTICAST_IF requires a NetworkInterface".to_string(),
                }
                .into())
            }
        };
        // GC: `getIndex()` and `dc_interface_ipv4` both run Java, and `this`
        // is stored through and RETURNED after them. (The audit flagged
        // `index` here, which is an `i32`; the reference at risk is `this`.)
        // `ni` crosses the same two calls and is the receiver of the second.
        let ni_pin = ctx.pin_native_root(ni);
        let index = match ctx.invoke_virtual(ni, "getIndex", "()I", &[]) {
            Ok(Some(Value::Int(i))) => i,
            _ => 0,
        };
        let ni = ctx.read_native_pin(ni_pin, ni);
        if let (Some(fd), Some(v4)) = (fd, dc_interface_ipv4(ctx, ni)) {
            let _ = ctx.fd_table().udp_set_multicast_if_v4(fd, &v4);
        }
        let this = ctx.read_native_pin(base_pin, this);
        ctx.unpin_native_roots(base_pin);
        dc_option_set(ctx, this, &name, index);
        return Ok(Some(Value::Object(Some(this))));
    }

    let value = crate::socket_channel::socket_option_value(
        ctx,
        args.get(2).copied().unwrap_or(Value::Int(0)),
    );
    // `socket_option_value` unboxes through Java (`intValue`/`booleanValue`).
    let this = ctx.read_native_pin(base_pin, this);

    // Range checks first: `setOption` must refuse an out-of-range value rather
    // than record it and hand back a channel that reads it straight out again.
    match name.as_str() {
        "IP_TOS" | "IP_MULTICAST_TTL" if !(0..=255).contains(&value) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid value '{value}' for {name}"),
            }
            .into())
        }
        "SO_RCVBUF" | "SO_SNDBUF" if value <= 0 => {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid send/receive buffer size: {value}"),
            }
            .into())
        }
        _ => {}
    }

    if let Some(fd) = fd {
        let table = ctx.fd_table();
        let _ = match name.as_str() {
            "SO_REUSEADDR" => table.udp_set_reuse_address(fd, value != 0),
            "SO_BROADCAST" => table.udp_set_broadcast(fd, value != 0),
            "SO_RCVBUF" => table.udp_set_recv_buffer_size(fd, value as usize),
            "SO_SNDBUF" => table.udp_set_send_buffer_size(fd, value as usize),
            "IP_TOS" => table.udp_set_tos(fd, value as u32),
            "IP_MULTICAST_TTL" => table.udp_set_multicast_ttl_v4(fd, value as u32),
            "IP_MULTICAST_LOOP" => table.udp_set_multicast_loop_v4(fd, value != 0),
            _ => Ok(()),
        };
    }
    dc_option_set(ctx, this, &name, value);
    Ok(Some(Value::Object(Some(this))))
}

/// `DatagramChannel.getOption(SocketOption)`.
///
/// Reads the SOCKET where the socket can answer, and the recorded request
/// otherwise. The order matters: a value set before `bind()` was applied to the
/// fd that existed then, and `bind` closes that fd and opens a replacement
/// (`native_dc_bind`), so on a rebound channel the record is the only place the
/// caller's own value still exists.
///
/// The return MUST be boxed — `getOption` is `<T> T getOption(SocketOption<T>)`,
/// and a raw `Value::Int` returned for an object-typed method coerces to null,
/// which NPEs the adaptor's `((Boolean) …).booleanValue()`. See
/// `socket_channel::box_socket_option`.
pub(crate) fn dc_get_option(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // GC (gc-common w29-c): `dc_option_name` reads the `SocketOption`'s name
    // through Java, so `this` is re-read through a pin before the fd and the
    // recorded options are looked up by it -- as `dc_set_option` does. The
    // tables are keyed by the channel's CURRENT address now, so the pre-call
    // address (the lookup this used to make) would miss a moved channel's
    // socket. Released at the native's exit (`safe_native_call`).
    let this = dc_receiver_channel(ctx, obj_arg92(args, 0)?);
    let base_pin = ctx.pin_native_root(this);
    let name = dc_option_name(ctx, args.get(1));
    let this = ctx.read_native_pin(base_pin, this);
    let fd = dc_fd(ctx, this);

    if name == "IP_MULTICAST_IF" {
        // Prefer the interface Java itself named: the socket only knows an
        // address, and mapping an address back to an interface is lossy.
        let index = dc_option_get(ctx, this, &name).unwrap_or(0);
        if index > 0 {
            return ctx.invoke(
                "java/net/NetworkInterface",
                "getByIndex",
                "(I)Ljava/net/NetworkInterface;",
                &[Value::Int(index)],
            );
        }
        let address = fd.and_then(|fd| ctx.fd_table().udp_multicast_if_v4(fd).ok());
        match address {
            Some(v4) if !v4.is_unspecified() => {
                let text = ctx.create_string(&v4.to_string());
                let inet = ctx.invoke(
                    "java/net/InetAddress",
                    "getByName",
                    "(Ljava/lang/String;)Ljava/net/InetAddress;",
                    &[Value::Object(Some(text))],
                )?;
                ctx.invoke(
                    "java/net/NetworkInterface",
                    "getByInetAddress",
                    "(Ljava/net/InetAddress;)Ljava/net/NetworkInterface;",
                    &[inet.unwrap_or(Value::Object(None))],
                )
            }
            // No interface selected — HotSpot answers null here, measured.
            _ => Ok(Some(Value::Object(None))),
        }
    } else {
        let live = fd.and_then(|fd| {
            let table = ctx.fd_table();
            match name.as_str() {
                "SO_REUSEADDR" => table.udp_reuse_address(fd).ok().map(i32::from),
                "SO_BROADCAST" => table.udp_broadcast(fd).ok().map(i32::from),
                "SO_RCVBUF" => table.udp_recv_buffer_size(fd).ok().map(|n| n as i32),
                "SO_SNDBUF" => table.udp_send_buffer_size(fd).ok().map(|n| n as i32),
                "IP_TOS" => table.udp_tos(fd).ok().map(|n| n as i32),
                "IP_MULTICAST_TTL" => table.udp_multicast_ttl_v4(fd).ok().map(|n| n as i32),
                "IP_MULTICAST_LOOP" => table.udp_multicast_loop_v4(fd).ok().map(i32::from),
                _ => None,
            }
        });
        let raw = live
            .or_else(|| dc_option_get(ctx, this, &name))
            .unwrap_or(0);
        crate::socket_channel::box_socket_option(ctx, &name, raw)
    }
}

/// `DatagramChannel.supportedOptions()`.
///
/// HotSpot JDK 25 / Linux answers `[IP_DONTFRAGMENT, IP_MULTICAST_IF,
/// IP_MULTICAST_LOOP, IP_MULTICAST_TTL, IP_TOS, SO_BROADCAST,
/// SO_INCOMING_NAPI_ID, SO_RCVBUF, SO_REUSEADDR, SO_REUSEPORT, SO_SNDBUF]`
/// (measured). The `jdk.net.ExtendedSocketOptions` entries are left out here
/// because nothing in this file implements them, and the set is a CONTRACT, not
/// a wish list: netty's `NioChannelOption.setOption` uses
/// `supportedOptions().contains(...)` as its only gate and reports an unlisted
/// option as "not set" — so listing an option nothing honours turns a refusal
/// into a silent lie.
///
/// What must NOT be here is just as load-bearing: `TCP_NODELAY` is netty's
/// `newInvalidOption()` for `NioDatagramChannelTest.testInvalidNioChannelOption`,
/// which asserts `setOption` returns false for it.
pub(crate) fn dc_supported_options(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    crate::socket_channel::supported_options_set(
        ctx,
        &[
            "IP_MULTICAST_IF",
            "IP_MULTICAST_LOOP",
            "IP_MULTICAST_TTL",
            "IP_TOS",
            "SO_BROADCAST",
            "SO_RCVBUF",
            "SO_REUSEADDR",
            "SO_SNDBUF",
        ],
    )
}

/// `DatagramChannel.socket()`.
///
/// # This used to answer the CHANNEL ITSELF, and that was a type error
///
/// `socket()` is declared `()Ljava/net/DatagramSocket;` and a
/// `java.nio.channels.DatagramChannel` is not a `java.net.DatagramSocket`. The
/// old body returned `this` anyway, and nothing caught it: javac emits NO
/// checkcast at the call site, because the DECLARED return type already
/// satisfies the assignment, so the wrong object flowed into
/// `DatagramSocket`-typed locals silently. What it cost, measured against
/// HotSpot on the same host:
///
/// ```text
///   ch.socket().getClass().getName()   java.nio.channels.DatagramChannel
///                            HotSpot:  sun.nio.ch.DatagramSocketAdaptor
///   ch.socket() instanceof DatagramSocket   false     (HotSpot: true)
///   ch.socket() == ch                       true      (HotSpot: false)
///   ch.socket().getSoTimeout()   NoSuchMethodError    (HotSpot: the timeout)
///   ch.socket().getLocalPort()   NoSuchMethodError    (HotSpot: the port)
/// ```
///
/// Every `java.net.DatagramSocket` method that worked did so only because its
/// name AND descriptor happened to collide with a `DatagramChannel` native.
///
/// # The fix is the one `sc_socket`/`ssc_socket` already use
///
/// Mirror the real `DatagramChannelImpl.socket()` → `DatagramSocketAdaptor
/// .create(this)`. The adaptor is a genuine `java.net.DatagramSocket`
/// subclass, its construction runs the `DatagramSocket`/`MulticastSocket`
/// instance initializers, and it keeps the channel in its private `dc` field —
/// which is what `dc_receiver_channel` reads to keep every native below
/// working on the channel's real UDP fd.
///
/// The adaptor is CACHED, unlike `sc_socket`'s. Two calls to `socket()` must
/// answer the same object or the adaptor's own state — `timeout`, which
/// `getSoTimeout()` reads — resets on every call, turning an accepted timeout
/// into a silent `0`.
///
/// # The fallback is the channel, deliberately
///
/// `sun.nio.ch.DatagramSocketAdaptor` does not exist under `--real-jdk` /
/// synthetic-JDK, where there is no class library to build it from. Falling
/// back to the channel keeps that mode byte-for-byte on its current behaviour
/// instead of failing `socket()` outright. A bare `new java/net/DatagramSocket`
/// would be worse than either: JDK 25 routes every method through a `delegate`
/// field that no `<init>` has filled in.
///
/// # Where the adaptor is kept (gc-common w16-c)
///
/// In the channel's own `socket` field when its layout declares one
/// ([`dc_socket_slot`]), exactly as `DatagramChannelImpl.socket()` does: read
/// it, create on null, publish with a compare-and-set, and answer whichever
/// adaptor won. The pair is then an ordinary heap cycle that dies with the
/// channel. The rooted [`dc_socket_cache`] is only the fallback for a layout
/// without the field; it made every channel that called `socket()` immortal.
pub(crate) fn native_dc_socket(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg92(args, 0)?;
    let slot = dc_socket_slot(ctx, this);
    match slot {
        Some(slot) => {
            if let Value::Object(Some(cached)) = ctx.get_field(this, slot) {
                return Ok(Some(Value::Object(Some(cached))));
            }
        }
        None => {
            if let Some(cached) = dc_socket_cache_get(ctx, this) {
                return Ok(Some(Value::Object(Some(cached))));
            }
        }
    }
    // `invoke` runs Java bytecode, which allocates, which can relocate `this`
    // under a moving collector — and `this` is then written into the cache as
    // one half of a pair, so a stale local here would key the row on a dead
    // address. Same reason `ssc_socket` pins.
    let pin = ctx.pin_native_root(this);
    let created = ctx.invoke(
        "sun/nio/ch/DatagramSocketAdaptor",
        "create",
        "(Lsun/nio/ch/DatagramChannelImpl;)Ljava/net/DatagramSocket;",
        &[Value::Object(Some(this))],
    );
    this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    if let Ok(Some(v @ Value::Object(Some(adaptor)))) = created {
        match slot {
            Some(slot) => return Ok(Some(dc_publish_socket(ctx, this, slot, v))),
            None => dc_socket_cache_put(ctx, this, adaptor),
        }
        return Ok(Some(v));
    }
    Ok(Some(Value::Object(Some(this))))
}

/// The slot of the receiver's own `socket` field (`DatagramChannelImpl`'s
/// adaptor cache), resolved on its runtime class. `None` for a layout without
/// the field: the abstract `java.nio.channels.DatagramChannel` a fallback
/// allocation mints, or a fabricated synthetic-JDK stub.
pub(crate) fn dc_socket_slot(ctx: &dyn NativeContext, channel: ObjectRef) -> Option<usize> {
    let class_id = ctx.class_id_of_object(channel);
    ctx.resolve_field_index_by_class_id(class_id, "socket")
}

/// Publish `adaptor` in `channel.socket` unless another thread got there
/// first, and answer the adaptor the field holds: the JDK's
/// `SOCKET.compareAndSet(this, null, socket)` followed by a re-read. An
/// implementation whose CAS cannot act on the field (it answers false and the
/// field still reads null) gets a plain store, so `socket()` still answers
/// one object from then on.
pub(crate) fn dc_publish_socket(
    ctx: &mut dyn NativeContext,
    channel: ObjectRef,
    slot: usize,
    adaptor: Value,
) -> Value {
    if ctx.compare_and_swap_field(channel, slot, Value::Object(None), adaptor) {
        return adaptor;
    }
    match ctx.get_field(channel, slot) {
        winner @ Value::Object(Some(_)) => winner,
        _ => {
            ctx.set_field(channel, slot, adaptor);
            adaptor
        }
    }
}

pub(crate) fn native_dc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Close the sockets of channels that died unclosed since this VM last
    // opened or closed one (gc-common w16-c, `io_orphan_fds`).
    close_orphan_fds(ctx);
    // DUAL-STACK, matching `sun.nio.ch.Net.socket(family, stream=false)`: with
    // no `ProtocolFamily` argument the JDK opens AF_INET6 with `IPV6_V6ONLY`
    // off whenever IPv6 is available. An AF_INET channel here refused every
    // IPv6 destination with `EAFNOSUPPORT` — see `open_udp_dual_stack_socket`.
    let fd_id = ctx
        .fd_table()
        .open_udp_dual_stack()
        .map_err(|e| RuntimeError::IOException {
            message: format!("DatagramChannel.open: {e}"),
        })?;

    // Minted AS `sun.nio.ch.DatagramChannelImpl`, the class HotSpot 25 hands
    // back here, and NOT as the abstract `java.nio.channels.DatagramChannel`
    // this line used to name.
    //
    // **The defect.** `try_alloc_synthetic` resolves that name against the real
    // image, gets the real ABSTRACT JDK class, and `alloc_object` then mints an
    // object whose runtime class is abstract — a receiver `new` cannot legally
    // produce (JVMS §6.5). MEASURED by `probes/W4Abstract.java`:
    // `DatagramChannel.open().getClass()` answered
    // `java.nio.channels.DatagramChannel`, `Modifier.isAbstract == true`, in
    // BOTH modes, against `sun.nio.ch.DatagramChannelImpl` on the oracle. Same
    // one-line shape `H21-1` fixed for `Pipe` and `socket_channel.rs` for the
    // two stream channels.
    //
    // **Why no slot map has to move.** This family keeps NOTHING in the
    // object's fields: `dc_fds`, `dc_connected_channels`,
    // `dc_nonblocking_channels` and `dc_socket_cache` above are all
    // identity-hash keyed side tables, and their doc comments say why. So the
    // real `DatagramChannelImpl`'s much larger layout is inert here, exactly as
    // `socket_channel.rs::alloc_channel_as_impl` records for `SocketChannel`.
    // Contrast `datagram.rs::dgram_join_group`, which DOES index slots and
    // therefore needed the appended-slot base.
    //
    // **The registration half is in `register_datagram_channel`**, which
    // mirrors every row it writes onto the `Impl` spelling. Without that the
    // real `DatagramChannelImpl` bytecode would take over on the new receiver —
    // dispatch keys on the receiver's class (`H11-1`) and the superclass walk
    // does not run for a class that declares the method with `Code`.
    let dc = crate::concrete_receiver::alloc_concrete(
        ctx,
        &["sun/nio/ch/DatagramChannelImpl"],
        "java/nio/channels/DatagramChannel",
        DC_NUM_FIELDS,
    )
    .obj;
    // Seed the real `AbstractInterruptibleChannel` / `AbstractSelectableChannel`
    // monitor and interruptor fields, exactly as `socket_channel.rs` does for
    // the two stream channels. This factory does not run the JDK constructor
    // that assigns them, so `closeLock` stayed null on every datagram channel
    // this VM ever built -- and the real `close()` bytecode opens with
    // `synchronized (closeLock)`.
    //
    // The reason this was invisible until 2026-09-05 is that nothing ran that
    // bytecode: `close()` resolved to `native_dc_close`. Then the JIT's
    // `final`-method devirtualiser bound the site straight to the JDK body,
    // and netty's `NioDatagramChannel.doClose()` began throwing
    // `NullPointerException` from the tier-up call on -- 3,487 of 4,000 closes
    // in `probes/CloseDevirtProbe.java`, each leaking a UDP socket and leaving
    // netty's `AbstractChannel.close()` to raise "close() must be invoked
    // after the channel is closed." over a channel that never closed.
    // `invoke::final_devirt_native_shadow` refuses that bind, and this seeding
    // is the SECOND, independent half -- not decoration. Measured 2026-09-05 by
    // reverting this file's datagram changes and rebuilding, one binary per arm:
    //
    //                                       screen ON   screen OFF
    //     ChannelCloseDevirtProbe datagram    0/4000    3487/4000 (first @512)
    //     ChannelStateAfterCloseCensus        0/400000  399999/400000
    //
    // ...against 0 in BOTH arms once these lines are back. So either half alone
    // covers the netty face; shipping both is deliberate, because they cover it
    // at different levels (the screen stops compiled code reaching the JDK body
    // at all; this makes the JDK body correct when something else reaches it).
    //
    // An earlier revision of this comment claimed the seeding was NOT sufficient
    // -- that a devirtualised close got one step further and died on
    // `"this.stateLock" is null` inside
    // `DatagramChannelImpl.implCloseSelectableChannel`. That was true, and
    // measured, on the tree this branch started from; a `dev` merge on the same
    // day made the `implCloseSelectableChannel` registration below win that
    // dispatch, and the claim went stale without anything in this file changing.
    // Re-measured rather than re-reasoned. Returns a possibly-relocated ref:
    // seeding allocates.
    let dc = crate::socket_channel::init_channel_locks(ctx, dc);
    // "A newly-created channel is always in blocking mode"
    // (`java.nio.channels.SelectableChannel`). Assert it rather than assume
    // it: the side tables are keyed by identity hash, and a fresh object may
    // reuse the hash of a collected channel that was switched to non-blocking.
    dc_set_blocking(ctx, dc, true);
    set_dc_fd(ctx, dc, fd_id);
    Ok(Some(Value::Object(Some(dc))))
}

/// Turn a UDP bind failure into the exception HotSpot raises for it.
///
/// `java.nio.channels.DatagramChannel.bind` surfaces an unavailable address as
/// `java.net.BindException`, and callers test for it by type — netty's
/// `DnsNameResolverTest.testAddressAlreadyInUse` asserts
/// `assertInstanceOf(BindException.class, cause.getCause())`. A plain
/// `IOException` carrying the OS text satisfies nothing that looks at the
/// type, and on Windows the text is localised on top of that.
///
/// Windows reports a clash two different ways depending on whether the caller
/// asked for `SO_REUSEADDR`: `WSAEADDRINUSE` without it, `WSAEACCES` (mapped
/// by Rust to `PermissionDenied`) with it, because the holder owns the port
/// exclusively. Both are `BindException` on HotSpot, so both are here.
pub(crate) fn dc_bind_error(_addr: &str, e: std::io::Error) -> RuntimeError {
    use std::io::ErrorKind;
    // The message is the bare strerror text, as on HotSpot 25.0.4+7
    // (`Sbr2W7TailNet.java`); the address is not part of it.
    match e.kind() {
        ErrorKind::AddrInUse => RuntimeError::BindException {
            message: "Address already in use".to_string(),
        },
        ErrorKind::AddrNotAvailable => RuntimeError::BindException {
            message: "Cannot assign requested address".to_string(),
        },
        ErrorKind::PermissionDenied => RuntimeError::BindException {
            message: "Permission denied".to_string(),
        },
        _ => RuntimeError::IOException {
            message: format!("DatagramChannel.bind: {e}"),
        },
    }
}

pub(crate) fn native_dc_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    // `DatagramChannel.bind(null)` is the JDK contract for a wildcard,
    // ephemeral UDP socket. The real JDK JNDI DNS client relies on it for TXT
    // lookups; treating the null address as a required object turned that
    // ordinary call into `NullPointerException: arg 1 is null`.
    // Decode through the same helper `connect` uses. The previous ad-hoc
    // `read_string(addr_obj)` never matched a real `InetSocketAddress` — it is
    // not a String and its slot 0 is not a host String either — so EVERY
    // explicit bind silently fell through to the `0.0.0.0:0` wildcard default.
    // `bind(new InetSocketAddress("127.0.0.1", 0))` therefore bound the
    // wildcard, and `getLocalAddress()` reported `0.0.0.0` where HotSpot
    // reports `127.0.0.1`. Keep the wildcard only for the genuinely-null
    // argument, which is the JDK contract the comment above describes.
    let addr_str = match args.get(1) {
        Some(Value::Object(Some(addr_obj))) => dc_socket_addr(ctx, *addr_obj)
            .or_else(|| ctx.read_string(*addr_obj))
            .unwrap_or_else(|| "0.0.0.0:0".to_string()),
        _ => "0.0.0.0:0".to_string(),
    };

    // `SO_REUSEADDR` is a PRE-bind option, so whatever was applied to the
    // socket being replaced does not carry over — reapply what Java asked for.
    // `dc_option_get` is the record `native_dc_set_option` keeps for exactly
    // the cases where the socket cannot answer for itself.
    let reuse = dc_option_get(ctx, this, "SO_REUSEADDR").unwrap_or(0) != 0;

    // Bind the socket this channel ALREADY has, keeping its fd id.
    //
    // This used to `close(old_fd)` and `open_udp(addr)`, which handed the
    // channel a NEW fd id. The id is the channel's identity everywhere else in
    // the VM — most importantly it is the key the NIO selector registers under
    // — and netty registers before it binds (`AbstractChannel.register0` runs
    // `javaChannel().register(selector, 0)`; `doBind` comes after). So the
    // registration was left keyed on a dead id, holding a dup of the discarded
    // socket, and `NioDatagramChannel` never received a single datagram: the
    // bind succeeded, the send worked, and inbound traffic simply never
    // reached the event loop. `PcapWriteHandlerTest`'s `udpV4*` cases and
    // `io.netty.resolver.dns`'s transport are the visible half of that.
    // A WILDCARD bind must keep the channel dual-stack. `open()` gave this
    // channel an AF_INET6 socket with `IPV6_V6ONLY` off; rebinding it to the
    // literal v4 wildcard would replace that with AF_INET and silently drop
    // the second family — see `FileDescriptorTable::udp_rebind_dual_stack`.
    // HotSpot reports `/[0:0:0:0:0:0:0:0]:port` for both `bind(null)` and
    // `bind(new InetSocketAddress("0.0.0.0", 0))`, which is what this matches.
    let wildcard_port = cratonvm_native_api::fd_table::wildcard_bind_port(&addr_str);
    if let Some(existing) = dc_fd(ctx, this) {
        let rebound = match wildcard_port {
            Some(port) => ctx.fd_table().udp_rebind_dual_stack(existing, port, reuse),
            None => ctx.fd_table().udp_rebind(existing, Some(&addr_str), reuse),
        };
        rebound.map_err(|e| dc_bind_error(&addr_str, e))?;
        if let Ok(fresh) = ctx.fd_table().udp_try_clone(existing) {
            crate::nio_selector::selector_refresh_udp(existing as i32, &fresh);
        }
        return Ok(Some(Value::Object(Some(this))));
    }

    // No socket yet (a legacy channel layout). Same wildcard rule as above.
    let fd_id = match (wildcard_port, reuse) {
        (Some(_), false) => ctx.fd_table().open_udp_dual_stack(),
        _ if reuse => ctx.fd_table().open_udp_reuse(Some(&addr_str)),
        _ => ctx.fd_table().open_udp(Some(&addr_str)),
    }
    .map_err(|e| dc_bind_error(&addr_str, e))?;

    set_dc_fd(ctx, this, fd_id);
    Ok(Some(Value::Object(Some(this))))
}

/// Render a real-JDK `java.net.InetAddress`'s NUMERIC address as a literal
/// that `std::net::ToSocketAddrs` parses without consulting a resolver —
/// dotted-quad for v4, bracketed for v6 so `format!("{host}:{port}")` stays
/// unambiguous.
///
/// Returns `None` for anything that is not a real-JDK holder layout, and for
/// an `InetSocketAddress` whose `addr` is null (an unresolved one); the caller
/// falls back to the hostname there.
///
/// # Why the caller must prefer this over the hostname
///
/// `dc_socket_addr` used to answer `InetSocketAddressHolder.hostname` first,
/// and `InetAddressHolder.hostName` after it, reaching the numeric `address`
/// int only when both were absent. That handed a NAME to
/// `UdpSocket::bind`/`send_to`, which resolves it again through the platform
/// resolver — a second, independent answer to a question Java had already
/// answered.
///
/// The two resolvers disagree on `localhost`. Windows `getaddrinfo` orders
/// `::1` first; glibc, with the stock `127.0.0.1 localhost` line ahead of
/// `::1 localhost` in `/etc/hosts`, orders `127.0.0.1` first. So
/// `DatagramChannel.bind(new InetSocketAddress("localhost", 0))` — whose
/// `InetSocketAddress` already holds a resolved `127.0.0.1` — bound `::1` on
/// Windows and `127.0.0.1` on Linux, from identical bytes. HotSpot never has
/// this divergence: `sun.nio.ch.Net.bind` takes `isa.getAddress()` and never
/// looks at the name.
///
/// Downstream that produced a destination address no test constructs.
/// Apache MINA's `NioDatagramAcceptor.localAddress()` rewrites any bound
/// `Inet6Address` for which `isIPv4CompatibleAddress()` holds into the v4
/// address in its last four bytes ("Ugly hack to workaround a problem on
/// linux", per its own comment). `::1` satisfies that predicate, and its last
/// four bytes are `[0, 0, 0, 1]` — so netty's `TestDnsServer.localAddress()`
/// answered `0.0.0.1`, every `DnsNameResolver` query went to `0.0.0.1`, and
/// Windows failed each one with `WSAENETUNREACH`.
pub(crate) fn inet_addr_literal(ctx: &dyn NativeContext, inet_addr: ObjectRef) -> Option<String> {
    // IPv6 FIRST. `Inet6Address` keeps its sixteen bytes in a separate
    // `holder6` (`Inet6Address$Inet6AddressHolder.ipaddress`), and the base
    // holder's `address` int stays 0 for it — reading that would render every
    // v6 address as `0.0.0.0`, which binds the v4 wildcard.
    if let Value::Object(Some(h6)) = ctx.get_field_by_name(inet_addr, "holder6") {
        if let Value::Object(Some(arr)) = ctx.get_field_by_name(h6, "ipaddress") {
            if ctx.array_length(arr) == 16 {
                let mut octets = [0u8; 16];
                for (i, slot) in octets.iter_mut().enumerate() {
                    match ctx.get_array_element(arr, i) {
                        Value::Int(b) => *slot = b as u8,
                        _ => return None,
                    }
                }
                // Bracketed: the caller appends `:{port}`, and a bare v6
                // literal there is ambiguous to every parser that sees it.
                return Some(format!("[{}]", std::net::Ipv6Addr::from(octets)));
            }
        }
    }
    let inet_holder = match ctx.get_field_by_name(inet_addr, "holder") {
        Value::Object(Some(h)) => h,
        _ => return None,
    };
    match ctx.get_field_by_name(inet_holder, "address") {
        Value::Int(address) => {
            Some(std::net::Ipv4Addr::from((address as u32).to_be_bytes()).to_string())
        }
        _ => None,
    }
}

/// Extract a printable host:port from both the real JDK 25 holder layout and
/// CratonVM's small synthetic InetSocketAddress layout.
pub(crate) fn dc_socket_addr(ctx: &dyn NativeContext, addr: ObjectRef) -> Option<String> {
    if let Value::Object(Some(holder)) = ctx.get_field_by_name(addr, "holder") {
        let port = match ctx.get_field_by_name(holder, "port") {
            Value::Int(port) if (0..=65_535).contains(&port) => port,
            _ => return None,
        };
        // The RESOLVED address wins over the hostname, and this ordering is
        // load-bearing — see [`inet_addr_literal`]. Java has already resolved
        // the name; handing the name back to the OS asks a SECOND resolver the
        // same question and takes whichever answer it happens to order first.
        if let Value::Object(Some(inet_addr)) = ctx.get_field_by_name(holder, "addr") {
            if let Some(host) = inet_addr_literal(ctx, inet_addr) {
                return Some(format!("{host}:{port}"));
            }
        }
        // Only an UNRESOLVED `InetSocketAddress` (`createUnresolved`, or a
        // constructor whose lookup failed) reaches here with a name and no
        // address; the name is then all there is, and the OS resolver is the
        // right place to send it.
        let hostname = match ctx.get_field_by_name(holder, "hostname") {
            Value::Object(Some(hostname)) => ctx.read_string(hostname).unwrap_or_default(),
            _ => String::new(),
        };
        if !hostname.is_empty() {
            return Some(format!("{hostname}:{port}"));
        }
        return None;
    }

    let host = match ctx.get_field(addr, 0) {
        Value::Object(Some(host)) => ctx.read_string(host)?,
        _ => return None,
    };
    let port = match ctx.get_field(addr, 1) {
        Value::Int(port) if (0..=65_535).contains(&port) => port,
        _ => return None,
    };
    Some(format!("{host}:{port}"))
}

pub(crate) fn native_dc_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    let peer = obj_arg92(args, 1)?;
    let peer = dc_socket_addr(ctx, peer).ok_or_else(|| RuntimeError::IOException {
        message: "DatagramChannel.connect: unsupported SocketAddress".into(),
    })?;
    let fd = dc_fd(ctx, this).ok_or_else(|| RuntimeError::IOException {
        message: "DatagramChannel.connect: channel has no UDP socket".into(),
    })?;
    ctx.fd_table()
        .udp_connect(fd, &peer)
        .map_err(|e| RuntimeError::IOException {
            message: format!("DatagramChannel.connect({peer}): {e}"),
        })?;
    dc_mark_connected(ctx, this);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_dc_disconnect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    if !dc_is_connected(ctx, this) {
        return Ok(Some(Value::Object(Some(this))));
    }
    if let Some(fd) = dc_fd(ctx, this) {
        ctx.fd_table()
            .udp_disconnect(fd)
            .map_err(|e| RuntimeError::IOException {
                message: format!("DatagramChannel.disconnect: {e}"),
            })?;
    }
    dc_clear_connected(ctx, this);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_dc_is_connected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    Ok(Some(Value::Int(i32::from(dc_is_connected(ctx, this)))))
}

pub(crate) fn native_dc_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    let buffer = obj_arg92(args, 1)?;
    let fd = dc_fd(ctx, this).ok_or_else(|| RuntimeError::IOException {
        message: "DatagramChannel.write: channel has no UDP socket".into(),
    })?;
    let view = bb_storage_view(ctx, buffer)?;
    let remaining = (view.lim - view.pos).max(0) as usize;
    let mut bytes = vec![0u8; remaining];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = bb_read_byte(ctx, view, view.pos as usize + index)?;
    }
    let sent =
        ctx.fd_table()
            .udp_send_connected(fd, &bytes)
            .map_err(|e| RuntimeError::IOException {
                message: format!("DatagramChannel.write: {e}"),
            })?;
    buf_set_position(ctx, buffer, view.pos + sent as i32);
    Ok(Some(Value::Int(sent as i32)))
}

pub(crate) fn native_dc_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    let buffer = obj_arg92(args, 1)?;
    let fd = dc_fd(ctx, this).ok_or_else(|| RuntimeError::IOException {
        message: "DatagramChannel.read: channel has no UDP socket".into(),
    })?;
    let pos = bb_storage_view(ctx, buffer)?.pos;
    let remaining = (bb_storage_view(ctx, buffer)?.lim - pos).max(0) as usize;
    let mut bytes = vec![0u8; remaining];
    let mut buffer_after_blocking: Option<ObjectRef> = None;
    // A datagram that is already queued cannot park: take it outside the
    // bracket (see `FileDescriptorTable::udp_recv_if_ready`).
    let recv = match ctx.fd_table().udp_recv_if_ready(fd, &mut bytes) {
        Ok(Some(received)) => Ok(received),
        Err(error) => Err(error),
        Ok(None) => {
            // A blocking-mode channel parks in recv until a datagram arrives,
            // touching no Java heap. Bracket it so a concurrent stop-the-world
            // pause does not wait forever on a mutator that never reaches a
            // safepoint, and re-sync `buffer` afterwards — a pause inside the
            // region may have moved it. The `BbView` is re-derived after the
            // region for the same reason.
            let mut held = vec![Value::Object(Some(buffer))];
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let recv = ctx.fd_table().udp_recv(fd, &mut bytes);
            ctx.end_blocking_region_refs(&mut held);
            if let Value::Object(Some(b)) = held[0] {
                buffer_after_blocking = Some(b);
            }
            recv
        }
    };
    let buffer = buffer_after_blocking.unwrap_or(buffer);
    let (received, _) = match recv {
        Ok(received) => received,
        // Non-blocking channels report zero bytes when no datagram is ready;
        // surfacing EAGAIN as IOException makes JNDI treat a normal poll as a
        // resolver-wide communication failure.
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Ok(Some(Value::Int(0)));
        }
        Err(error) => {
            return Err(RuntimeError::IOException {
                message: format!("DatagramChannel.read: {error}"),
            }
            .into());
        }
    };
    let view = bb_storage_view(ctx, buffer)?;
    bb_write_bytes(ctx, view, view.pos as usize, &bytes[..received])?;
    buf_set_position(ctx, buffer, view.pos + received as i32);
    Ok(Some(Value::Int(received as i32)))
}

/// `DatagramChannel.write(ByteBuffer[], int, int) -> long` — the gathering form.
///
/// NOT a loop over the single-buffer native, and that is the whole difficulty
/// `W7-9` §6 named when it declined to implement this ("not composable from the
/// single-buffer natives that do exist: for a datagram channel a scattering
/// read consumes exactly one datagram"). A gathering write must produce ONE
/// datagram from all the source buffers; writing each buffer in turn would put
/// N datagrams on the wire and a receiver would see N messages.
///
/// So the buffers are concatenated VM-side into one payload — the same
/// `bb_storage_view` / `bb_read_byte` machinery `native_dc_write` uses — and
/// sent once. Positions are then advanced by exactly what the socket took, in
/// buffer order, which is what `GatheringByteChannel` specifies.
///
/// Before this, the JDK's own `DatagramChannelImpl.write(ByteBuffer[],int,int)`
/// bytecode ran against our synthetic channel and died on
/// `NullPointerException: … because "this.writeLock" is null` — our own
/// uninitialised state surfacing from inside library code, where the caller
/// expected either bytes or a named refusal. Netty's datagram path uses this
/// overload (`GatheringByteChannel`), which is why `W7-9` called it "genuinely
/// absent, and genuinely reachable".
pub(crate) fn native_dc_write_gathering(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    let srcs = obj_arg92(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = ctx.array_length(srcs) as i32;
    // `Objects.checkFromIndexSize`, the JDK's own precondition.
    if offset < 0 || length < 0 || offset > count - length {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!(
                "offset {offset}, length {length}, array length {count}"
            )),
        }
        .into());
    }
    let fd = dc_fd(ctx, this).ok_or_else(|| RuntimeError::IOException {
        message: "DatagramChannel.write: channel has no UDP socket".into(),
    })?;

    let mut payload: Vec<u8> = Vec::new();
    for index in offset..offset + length {
        let Value::Object(Some(buffer)) = ctx.get_array_element(srcs, index as usize) else {
            continue;
        };
        let view = bb_storage_view(ctx, buffer)?;
        let remaining = (view.lim - view.pos).max(0) as usize;
        for step in 0..remaining {
            payload.push(bb_read_byte(ctx, view, view.pos as usize + step)?);
        }
    }
    let sent = ctx
        .fd_table()
        .udp_send_connected(fd, &payload)
        .map_err(|e| RuntimeError::IOException {
            message: format!("DatagramChannel.write: {e}"),
        })?;

    // Advance each source by the part of it that actually went out. A short
    // send leaves the tail buffers untouched rather than silently consumed.
    let mut left = sent;
    for index in offset..offset + length {
        if left == 0 {
            break;
        }
        let Value::Object(Some(buffer)) = ctx.get_array_element(srcs, index as usize) else {
            continue;
        };
        let view = bb_storage_view(ctx, buffer)?;
        let remaining = (view.lim - view.pos).max(0) as usize;
        let taken = remaining.min(left);
        buf_set_position(ctx, buffer, view.pos + taken as i32);
        left -= taken;
    }
    Ok(Some(Value::Long(sent as i64)))
}

/// `DatagramChannel.read(ByteBuffer[], int, int) -> long` — the scattering form.
///
/// The mirror of [`native_dc_write_gathering`], and it has the same reason for
/// not being a loop: a scattering read consumes exactly ONE datagram and
/// spreads it across the buffers. Reading per-buffer would consume one datagram
/// each and silently drop whatever did not fit the first.
///
/// Bytes beyond the buffers' total remaining are DISCARDED, which is the
/// datagram contract — `ScatteringByteChannel` says the rest of the datagram is
/// dropped, not held for the next read.
pub(crate) fn native_dc_read_scattering(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    let dsts = obj_arg92(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = ctx.array_length(dsts) as i32;
    if offset < 0 || length < 0 || offset > count - length {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!(
                "offset {offset}, length {length}, array length {count}"
            )),
        }
        .into());
    }
    let fd = dc_fd(ctx, this).ok_or_else(|| RuntimeError::IOException {
        message: "DatagramChannel.read: channel has no UDP socket".into(),
    })?;

    let mut capacity = 0usize;
    for index in offset..offset + length {
        let Value::Object(Some(buffer)) = ctx.get_array_element(dsts, index as usize) else {
            continue;
        };
        let view = bb_storage_view(ctx, buffer)?;
        capacity += (view.lim - view.pos).max(0) as usize;
    }
    let mut bytes = vec![0u8; capacity];
    // Same blocking-region bracket and re-sync as `native_dc_read`: the recv
    // parks without touching the Java heap, and a stop-the-world pause during
    // it may move the destination ARRAY.
    let mut dsts_after_blocking: Option<ObjectRef> = None;
    let recv = match ctx.fd_table().udp_recv_if_ready(fd, &mut bytes) {
        Ok(Some(received)) => Ok(received),
        Err(error) => Err(error),
        Ok(None) => {
            let mut held = vec![Value::Object(Some(dsts))];
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let recv = ctx.fd_table().udp_recv(fd, &mut bytes);
            ctx.end_blocking_region_refs(&mut held);
            if let Value::Object(Some(a)) = held[0] {
                dsts_after_blocking = Some(a);
            }
            recv
        }
    };
    let dsts = dsts_after_blocking.unwrap_or(dsts);
    let (received, _) = match recv {
        Ok(received) => received,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Ok(Some(Value::Long(0)));
        }
        Err(error) => {
            return Err(RuntimeError::IOException {
                message: format!("DatagramChannel.read: {error}"),
            }
            .into());
        }
    };

    let mut placed = 0usize;
    for index in offset..offset + length {
        if placed >= received {
            break;
        }
        let Value::Object(Some(buffer)) = ctx.get_array_element(dsts, index as usize) else {
            continue;
        };
        let view = bb_storage_view(ctx, buffer)?;
        let remaining = (view.lim - view.pos).max(0) as usize;
        let take = remaining.min(received - placed);
        bb_write_bytes(ctx, view, view.pos as usize, &bytes[placed..placed + take])?;
        buf_set_position(ctx, buffer, view.pos + take as i32);
        placed += take;
    }
    Ok(Some(Value::Long(placed as i64)))
}

/// `DatagramChannel.send(ByteBuffer, SocketAddress) -> int`.
///
/// Previously registered by `datagram.rs` against its own registry, which
/// `open()`/`bind()` never populated — every send threw
/// `IOException: send: no socket id`. It belongs with the rest of the
/// fd_table-backed family here.
pub(crate) fn native_dc_send(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    let buffer = obj_arg92(args, 1)?;
    let target = obj_arg92(args, 2)?;
    let fd = dc_fd(ctx, this).ok_or_else(|| RuntimeError::IOException {
        message: "DatagramChannel.send: channel has no UDP socket".into(),
    })?;
    let dest = dc_socket_addr(ctx, target).ok_or_else(|| RuntimeError::IOException {
        message: "DatagramChannel.send: target SocketAddress unparsable".into(),
    })?;
    // SSRF gate, matching the TCP path and the previous `datagram.rs` send:
    // refuse datagrams to blocked ranges (link-local cloud metadata) before
    // the real sendto(2).
    if let Ok(parsed) = dest.parse::<std::net::SocketAddr>() {
        crate::datagram::check_outbound_target(parsed)?;
    }

    let view = bb_storage_view(ctx, buffer)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos >= lim {
        return Ok(Some(Value::Int(0)));
    }
    let mut bytes = vec![0u8; (lim - pos) as usize];
    bb_read_bytes(ctx, view, pos as usize, &mut bytes)?;

    // send can park on a full local socket buffer — same GC-blocking protocol
    // as the receive path above.
    let mut buffer_after_blocking: Option<ObjectRef> = None;
    let sent = match ctx.fd_table().udp_send_if_ready(fd, &bytes, &dest) {
        Ok(Some(n)) => Ok(n),
        Err(error) => Err(error),
        Ok(None) => {
            let mut held = vec![Value::Object(Some(buffer))];
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let sent = ctx.fd_table().udp_send(fd, &bytes, &dest);
            ctx.end_blocking_region_refs(&mut held);
            if let Value::Object(Some(b)) = held[0] {
                buffer_after_blocking = Some(b);
            }
            sent
        }
    };
    let buffer = buffer_after_blocking.unwrap_or(buffer);
    let sent = match sent {
        Ok(n) => n,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Ok(Some(Value::Int(0)));
        }
        Err(error) => {
            return Err(RuntimeError::IOException {
                message: format!("DatagramChannel.send to {dest}: {error}"),
            }
            .into());
        }
    };
    buf_set_position(ctx, buffer, pos + sent as i32);
    Ok(Some(Value::Int(sent as i32)))
}

/// Build a real-JDK-layout InetSocketAddress for a received IPv4 datagram.
/// `DnsClient.blockingReceive` compares this object with its connected target,
/// so a generic SocketAddress or a flat synthetic layout is insufficient.
pub(crate) fn dc_inet_socket_address(
    ctx: &mut dyn NativeContext,
    source: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let (host, port) = source
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse::<i32>().ok().map(|port| (host, port)))
        .unwrap_or(("0.0.0.0", 0));
    let packed = host
        .parse::<std::net::Ipv4Addr>()
        .map(|ip| i32::from_be_bytes(ip.octets()))
        .unwrap_or(0);

    // GC: six allocations, four references, and every store is to an object
    // minted before the allocation that follows it. `inet` in particular is
    // stored into `socket_holder` FIVE allocations after it was created.
    let inet = try_alloc_synthetic(ctx, "java/net/Inet4Address", 2)?;
    let inet_pin = ctx.pin_native_root(inet);
    let inet_holder = try_alloc_synthetic(ctx, "java/net/InetAddress$InetAddressHolder", 3)?;
    let holder_pin = ctx.pin_native_root(inet_holder);
    let host_string = ctx.create_string(host);
    let inet_holder = ctx.read_native_pin(holder_pin, inet_holder);
    ctx.set_field_by_name(inet_holder, "hostName", Value::Object(Some(host_string)));
    ctx.set_field_by_name(inet_holder, "address", Value::Int(packed));
    ctx.set_field_by_name(inet_holder, "family", Value::Int(1));
    let inet = ctx.read_native_pin(inet_pin, inet);
    ctx.set_field_by_name(inet, "holder", Value::Object(Some(inet_holder)));

    // Width 1, not 2: every field below is written BY NAME, so the request only
    // ever needed to cover what the class declares, and real
    // `java.net.InetSocketAddress` declares exactly one instance field
    // (`javap -p`, JDK 25.0.3.9: `private final transient
    // InetSocketAddress$InetSocketAddressHolder holder`; its superclass
    // `java.net.SocketAddress` declares only a static `serialVersionUID`).
    // The second slot sat past the declared width with no writer and no reader.
    // No-op in synthetic-JDK mode, where `class_manager` fabricates this class
    // with three fields and `alloc_object` clamps any request up to them.
    // W7-66-live-over-allocations.md.
    let socket = try_alloc_synthetic(ctx, "java/net/InetSocketAddress", 1)?;
    let socket_pin = ctx.pin_native_root(socket);
    let socket_holder =
        try_alloc_synthetic(ctx, "java/net/InetSocketAddress$InetSocketAddressHolder", 3)?;
    let sh_pin = ctx.pin_native_root(socket_holder);
    let socket_host = ctx.create_string(host);
    let socket_holder = ctx.read_native_pin(sh_pin, socket_holder);
    let inet = ctx.read_native_pin(inet_pin, inet);
    ctx.set_field_by_name(socket_holder, "hostname", Value::Object(Some(socket_host)));
    ctx.set_field_by_name(socket_holder, "addr", Value::Object(Some(inet)));
    ctx.set_field_by_name(socket_holder, "port", Value::Int(port));
    let socket = ctx.read_native_pin(socket_pin, socket);
    ctx.set_field_by_name(socket, "holder", Value::Object(Some(socket_holder)));
    ctx.unpin_native_roots(inet_pin);
    Ok(socket)
}

pub(crate) fn native_dc_receive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    let bb = obj_arg92(args, 1)?;

    if dc_fd(ctx, this).is_none() {
        return Err(RuntimeError::IOException {
            message: "DatagramChannel is closed".into(),
        }
        .into());
    }

    let Some(fd_id) = dc_fd(ctx, this) else {
        return Ok(Some(Value::Object(None)));
    };

    let view = bb_storage_view(ctx, bb)?;
    let pos = view.pos;
    let lim = view.lim;
    let remaining = (lim - pos) as usize;
    if remaining == 0 {
        return Ok(Some(Value::Object(None)));
    }

    let mut buf = vec![0u8; remaining];
    let mut bb_after_blocking: Option<ObjectRef> = None;
    // A datagram that is already queued cannot park the thread, so take it
    // WITHOUT the blocking-region bracket below (a TLAB retire, a root-snapshot
    // deposit and a post-block re-sync: ~45 us, against ~2 us for the receive
    // itself). Only an empty socket falls through to the bracketed wait.
    let recv = match ctx.fd_table().udp_recv_if_ready(fd_id, &mut buf) {
        Ok(Some(received)) => Ok(received),
        Err(error) => Err(error),
        Ok(None) => {
            // gc-common w16-c: the recv parks (a blocking channel waits for a
            // datagram with no bound) and used to do so OUTSIDE a blocking
            // region, so a stop-the-world collection requested meanwhile waited
            // for this thread until a datagram arrived. Same bracket and
            // re-sync as `native_dc_read`: the recv touches no Java heap, and a
            // pause during it may move the buffer, so `bb` comes back through
            // `held` and its view is rebuilt.
            let mut held = vec![Value::Object(Some(bb))];
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let recv = ctx.fd_table().udp_recv(fd_id, &mut buf);
            ctx.end_blocking_region_refs(&mut held);
            if let Value::Object(Some(b)) = held[0] {
                bb_after_blocking = Some(b);
            }
            recv
        }
    };
    let bb = bb_after_blocking.unwrap_or(bb);
    let (n, source_addr) = match recv {
        Ok(received) => received,
        // NIO returns null, rather than throwing, when a non-blocking
        // DatagramChannel has no packet ready yet.
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Ok(Some(Value::Object(None)));
        }
        Err(error) => {
            return Err(RuntimeError::IOException {
                message: format!("receive: {error}"),
            }
            .into());
        }
    };

    // Re-derived after the region: the pre-region view names the buffer's
    // backing array as it was before any pause moved it.
    let view = bb_storage_view(ctx, bb)?;
    bb_write_bytes(ctx, view, pos as usize, &buf[..n])?;
    buf_set_position(ctx, bb, pos + n as i32);

    let sa = dc_inet_socket_address(ctx, &source_addr)?;
    Ok(Some(Value::Object(Some(sa))))
}

pub(crate) fn native_dc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    // Closing a channel cancels and deregisters its selection keys. This close
    // never did: a closed `DatagramChannel` stayed in every selector it was
    // registered with -- still polled through the selector's UDP clone, still
    // listed by `Selector.keys()` -- and its `sk_table` row stayed a GC root
    // for the life of the VM (gc-common w11-b, `common-w10b`). The fd slot is
    // swept only for a key whose row names THIS channel.
    let selector_fd = dc_fd(ctx, this).map(|fd| fd as i32);
    crate::nio_selector::deregister_channel_everywhere(ctx, this, selector_fd, Vec::new());
    dc_clear_connected(ctx, this);
    // Drop the blocking-mode override too, so the identity hash this channel
    // releases cannot carry "non-blocking" over to a later channel that
    // happens to be allocated with the same hash.
    dc_set_blocking(ctx, this, true);
    // ... and the `socket()` adaptor, for the same staleness reason plus one
    // more: that row holds two heap references and is a GC root, so leaving it
    // behind keeps a closed channel and its adaptor alive forever.
    dc_socket_cache_clear(ctx, this);
    // ... and the recorded socket options, same staleness reason again.
    dc_option_clear(ctx, this);
    if let Some(fd_id) = remove_dc_fd(ctx, this) {
        // ... and its multicast-group rows (JIT r23 wave 3 review), before
        // the socket goes.
        crate::datagram::forget_joined_groups(fd_id);
        let _ = ctx.fd_table().close(fd_id);
    }
    // ... and any socket the weak-row sweep parked for this VM (gc-common
    // w16-c, `io_orphan_fds`).
    close_orphan_fds(ctx);
    // `native_dc_is_open` answers from `dc_fd` above, but the JDK's own
    // `AbstractInterruptibleChannel.isOpen()` reads a `closed` field this
    // family never wrote — and a JIT-compiled caller runs that body, because
    // `isOpen()` is `final` and the devirtualiser takes it. See
    // `socket_channel::mark_jdk_channel_closed` for the netty failure this
    // divergence produced.
    crate::socket_channel::mark_jdk_channel_closed(ctx, this);
    Ok(None)
}

pub(crate) fn native_dc_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    let open = dc_fd(ctx, this).is_some();
    Ok(Some(Value::Int(if open { 1 } else { 0 })))
}

pub(crate) fn native_dc_configure_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    let blocking = match args.get(1) {
        Some(Value::Int(b)) => *b != 0,
        _ => true,
    };
    // Record the mode FIRST, and unconditionally. `isBlocking()` is answered
    // from this table (see `native_dc_is_blocking`), and it must reflect what
    // the caller asked for even when there is no fd to flip — a channel this
    // family never opened, or one already closed. This flip used to touch only
    // the OS socket, so `configureBlocking(false)` left `isBlocking()`
    // answering the JDK's initial `true` and Java code could not see the mode
    // it had just selected.
    dc_set_blocking(ctx, this, blocking);
    let Some(fd_id) = dc_fd(ctx, this) else {
        return Ok(Some(Value::Object(Some(this))));
    };
    let _ = ctx.fd_table().udp_set_nonblocking(fd_id, !blocking);
    Ok(Some(Value::Object(Some(this))))
}

/// `DatagramChannel.isBlocking()` for the fd-table-backed channel family.
///
/// Registered HERE, next to the `configureBlocking` that actually runs, rather
/// than left to `net_channels`'s phase-72 entry or `nio_native`'s
/// `t16_dc_is_blocking`. Native registration is last-wins
/// (`native-api/src/registry.rs`) and `register_io_natives` re-runs
/// `register_datagram_channel` AFTER `register_t16_channel_overrides`, so this
/// family's `open`/`configureBlocking`/`close` win in every build. Both other
/// readers answer from object slot 3 — the retired 5-field synthetic layout's
/// blocking flag, which on the 3-slot fd-table channel this family allocates
/// is an out-of-bounds read, and on a real-JDK `DatagramChannel` belongs to
/// the JDK's own private layout. Pairing the reader with the writer is what
/// keeps the two from disagreeing.
pub(crate) fn native_dc_is_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    Ok(Some(Value::Int(i32::from(dc_is_blocking(ctx, this)))))
}

/// `DatagramChannel.remoteAddress()` / `getRemoteAddress()`.
///
/// The same per-method hole as `localAddress()` above, found by walking the
/// whole adaptor surface (`AdaptorAudit`) instead of waiting for the next
/// application to hit it: it is declared on `sun.nio.ch.DatagramChannelImpl`,
/// not on the abstract `java.nio.channels.DatagramChannel` this VM
/// instantiates, and `sun.nio.ch.DatagramSocketAdaptor`'s real bytecode calls
/// it from `getRemoteSocketAddress()` and `getPort()`:
///
/// ```text
///   NoSuchMethodError: java.nio.channels.DatagramChannel.remoteAddress()
///                      Ljava/net/InetSocketAddress;
/// ```
///
/// NOTE THE DESCRIPTOR, again: like `localAddress()`, the `DatagramChannelImpl`
/// spelling returns the NARROWER `java.net.InetSocketAddress`. Both are
/// registered.
///
/// `null` for an unconnected channel is the JDK's own answer, not a failure —
/// `getRemoteSocketAddress()` returns null and `getPort()` returns -1 there.
pub(crate) fn native_dc_remote_addr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = dc_receiver_channel(ctx, obj_arg92(args, 0)?);
    if !dc_is_connected(ctx, this) {
        return Ok(Some(Value::Object(None)));
    }
    let addr = dc_fd(ctx, this).and_then(|fd| ctx.fd_table().udp_peer_addr(fd).ok());
    match addr {
        Some(addr) => dc_build_inet_socket_address(ctx, &addr),
        None => Ok(Some(Value::Object(None))),
    }
}

/// Build a real `java.net.InetSocketAddress` from a `host:port` string.
///
/// Shared by the local- and remote-address bridges. Goes through the real
/// `(String,int)` constructor rather than writing slots by hand — see
/// [`native_dc_local_addr`] for what the hand-written two-slot form cost.
pub(crate) fn dc_build_inet_socket_address(ctx: &mut dyn NativeContext, addr: &str) -> MethodCallResult {
    let (host, port) = addr
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse::<i32>().ok().map(|port| (host, port)))
        .unwrap_or(("0.0.0.0", 0));
    let host_s = ctx.create_string(host);
    let host_pin = ctx.pin_native_root(host_s);
    let host_s = ctx.read_native_pin(host_pin, host_s);
    let built = ctx.new_object_initialized(
        "java/net/InetSocketAddress",
        "(Ljava/lang/String;I)V",
        &[Value::Object(Some(host_s)), Value::Int(port)],
    );
    ctx.unpin_native_roots(host_pin);
    match built {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_dc_local_addr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg92(args, 0)?;
    // Phase T16 can allocate an older channel layout that does not retain the
    // fd-table id in field zero. The JDK contract after bind(null) is still a
    // non-null wildcard local address; use port zero when that legacy layout
    // cannot expose its ephemeral port rather than returning null to JNDI.
    let addr = dc_fd(ctx, this)
        .and_then(|fd| ctx.fd_table().udp_local_addr(fd).ok())
        .unwrap_or_else(|| "0.0.0.0:0".to_string());
    // Build the address through the real constructor rather than writing the
    // legacy two-slot layout by hand. The hand-written form put a bare host
    // String in slot 0 — where a real-layout `InetSocketAddress` keeps its
    // `holder` — so `getAddress()` fell through to an out-of-range slot read
    // and answered **null** while `isUnresolved()` still answered **false**.
    // That is the same broken pair that let `ServerSocket.bind` walk past its
    // unresolved-address guard and NPE inside `sun.nio.ch.Net.bind`; here it
    // simply meant `DatagramChannel.getLocalAddress().getAddress()` was null on
    // a channel that was demonstrably bound. `(Ljava/lang/String;I)V` resolves
    // the numeric literal and populates the holder, so both answers agree and
    // match HotSpot. (That construction now lives in
    // `dc_build_inet_socket_address`, shared with the remote-address bridge.)
    dc_build_inet_socket_address(ctx, &addr)
}


/// The `FileOutputStream` write natives, with the `jdk.FileWrite` the JDK's own
/// `FileOutputStream.traceWrite` commits (SBR5 lane jfr; these natives shadow
/// that bytecode in `--compatible`). `ctx.jfr_vm_ticks()` is `0` unless a JFR
/// recording is running, so a recording-free run pays one relaxed load.
pub(crate) fn native_fos_write_byte_jfr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if !cratonvm_native_api::jfr_recording() {
        return native_fos_write_byte(ctx, args);
    }
    let t0 = ctx.jfr_vm_ticks();
    let r = native_fos_write_byte(ctx, args);
    if t0 != 0 && r.is_ok() {
        if let Some(Value::Object(Some(this))) = args.first() {
            ctx.jfr_file_write_done(*this, 1, t0);
        }
    }
    r
}

pub(crate) fn native_fos_write_bytes_jfr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if !cratonvm_native_api::jfr_recording() {
        return native_fos_write_bytes(ctx, args);
    }
    let t0 = ctx.jfr_vm_ticks();
    let r = native_fos_write_bytes(ctx, args);
    if t0 != 0 && r.is_ok() {
        if let (Some(Value::Object(Some(this))), Some(Value::Int(len))) =
            (args.first(), args.get(3))
        {
            ctx.jfr_file_write_done(*this, i64::from(*len), t0);
        }
    }
    r
}

pub(crate) fn native_fos_write_byte_array_jfr(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if !cratonvm_native_api::jfr_recording() {
        return native_fos_write_byte_array(ctx, args);
    }
    let t0 = ctx.jfr_vm_ticks();
    let r = native_fos_write_byte_array(ctx, args);
    if t0 != 0 && r.is_ok() {
        if let (Some(Value::Object(Some(this))), Some(Value::Object(Some(arr)))) =
            (args.first(), args.get(1))
        {
            let len = ctx.array_length(*arr) as i64;
            ctx.jfr_file_write_done(*this, len, t0);
        }
    }
    r
}

