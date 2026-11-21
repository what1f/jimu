//! Dedicated QuickJS worker thread managing VM isolation, interrupts, timeouts, and atomic reload.

use crate::prelude::PRELUDE_SOURCE;
use crate::types::{
    HookKind, JsExtensionError, RegistrySnapshot, ResolvedSource, parse_registry_snapshot,
};
use futures_channel::oneshot;
use pikra_ai::CancelToken;
use rquickjs::context::EvalOptions;
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::promise::PromiseState;
use rquickjs::{
    Context, Ctx, Error as JsError, Exception, Function, Module, Object, Persistent, Promise,
    Runtime, Value,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Thread stack size for the QuickJS worker thread (comfortably above QuickJS's JS stack guard).
const WORKER_THREAD_STACK_BYTES: usize = 4 * 1024 * 1024;

/// Number of microtask jobs executed between explicit deadline/cancellation checks.
const JOBS_PER_POLL: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct VmLimits {
    pub memory_limit_bytes: usize,
    pub max_stack_size_bytes: usize,
    pub timeout_ms: u64,
}

pub(crate) enum WorkerCmd {
    Reload {
        sources: Vec<ResolvedSource>,
        reply: mpsc::Sender<Result<RegistrySnapshot, JsExtensionError>>,
    },
    InvokeTool {
        name: String,
        tool_call_id: String,
        args_json: String,
        cancel: CancelToken,
        reply: oneshot::Sender<Result<String, JsExtensionError>>,
    },
    InvokeHook {
        hook: HookKind,
        payload_json: String,
        cancel: CancelToken,
        reply: oneshot::Sender<Result<Option<String>, JsExtensionError>>,
    },
    Shutdown,
}

pub(crate) struct WorkerHandle {
    pub tx: mpsc::Sender<WorkerCmd>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl WorkerHandle {
    pub(crate) fn spawn(
        limits: VmLimits,
        initial_sources: Vec<ResolvedSource>,
    ) -> Result<(Self, RegistrySnapshot), JsExtensionError> {
        let (tx, rx) = mpsc::channel::<WorkerCmd>();
        let (init_tx, init_rx) = mpsc::channel::<Result<RegistrySnapshot, JsExtensionError>>();

        let join = thread::Builder::new()
            .name("pikra-js-vm".to_string())
            .stack_size(WORKER_THREAD_STACK_BYTES)
            .spawn(move || {
                run_worker_loop(limits, initial_sources, init_tx, rx);
            })
            .map_err(|err| {
                JsExtensionError::RuntimeUnavailable(format!(
                    "failed to spawn QuickJS worker thread: {err}"
                ))
            })?;

        let snapshot = init_rx.recv().map_err(|_| {
            JsExtensionError::RuntimeUnavailable(
                "QuickJS worker exited during initialization".to_string(),
            )
        })??;

        Ok((
            Self {
                tx,
                join: Mutex::new(Some(join)),
            },
            snapshot,
        ))
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.tx.send(WorkerCmd::Shutdown);
        if let Some(handle) = self.join.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

#[derive(Default)]
struct InterruptInner {
    deadline: Option<Instant>,
    timeout_ms: u64,
    cancel: Option<CancelToken>,
}

#[derive(Default)]
struct InterruptState {
    inner: Mutex<InterruptInner>,
}

impl InterruptState {
    fn arm(&self, timeout_ms: u64, cancel: Option<CancelToken>) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.timeout_ms = timeout_ms;
        guard.deadline = if timeout_ms > 0 {
            Some(Instant::now() + Duration::from_millis(timeout_ms))
        } else {
            None
        };
        guard.cancel = cancel;
    }

    fn disarm(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.deadline = None;
        guard.cancel = None;
    }

    fn should_interrupt(&self) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return true;
        }
        guard.deadline.is_some_and(|d| Instant::now() >= d)
    }

    fn check(&self) -> Option<JsExtensionError> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Some(JsExtensionError::Cancelled);
        }
        if guard.deadline.is_some_and(|d| Instant::now() >= d) {
            return Some(JsExtensionError::Timeout {
                timeout_ms: guard.timeout_ms,
            });
        }
        None
    }
}

struct InterruptGuard<'a>(&'a InterruptState);

impl Drop for InterruptGuard<'_> {
    fn drop(&mut self) {
        self.0.disarm();
    }
}

/// Filesystem resolver for relative ESM imports from file-backed JS extensions.
struct FsModuleResolver;

impl Resolver for FsModuleResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        let candidate = if name.starts_with('.') {
            let base_dir = Path::new(base).parent().unwrap_or_else(|| Path::new("."));
            base_dir.join(name)
        } else {
            PathBuf::from(name)
        };

        let probes = [
            candidate.clone(),
            candidate.with_extension("js"),
            candidate.with_extension("mjs"),
            candidate.join("index.js"),
            candidate.join("index.mjs"),
        ];

        for probe in probes {
            if probe.is_file() {
                let canonical = fs::canonicalize(&probe).unwrap_or(probe);
                return Ok(canonical.display().to_string());
            }
        }

        Err(JsError::new_resolving(base, name))
    }
}

/// Filesystem loader for relative ESM imports from file-backed JS extensions.
struct FsModuleLoader;

impl Loader for FsModuleLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        let source = fs::read_to_string(name).map_err(|_| JsError::new_loading(name))?;
        Module::declare(ctx.clone(), name, source)
    }
}

/// Self-contained QuickJS VM instance.
///
/// Field declaration order is significant: `Persistent` handles drop BEFORE `context`,
/// which drops BEFORE `runtime`.
struct VmInstance {
    runtime_obj: Persistent<Object<'static>>,
    context: Context,
    runtime: Runtime,
    interrupt: Arc<InterruptState>,
    timeout_ms: u64,
}

impl VmInstance {
    fn build(
        limits: &VmLimits,
        sources: &[ResolvedSource],
    ) -> Result<(Self, RegistrySnapshot), JsExtensionError> {
        let runtime = Runtime::new().map_err(|err| {
            JsExtensionError::RuntimeUnavailable(format!("failed to create QuickJS runtime: {err}"))
        })?;
        runtime.set_memory_limit(limits.memory_limit_bytes);
        runtime.set_max_stack_size(limits.max_stack_size_bytes);
        runtime.set_loader(FsModuleResolver, FsModuleLoader);

        let interrupt = Arc::new(InterruptState::default());
        let int_ref = Arc::clone(&interrupt);
        runtime.set_interrupt_handler(Some(Box::new(move || int_ref.should_interrupt())));

        let context = Context::full(&runtime).map_err(|err| {
            JsExtensionError::RuntimeUnavailable(format!("failed to create QuickJS context: {err}"))
        })?;

        interrupt.arm(limits.timeout_ms, None);
        let _guard = InterruptGuard(&interrupt);

        // 1. Evaluate prelude and capture the internal runtime object.
        let runtime_obj = context.with(
            |ctx| -> Result<Persistent<Object<'static>>, JsExtensionError> {
                let mut opts = EvalOptions::default();
                opts.global = true;
                opts.strict = true;
                opts.filename = Some("pikra-js-prelude.js".to_string());

                let init_fn: Function<'_> = ctx
                    .eval_with_options(PRELUDE_SOURCE, opts)
                    .map_err(|err| classify_js_error(&ctx, err, &interrupt))?;
                let obj: Object<'_> = init_fn
                    .call(())
                    .map_err(|err| classify_js_error(&ctx, err, &interrupt))?;
                Ok(Persistent::save(&ctx, obj))
            },
        )?;

        let mut vm = Self {
            runtime_obj,
            context,
            runtime,
            interrupt: Arc::clone(&interrupt),
            timeout_ms: limits.timeout_ms,
        };

        // 2. Load each extension source in order.
        let mut loaded_names = Vec::with_capacity(sources.len());
        for source in sources {
            vm.eval_extension_source(source)?;
            loaded_names.push(source.name.clone());
        }

        // 3. Inspect registered tools and active hooks.
        let inspect_json = vm.context.with(|ctx| -> Result<String, JsExtensionError> {
            let rt_obj = vm
                .runtime_obj
                .clone()
                .restore(&ctx)
                .map_err(|err| JsExtensionError::Script(err.to_string()))?;
            let inspect_fn: Function<'_> = rt_obj
                .get("inspect")
                .map_err(|err| classify_js_error(&ctx, err, &vm.interrupt))?;
            inspect_fn
                .call(())
                .map_err(|err| classify_js_error(&ctx, err, &vm.interrupt))
        })?;

        let snapshot = parse_registry_snapshot(&inspect_json, loaded_names)?;
        Ok((vm, snapshot))
    }

    fn eval_extension_source(&mut self, source: &ResolvedSource) -> Result<(), JsExtensionError> {
        enum EvalOutcome {
            Done,
            PendingModule {
                eval_promise: Persistent<Promise<'static>>,
                namespace: Persistent<Object<'static>>,
            },
            DefaultFn(Persistent<Function<'static>>),
        }

        let outcome = self
            .context
            .with(|ctx| -> Result<EvalOutcome, JsExtensionError> {
                match Module::declare(ctx.clone(), source.name.as_str(), source.code.as_str()) {
                    Ok(declared) => {
                        let (evaluated, eval_promise) = declared
                            .eval()
                            .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;

                        match eval_promise.state() {
                            PromiseState::Pending => {
                                // Do NOT read `default` yet: top-level `await` leaves exports in
                                // TDZ until `eval_promise` settles. Save the live namespace object.
                                let ns = evaluated
                                    .namespace()
                                    .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                                Ok(EvalOutcome::PendingModule {
                                    eval_promise: Persistent::save(&ctx, eval_promise),
                                    namespace: Persistent::save(&ctx, ns),
                                })
                            }
                            PromiseState::Resolved => {
                                let _ = eval_promise.result::<Value<'_>>();
                                let ns = evaluated
                                    .namespace()
                                    .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                                let default_fn =
                                    extract_default_fn_from_ns(&ctx, &ns, &self.interrupt)?;
                                Ok(match default_fn {
                                    Some(f) => EvalOutcome::DefaultFn(Persistent::save(&ctx, f)),
                                    None => EvalOutcome::Done,
                                })
                            }
                            PromiseState::Rejected => {
                                let err = eval_promise
                                    .result::<Value<'_>>()
                                    .expect("rejected promise has result")
                                    .unwrap_err();
                                Err(classify_js_error(&ctx, err, &self.interrupt))
                            }
                        }
                    }
                    Err(mod_err) => {
                        // Only fall back to classic script mode if the source does not use ESM syntax.
                        if looks_like_esm(&source.code) {
                            return Err(classify_js_error(&ctx, mod_err, &self.interrupt));
                        }
                        // Consume any pending exception from the failed module parse.
                        let _ = ctx.catch();
                        let mut opts = EvalOptions::default();
                        opts.global = true;
                        opts.strict = false;
                        opts.filename = Some(source.name.clone());
                        ctx.eval_with_options::<Value<'_>, _>(source.code.as_str(), opts)
                            .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                        Ok(EvalOutcome::Done)
                    }
                }
            })?;

        let default_fn = match outcome {
            EvalOutcome::Done => {
                drain_jobs(&self.runtime, &self.interrupt)?;
                None
            }
            EvalOutcome::DefaultFn(f) => Some(f),
            EvalOutcome::PendingModule {
                eval_promise,
                namespace,
            } => {
                if let Err(err) = self.settle_promise_void(eval_promise) {
                    drop(namespace);
                    return Err(err);
                }
                self.context.with(|ctx| {
                    let ns = namespace
                        .restore(&ctx)
                        .map_err(|err| JsExtensionError::Script(err.to_string()))?;
                    let func = extract_default_fn_from_ns(&ctx, &ns, &self.interrupt)?;
                    Ok(func.map(|f| Persistent::save(&ctx, f)))
                })?
            }
        };

        if let Some(default_fn) = default_fn {
            let pending = self.context.with(
                |ctx| -> Result<Option<Persistent<Promise<'static>>>, JsExtensionError> {
                    let rt_obj = self
                        .runtime_obj
                        .clone()
                        .restore(&ctx)
                        .map_err(|err| JsExtensionError::Script(err.to_string()))?;
                    let api: Value<'_> = rt_obj
                        .get("api")
                        .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                    let func = default_fn
                        .restore(&ctx)
                        .map_err(|err| JsExtensionError::Script(err.to_string()))?;
                    let ret: Value<'_> = func
                        .call((api,))
                        .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                    if let Some(promise) = ret.into_promise() {
                        match promise.state() {
                            PromiseState::Pending => Ok(Some(Persistent::save(&ctx, promise))),
                            PromiseState::Resolved => {
                                let _ = promise.result::<Value<'_>>();
                                Ok(None)
                            }
                            PromiseState::Rejected => {
                                let err = promise
                                    .result::<Value<'_>>()
                                    .expect("rejected promise has result")
                                    .unwrap_err();
                                Err(classify_js_error(&ctx, err, &self.interrupt))
                            }
                        }
                    } else {
                        Ok(None)
                    }
                },
            )?;
            if let Some(promise) = pending {
                self.settle_promise_void(promise)?;
            } else {
                drain_jobs(&self.runtime, &self.interrupt)?;
            }
        }

        Ok(())
    }

    fn settle_promise_void(
        &self,
        promise: Persistent<Promise<'static>>,
    ) -> Result<(), JsExtensionError> {
        let drain_res = drain_jobs(&self.runtime, &self.interrupt);
        if let Err(err) = drain_res {
            drop(promise);
            return Err(err);
        }
        self.context.with(|ctx| {
            let promise = promise
                .restore(&ctx)
                .map_err(|err| JsExtensionError::Script(err.to_string()))?;
            match promise.state() {
                PromiseState::Resolved => {
                    let _ = promise.result::<Value<'_>>();
                    Ok(())
                }
                PromiseState::Rejected => {
                    let err = promise
                        .result::<Value<'_>>()
                        .expect("rejected promise has result")
                        .unwrap_err();
                    Err(classify_js_error(&ctx, err, &self.interrupt))
                }
                PromiseState::Pending => Err(JsExtensionError::Script(
                    "extension async initialization promise remained unsettled".to_string(),
                )),
            }
        })
    }

    fn settle_promise_opt_string(
        &self,
        promise: Persistent<Promise<'static>>,
    ) -> Result<Option<String>, JsExtensionError> {
        let drain_res = drain_jobs(&self.runtime, &self.interrupt);
        if let Err(err) = drain_res {
            drop(promise);
            return Err(err);
        }
        self.context.with(|ctx| {
            let promise = promise
                .restore(&ctx)
                .map_err(|err| JsExtensionError::Script(err.to_string()))?;
            match promise.state() {
                PromiseState::Resolved => {
                    let val: Value<'_> = promise
                        .result()
                        .expect("resolved promise has result")
                        .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                    extract_opt_string(&ctx, val, &self.interrupt)
                }
                PromiseState::Rejected => {
                    let err = promise
                        .result::<Value<'_>>()
                        .expect("rejected promise has result")
                        .unwrap_err();
                    Err(classify_js_error(&ctx, err, &self.interrupt))
                }
                PromiseState::Pending => Err(JsExtensionError::Script(
                    "JS async handler returned a promise that remained unsettled".to_string(),
                )),
            }
        })
    }

    fn recover_after_error(&self) {
        self.interrupt.disarm();
        // Clear any leftover exception on the context and reclaim memory after timeout/OOM.
        self.context.with(|ctx| {
            let _ = ctx.catch();
        });
        self.runtime.run_gc();
    }

    fn invoke_tool(
        &self,
        name: &str,
        tool_call_id: &str,
        args_json: &str,
        cancel: CancelToken,
    ) -> Result<String, JsExtensionError> {
        if cancel.is_cancelled() {
            return Err(JsExtensionError::Cancelled);
        }
        self.interrupt.arm(self.timeout_ms, Some(cancel));
        let _guard = InterruptGuard(&self.interrupt);

        let step = self
            .context
            .with(|ctx| -> Result<CallOutcome, JsExtensionError> {
                let rt_obj = self
                    .runtime_obj
                    .clone()
                    .restore(&ctx)
                    .map_err(|err| JsExtensionError::Script(err.to_string()))?;
                let func: Function<'_> = rt_obj
                    .get("invokeTool")
                    .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                let ret: Value<'_> = func
                    .call((name, tool_call_id, args_json))
                    .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                inspect_call_value(&ctx, ret, &self.interrupt)
            });

        let res = match step {
            Ok(CallOutcome::Ready(opt)) => Ok(opt),
            Ok(CallOutcome::Pending(promise)) => self.settle_promise_opt_string(promise),
            Err(err) => Err(err),
        };

        if res.is_err() {
            self.recover_after_error();
        }
        res.and_then(|opt| {
            opt.ok_or_else(|| {
                JsExtensionError::Script("tool returned empty payload unexpectedly".to_string())
            })
        })
    }

    fn invoke_hook(
        &self,
        hook: HookKind,
        payload_json: &str,
        cancel: CancelToken,
    ) -> Result<Option<String>, JsExtensionError> {
        if cancel.is_cancelled() {
            return Err(JsExtensionError::Cancelled);
        }
        self.interrupt.arm(self.timeout_ms, Some(cancel));
        let _guard = InterruptGuard(&self.interrupt);

        let step = self
            .context
            .with(|ctx| -> Result<CallOutcome, JsExtensionError> {
                let rt_obj = self
                    .runtime_obj
                    .clone()
                    .restore(&ctx)
                    .map_err(|err| JsExtensionError::Script(err.to_string()))?;
                let func: Function<'_> = rt_obj
                    .get("invokeHook")
                    .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                let ret: Value<'_> = func
                    .call((hook.as_str(), payload_json))
                    .map_err(|err| classify_js_error(&ctx, err, &self.interrupt))?;
                inspect_call_value(&ctx, ret, &self.interrupt)
            });

        let res = match step {
            Ok(CallOutcome::Ready(opt)) => Ok(opt),
            Ok(CallOutcome::Pending(promise)) => self.settle_promise_opt_string(promise),
            Err(err) => Err(err),
        };

        if res.is_err() {
            self.recover_after_error();
        }
        res
    }
}

enum CallOutcome {
    Ready(Option<String>),
    Pending(Persistent<Promise<'static>>),
}

fn inspect_call_value<'js>(
    ctx: &Ctx<'js>,
    ret: Value<'js>,
    interrupt: &InterruptState,
) -> Result<CallOutcome, JsExtensionError> {
    if let Some(promise) = ret.clone().into_promise() {
        match promise.state() {
            PromiseState::Pending => Ok(CallOutcome::Pending(Persistent::save(ctx, promise))),
            PromiseState::Resolved => {
                let val: Value<'_> = promise
                    .result()
                    .expect("resolved promise has result")
                    .map_err(|err| classify_js_error(ctx, err, interrupt))?;
                Ok(CallOutcome::Ready(extract_opt_string(ctx, val, interrupt)?))
            }
            PromiseState::Rejected => {
                let err = promise
                    .result::<Value<'_>>()
                    .expect("rejected promise has result")
                    .unwrap_err();
                Err(classify_js_error(ctx, err, interrupt))
            }
        }
    } else {
        Ok(CallOutcome::Ready(extract_opt_string(ctx, ret, interrupt)?))
    }
}

fn extract_opt_string<'js>(
    ctx: &Ctx<'js>,
    val: Value<'js>,
    interrupt: &InterruptState,
) -> Result<Option<String>, JsExtensionError> {
    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }
    let s = val
        .as_string()
        .ok_or_else(|| {
            JsExtensionError::Serialization("expected JSON string or undefined from JS".to_string())
        })?
        .to_string()
        .map_err(|err| classify_js_error(ctx, err, interrupt))?;
    Ok(Some(s))
}

fn extract_default_fn_from_ns<'js>(
    ctx: &Ctx<'js>,
    ns: &Object<'js>,
    interrupt: &InterruptState,
) -> Result<Option<Function<'js>>, JsExtensionError> {
    let default_val: Value<'_> = ns
        .get("default")
        .map_err(|err| classify_js_error(ctx, err, interrupt))?;
    if default_val.is_undefined() || default_val.is_null() {
        return Ok(None);
    }
    Ok(default_val.into_function())
}

fn looks_like_esm(code: &str) -> bool {
    code.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("export ")
            || trimmed.starts_with("export{")
            || trimmed.starts_with("export\t")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("import{")
            || trimmed.starts_with("import\"")
            || trimmed.starts_with("import'")
    })
}

fn drain_jobs(runtime: &Runtime, interrupt: &InterruptState) -> Result<(), JsExtensionError> {
    let mut executed = 0usize;
    while runtime.is_job_pending() {
        if let Some(reason) = interrupt.check() {
            return Err(reason);
        }
        match runtime.execute_pending_job() {
            Ok(true) => {
                executed += 1;
                if executed >= JOBS_PER_POLL {
                    executed = 0;
                    if let Some(reason) = interrupt.check() {
                        return Err(reason);
                    }
                }
            }
            Ok(false) => break,
            Err(job_err) => {
                if let Some(reason) = interrupt.check() {
                    return Err(reason);
                }
                let err = job_err
                    .0
                    .with(|ctx| classify_caught_value(&ctx, ctx.catch(), interrupt));
                return Err(err);
            }
        }
    }
    Ok(())
}

fn is_oom_error(name: Option<&str>, message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (name == Some("InternalError") && lower.contains("out of memory")) || lower == "out of memory"
}

fn classify_js_error(ctx: &Ctx<'_>, err: JsError, interrupt: &InterruptState) -> JsExtensionError {
    if let Some(reason) = interrupt.check() {
        return reason;
    }
    match err {
        JsError::Allocation => JsExtensionError::OutOfMemory("out of memory".to_string()),
        JsError::Exception => classify_caught_value(ctx, ctx.catch(), interrupt),
        other => JsExtensionError::Script(other.to_string()),
    }
}

fn classify_caught_value(
    _ctx: &Ctx<'_>,
    caught: Value<'_>,
    interrupt: &InterruptState,
) -> JsExtensionError {
    if let Some(reason) = interrupt.check() {
        return reason;
    }

    if let Some(obj) = caught.as_object()
        && let Some(exc) = Exception::from_object(obj.clone())
    {
        let name: Option<String> = obj.get("name").ok();
        let message = exc.message().unwrap_or_default();
        if is_oom_error(name.as_deref(), &message) {
            return JsExtensionError::OutOfMemory(message);
        }
        if name.as_deref() == Some("InternalError") && message.eq_ignore_ascii_case("interrupted") {
            return interrupt.check().unwrap_or(JsExtensionError::Cancelled);
        }
        let formatted = match name {
            Some(n) if !message.is_empty() => format!("{n}: {message}"),
            Some(n) => n,
            None => message,
        };
        return JsExtensionError::Script(formatted);
    }

    let message = caught
        .as_string()
        .and_then(|s| s.to_string().ok())
        .unwrap_or_else(|| format!("{caught:?}"));
    if is_oom_error(None, &message) {
        return JsExtensionError::OutOfMemory(message);
    }
    JsExtensionError::Script(message)
}

fn run_worker_loop(
    limits: VmLimits,
    initial_sources: Vec<ResolvedSource>,
    init_tx: mpsc::Sender<Result<RegistrySnapshot, JsExtensionError>>,
    rx: mpsc::Receiver<WorkerCmd>,
) {
    let mut generation: u64 = 1;
    let mut active_vm = match VmInstance::build(&limits, &initial_sources) {
        Ok((vm, mut snapshot)) => {
            snapshot.generation = generation;
            let _ = init_tx.send(Ok(snapshot));
            vm
        }
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            WorkerCmd::Reload { sources, reply } => {
                // Build the new VM completely BEFORE dropping the active VM so failed reloads
                // leave the previous active VM intact (atomic swap).
                match VmInstance::build(&limits, &sources) {
                    Ok((new_vm, mut snapshot)) => {
                        generation = generation.saturating_add(1);
                        snapshot.generation = generation;
                        active_vm = new_vm;
                        let _ = reply.send(Ok(snapshot));
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err));
                    }
                }
            }
            WorkerCmd::InvokeTool {
                name,
                tool_call_id,
                args_json,
                cancel,
                reply,
            } => {
                let res = active_vm.invoke_tool(&name, &tool_call_id, &args_json, cancel);
                let _ = reply.send(res);
            }
            WorkerCmd::InvokeHook {
                hook,
                payload_json,
                cancel,
                reply,
            } => {
                let res = active_vm.invoke_hook(hook, &payload_json, cancel);
                let _ = reply.send(res);
            }
            WorkerCmd::Shutdown => break,
        }
    }

    drop(active_vm);
}
