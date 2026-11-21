use crate::session::{PikraEventFn, PikraResultDoneFn, PikraVoidDoneFn, SendCallbackCtx};
use crate::transport::{FfiHttpTransport, PikraHttpCallbacks, SendHttpCallbacks};
use crate::{PikraStatus, read_cstr, write_out_string};
use futures_channel::oneshot;
use futures_util::FutureExt;
use pikra_agent::{AgentTool, AgentToolResult, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, HttpTransport};
#[cfg(not(target_os = "ios"))]
use pikra_tools::BashTool;
use pikra_tools::{
    ApplyPatchTool, EditTool, FindTool, GrepTool, ReadTool, SleepTool, ToolDefinitionOverride,
    WakeGate, WebFetchConfig, WebFetchTool, WorkspaceToolsConfig, WriteTool,
    wrap_tool_with_override,
};
#[cfg(all(unix, not(target_os = "ios")))]
use pikra_tools::{
    BackgroundEventCallback, ExecCommandTool, MonitorManager, MonitorTool, ProcessSessionManager,
    WriteStdinTool,
};
use serde::Deserialize;
use serde_json::Value;
#[cfg(all(unix, not(target_os = "ios")))]
use std::any::Any;
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{Notify, watch};

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pikra-builtins")
            .enable_all()
            .build()
            .expect("could not start workspace tool runtime")
    })
}

pub type PikraBuiltinEnvProviderFn =
    extern "C" fn(ctx: *mut c_void, completion: *mut PikraBuiltinEnvCompletion);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PikraBuiltinCallbacks {
    pub ctx: *mut c_void,
    pub environment: Option<PikraBuiltinEnvProviderFn>,
    pub background_event: Option<PikraEventFn>,
    pub destroy: Option<PikraVoidDoneFn>,
}

struct HostCallbacks {
    ctx: SendCallbackCtx,
    environment: Option<PikraBuiltinEnvProviderFn>,
    #[cfg(all(unix, not(target_os = "ios")))]
    background_event: Option<PikraEventFn>,
    destroy: Option<PikraVoidDoneFn>,
}

impl Drop for HostCallbacks {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy {
            destroy(self.ctx.as_ptr());
        }
    }
}

pub struct PikraBuiltinEnvCompletion(oneshot::Sender<Vec<(String, String)>>);

#[derive(Deserialize)]
struct EnvConfig {
    cwd: PathBuf,
    max_output_bytes: Option<usize>,
    default_bash_timeout_ms: Option<u64>,
    #[serde(default)]
    workspace_compatibility: bool,
    #[cfg(not(target_os = "ios"))]
    shell_max_output_bytes: Option<usize>,
}

type Cleanup = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;
// Erasing the manager type keeps its destructor out of binaries that omit its factories.
#[cfg(all(unix, not(target_os = "ios")))]
type ManagerSlot = OnceLock<Box<dyn Any + Send + Sync>>;

#[derive(Default)]
struct RunState {
    closed: bool,
    next_id: u64,
    active: HashMap<u64, CancelToken>,
    cleanup: Vec<Cleanup>,
}

struct BuiltinEnv {
    config: Arc<WorkspaceToolsConfig>,
    workspace_compatibility: bool,
    #[cfg(not(target_os = "ios"))]
    shell_max_output_bytes: Option<usize>,
    transport: Option<Arc<dyn HttpTransport>>,
    wake_gate: WakeGate,
    #[cfg(all(unix, not(target_os = "ios")))]
    background_event: Option<BackgroundEventCallback>,
    #[cfg(all(unix, not(target_os = "ios")))]
    process_manager: ManagerSlot,
    #[cfg(all(unix, not(target_os = "ios")))]
    monitor_manager: ManagerSlot,
    state: Mutex<RunState>,
    idle: Notify,
    shutdown_done: watch::Sender<bool>,
}

impl BuiltinEnv {
    fn register(self: &Arc<Self>, cancel: CancelToken) -> Option<ActiveRun> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return None;
        }
        let id = state.next_id;
        state.next_id += 1;
        state.active.insert(id, cancel);
        Some(ActiveRun {
            env: self.clone(),
            id,
        })
    }

    fn begin_shutdown(self: &Arc<Self>) {
        let (active, cleanup) = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return;
            }
            state.closed = true;
            let active = state.active.values().cloned().collect::<Vec<_>>();
            (active, std::mem::take(&mut state.cleanup))
        };
        for cancel in active {
            cancel.cancel();
        }
        let env = self.clone();
        runtime().spawn(async move {
            futures_util::future::join_all(cleanup.into_iter().map(|close| close())).await;
            while !env.state.lock().unwrap().active.is_empty() {
                env.idle.notified().await;
            }
            env.shutdown_done.send_replace(true);
        });
    }

    #[cfg(all(unix, not(target_os = "ios")))]
    fn manager<T: Clone + Send + Sync + 'static>(
        &self,
        slot: &ManagerSlot,
        create: impl FnOnce(Arc<WorkspaceToolsConfig>, WakeGate, Option<BackgroundEventCallback>) -> T,
        shutdown: impl FnOnce(T) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> Result<T, String> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err("Workspace tools are closed".into());
        }
        let resource = slot.get_or_init(|| {
            let manager = create(
                self.config.clone(),
                self.wake_gate.clone(),
                self.background_event.clone(),
            );
            let owned = manager.clone();
            state.cleanup.push(Box::new(move || shutdown(owned)));
            Box::new(manager)
        });
        Ok(resource
            .downcast_ref::<T>()
            .expect("manager slot type")
            .clone())
    }

    #[cfg(all(unix, not(target_os = "ios")))]
    fn process_manager(&self) -> Result<ProcessSessionManager, String> {
        self.manager(
            &self.process_manager,
            ProcessSessionManager::new,
            |manager| Box::pin(async move { manager.shutdown().await }),
        )
    }

    #[cfg(all(unix, not(target_os = "ios")))]
    fn monitor_manager(&self) -> Result<MonitorManager, String> {
        self.manager(&self.monitor_manager, MonitorManager::new, |manager| {
            Box::pin(async move { manager.shutdown().await })
        })
    }
}

struct ActiveRun {
    env: Arc<BuiltinEnv>,
    id: u64,
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        self.env.state.lock().unwrap().active.remove(&self.id);
        self.env.idle.notify_one();
    }
}

pub struct PikraBuiltinEnv(Arc<BuiltinEnv>);

pub struct PikraBuiltinTool {
    env: Arc<BuiltinEnv>,
    tool: Arc<dyn AgentTool>,
    blocking: bool,
}

pub struct PikraBuiltinRun(CancelToken);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_env_create(
    config_json: *const c_char,
    callbacks: *const PikraBuiltinCallbacks,
    http_callbacks: *const PikraHttpCallbacks,
    out_error: *mut *mut c_char,
) -> *mut PikraBuiltinEnv {
    let create = || -> Result<_, String> {
        let config = unsafe { read_cstr(config_json, "config_json") }?;
        let config: EnvConfig = serde_json::from_str(config).map_err(|e| e.to_string())?;
        if config.cwd.as_os_str().is_empty() {
            return Err("Specify a workspace directory".into());
        }
        let http = unsafe { http_callbacks.as_ref() }
            .map(|callbacks| SendHttpCallbacks::new(*callbacks))
            .transpose()?;
        let host = unsafe { callbacks.as_ref() }.map(|callbacks| {
            Arc::new(HostCallbacks {
                ctx: SendCallbackCtx(callbacks.ctx),
                environment: callbacks.environment,
                #[cfg(all(unix, not(target_os = "ios")))]
                background_event: callbacks.background_event,
                destroy: callbacks.destroy,
            })
        });
        let mut workspace = WorkspaceToolsConfig::new(config.cwd);
        if let Some(max) = config.max_output_bytes {
            workspace.max_output_bytes = max;
        }
        workspace.default_bash_timeout_ms = config.default_bash_timeout_ms;
        if config.workspace_compatibility {
            workspace.ignore_dirs.push(".build".into());
        }
        if let Some(host) = &host
            && let Some(provide) = host.environment
        {
            let host = host.clone();
            workspace.env_provider = Some(Arc::new(move || {
                let (tx, rx) = oneshot::channel();
                provide(
                    host.ctx.as_ptr(),
                    Box::into_raw(Box::new(PikraBuiltinEnvCompletion(tx))),
                );
                Box::pin(async move { rx.await.unwrap_or_default() })
            }));
        }
        #[cfg(all(unix, not(target_os = "ios")))]
        let background_event = host.and_then(|host| {
            let send = host.background_event?;
            let callback: BackgroundEventCallback = Arc::new(move |event| {
                if let Ok(json) = serde_json::to_string(&event)
                    && let Ok(json) = CString::new(json)
                {
                    send(host.ctx.as_ptr(), json.as_ptr());
                }
            });
            Some(callback)
        });
        let transport = http.map(|http| {
            http.take_ownership();
            Arc::new(FfiHttpTransport::new(http)) as Arc<dyn HttpTransport>
        });
        Ok(PikraBuiltinEnv(Arc::new(BuiltinEnv {
            config: Arc::new(workspace),
            workspace_compatibility: config.workspace_compatibility,
            #[cfg(not(target_os = "ios"))]
            shell_max_output_bytes: config.shell_max_output_bytes,
            transport,
            wake_gate: WakeGate::new(),
            #[cfg(all(unix, not(target_os = "ios")))]
            background_event,
            #[cfg(all(unix, not(target_os = "ios")))]
            process_manager: OnceLock::new(),
            #[cfg(all(unix, not(target_os = "ios")))]
            monitor_manager: OnceLock::new(),
            state: Mutex::default(),
            idle: Notify::new(),
            shutdown_done: watch::channel(false).0,
        })))
    };
    into_handle(create(), out_error)
}

fn into_handle<T>(result: Result<T, String>, out_error: *mut *mut c_char) -> *mut T {
    match result {
        Ok(value) => Box::into_raw(Box::new(value)),
        Err(error) => {
            unsafe { write_out_string(out_error, error) };
            null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_env_complete(
    completion: *mut PikraBuiltinEnvCompletion,
    environment_json: *const c_char,
) {
    if !completion.is_null() {
        let completion = unsafe { Box::from_raw(completion) };
        let environment: HashMap<String, String> =
            unsafe { read_cstr(environment_json, "environment_json") }
                .ok()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or_default();
        let _ = completion.0.send(environment.into_iter().collect());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_env_shutdown(
    env: *mut PikraBuiltinEnv,
    ctx: *mut c_void,
    on_done: Option<PikraVoidDoneFn>,
) {
    let Some(env) = (unsafe { env.as_ref() }) else {
        if let Some(done) = on_done {
            done(ctx);
        }
        return;
    };
    env.0.begin_shutdown();
    if let Some(done) = on_done {
        let ctx = SendCallbackCtx(ctx);
        let mut finished = env.0.shutdown_done.subscribe();
        runtime().spawn(async move {
            let _ = finished.wait_for(|done| *done).await;
            done(ctx.as_ptr());
        });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_env_destroy(env: *mut PikraBuiltinEnv) {
    if !env.is_null() {
        let env = unsafe { Box::from_raw(env) };
        env.0.begin_shutdown();
    }
}

fn factory<T: AgentTool + 'static>(
    env: *mut PikraBuiltinEnv,
    out_error: *mut *mut c_char,
    blocking: bool,
    make: impl FnOnce(&BuiltinEnv) -> Result<T, String>,
) -> *mut PikraBuiltinTool {
    let result = (|| {
        let env = unsafe { env.as_ref() }.ok_or("Specify a workspace environment")?;
        if env.0.state.lock().unwrap().closed {
            return Err("Workspace tools are closed".into());
        }
        Ok(PikraBuiltinTool {
            env: env.0.clone(),
            tool: Arc::new(make(&env.0)?),
            blocking,
        })
    })();
    into_handle(result, out_error)
}

macro_rules! builtin_factory {
    ($(#[$attr:meta])* $name:ident, $blocking:literal, $make:expr) => {
        $(#[$attr])*
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            env: *mut PikraBuiltinEnv,
            out_error: *mut *mut c_char,
        ) -> *mut PikraBuiltinTool {
            factory(env, out_error, $blocking, $make)
        }
    };
}

builtin_factory!(pikra_builtin_tool_read, true, |env| {
    let tool = ReadTool::new(env.config.clone());
    Ok(if env.workspace_compatibility {
        tool.with_plain_text()
    } else {
        tool
    })
});
builtin_factory!(pikra_builtin_tool_write, true, |env| Ok(WriteTool::new(
    env.config.clone()
)));
builtin_factory!(pikra_builtin_tool_edit, true, |env| Ok(EditTool::new(
    env.config.clone()
)));
builtin_factory!(pikra_builtin_tool_apply_patch, true, |env| Ok(
    ApplyPatchTool::new(env.config.clone())
));
builtin_factory!(pikra_builtin_tool_find, true, |env| Ok(FindTool::new(
    env.config.clone()
)));
builtin_factory!(pikra_builtin_tool_grep, true, |env| Ok(GrepTool::new(
    env.config.clone()
)));
builtin_factory!(pikra_builtin_tool_sleep, false, |env| Ok(SleepTool::new(
    env.wake_gate.clone()
)));
builtin_factory!(pikra_builtin_tool_web_fetch, false, |env| {
    let transport = env
        .transport
        .clone()
        .ok_or("Provide an HTTP client to use web_fetch")?;
    Ok(WebFetchTool::new(
        transport,
        WebFetchConfig {
            max_output_bytes: env.config.max_output_bytes,
            ..WebFetchConfig::default()
        },
    ))
});
builtin_factory!(
    #[cfg(not(target_os = "ios"))]
    pikra_builtin_tool_bash,
    false,
    |env| {
        let config = match env.shell_max_output_bytes {
            Some(max) => Arc::new((*env.config).clone().with_max_output_bytes(max)),
            None => env.config.clone(),
        };
        Ok(BashTool::new(config))
    }
);
builtin_factory!(
    #[cfg(all(unix, not(target_os = "ios")))]
    pikra_builtin_tool_exec_command,
    false,
    |env| Ok(ExecCommandTool::new(env.process_manager()?))
);
builtin_factory!(
    #[cfg(all(unix, not(target_os = "ios")))]
    pikra_builtin_tool_write_stdin,
    false,
    |env| Ok(WriteStdinTool::new(env.process_manager()?))
);
builtin_factory!(
    #[cfg(all(unix, not(target_os = "ios")))]
    pikra_builtin_tool_monitor,
    false,
    |env| Ok(MonitorTool::new(env.monitor_manager()?))
);

#[cfg(any(target_os = "ios", not(unix)))]
macro_rules! unavailable_subprocess {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            _env: *mut PikraBuiltinEnv,
            out_error: *mut *mut c_char,
        ) -> *mut PikraBuiltinTool {
            unsafe { write_out_string(out_error, if cfg!(target_os = "ios") {
                "subprocess tools are unavailable on iOS"
            } else {
                "subprocess tools are unavailable on this platform"
            }) };
            null_mut()
        }
    };
}

#[cfg(target_os = "ios")]
unavailable_subprocess!(pikra_builtin_tool_bash);
#[cfg(any(target_os = "ios", not(unix)))]
unavailable_subprocess!(pikra_builtin_tool_exec_command);
#[cfg(any(target_os = "ios", not(unix)))]
unavailable_subprocess!(pikra_builtin_tool_write_stdin);
#[cfg(any(target_os = "ios", not(unix)))]
unavailable_subprocess!(pikra_builtin_tool_monitor);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_tool_override(
    tool: *const PikraBuiltinTool,
    override_json: *const c_char,
    out_error: *mut *mut c_char,
) -> *mut PikraBuiltinTool {
    let result = (|| {
        let tool = unsafe { tool.as_ref() }.ok_or("Specify a workspace tool")?;
        let spec = unsafe { read_cstr(override_json, "override_json") }?;
        let spec: ToolDefinitionOverride = serde_json::from_str(spec).map_err(|e| e.to_string())?;
        Ok(PikraBuiltinTool {
            env: tool.env.clone(),
            tool: wrap_tool_with_override(tool.tool.clone(), spec)?,
            blocking: tool.blocking,
        })
    })();
    into_handle(result, out_error)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_tool_definition_json(
    tool: *const PikraBuiltinTool,
) -> *mut c_char {
    unsafe { tool.as_ref() }
        .and_then(|tool| CString::new(serde_json::to_string(&tool.tool.definition()).ok()?).ok())
        .map_or(null_mut(), CString::into_raw)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_tool_is_sequential(tool: *const PikraBuiltinTool) -> bool {
    unsafe { tool.as_ref() }.is_some_and(|tool| {
        tool.tool.execution_mode() == pikra_agent::ToolExecutionMode::Sequential
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_tool_execute(
    tool: *const PikraBuiltinTool,
    call_id: *const c_char,
    arguments_json: *const c_char,
    ctx: *mut c_void,
    progress: Option<PikraEventFn>,
    done: Option<PikraResultDoneFn>,
    out_error: *mut *mut c_char,
) -> *mut PikraBuiltinRun {
    let start = || {
        let tool = unsafe { tool.as_ref() }.ok_or("Specify a workspace tool")?;
        let done = done.ok_or("Specify a tool completion callback")?;
        let call_id = unsafe { read_cstr(call_id, "call_id") }?.to_owned();
        let args = unsafe { read_cstr(arguments_json, "arguments_json") }?;
        let args: Value = serde_json::from_str(args).map_err(|e| e.to_string())?;
        let cancel = CancelToken::new();
        let active = tool
            .env
            .register(cancel.clone())
            .ok_or("Workspace tools are closed")?;
        let inner = tool.tool.clone();
        let run_cancel = cancel.clone();
        let ctx = SendCallbackCtx(ctx);
        let update: ToolProgressCallback = Arc::new(move |result| {
            if let Some(progress) = progress
                && let Some(json) = serialize_result(&result)
            {
                progress(ctx.as_ptr(), json.as_ptr());
            }
        });
        let execute = async move {
            let execution = async {
                if run_cancel.is_cancelled() {
                    return Err("Operation aborted".into());
                }
                inner
                    .execute(call_id, args, run_cancel.clone(), update)
                    .await
            };
            let result = AssertUnwindSafe(execution).catch_unwind().await;
            let (status, json) = if run_cancel.is_cancelled() {
                (PikraStatus::ErrCancelled, None)
            } else {
                match result {
                    Ok(result) => (
                        PikraStatus::Ok,
                        serialize_result(&result.unwrap_or_else(AgentToolResult::error)),
                    ),
                    Err(_) => (PikraStatus::ErrInternal, None),
                }
            };
            let status = if status == PikraStatus::Ok && json.is_none() {
                PikraStatus::ErrInternal
            } else {
                status
            };
            done(
                ctx.as_ptr(),
                status,
                json.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                std::ptr::null(),
            );
            drop(active);
        };
        if tool.blocking {
            runtime().spawn_blocking(|| runtime().handle().block_on(execute));
        } else {
            runtime().spawn(execute);
        }
        Ok(PikraBuiltinRun(cancel))
    };
    into_handle(start(), out_error)
}

fn serialize_result(result: &AgentToolResult) -> Option<CString> {
    CString::new(serde_json::to_string(result).ok()?).ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_tool_cancel(run: *const PikraBuiltinRun) {
    if let Some(run) = unsafe { run.as_ref() } {
        run.0.cancel();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_run_destroy(run: *mut PikraBuiltinRun) {
    if !run.is_null() {
        drop(unsafe { Box::from_raw(run) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_builtin_tool_destroy(tool: *mut PikraBuiltinTool) {
    if !tool.is_null() {
        drop(unsafe { Box::from_raw(tool) });
    }
}
