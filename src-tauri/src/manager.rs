//! A process manager for ActivityWatch
//!
//! Used to start, stop and manage the lifecycle modules like aw-watcher-afk and aw-watcher-window.
//! A module is a process that runs in the background and sends events to the ActivityWatch server.
//!
//! The manager is responsible for starting and stopping the modules, and for keeping track of
//! their state.
//!
//! If a module crashes, the manager will notify the user and ask if they want to restart it.

#[cfg(unix)]
use {
    nix::sys::signal::{self, Signal},
    nix::unistd::Pid,
    std::os::unix::fs::PermissionsExt,
};
#[cfg(windows)]
use {
    std::os::windows::process::CommandExt,
    std::ptr::null_mut,
    winapi::shared::minwindef::{DWORD, FALSE},
    winapi::um::handleapi::CloseHandle,
    winapi::um::jobapi2::{AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject},
    winapi::um::processthreadsapi::{OpenProcess, TerminateProcess},
    winapi::um::winbase::CREATE_NO_WINDOW,
    winapi::um::winnt::{
        JobObjectExtendedLimitInformation, HANDLE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, PROCESS_TERMINATE,
    },
};

use log::{debug, error, info, trace, warn};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    mpsc::{channel, Receiver, Sender},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use std::{env, fs, thread};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, SubmenuBuilder};
use tauri::{AppHandle, Wry};

use crate::{get_app_handle, get_config, get_tray_id, module_alert_ui, HANDLE_CONDVAR};
use std::io::{BufRead, BufReader, Read};
use tauri_plugin_notification::NotificationExt;

/// How long a module must run before a crash no longer counts toward the restart limit.
const STABLE_UPTIME: Duration = Duration::from_secs(60);

#[derive(Debug)]
enum ModuleMessage {
    Started {
        name: String,
        pid: u32,
        args: Option<Vec<String>>,
    },
    Stopped {
        name: String,
        output: std::process::Output,
    },
    Init {},
    Notification {
        title: String,
        message: String,
    },
}

/// A lightweight, path-free view of every module's run state, keyed by module name.
/// `None` = discovered but never started, `Some(true)` = running, `Some(false)` = stopped
/// after having been started. This is all the tray and mini-mode event loop need; it is cheap
/// to build and shared via `Arc` instead of cloning the full per-module state on every event.
pub(crate) type ModulesSnapshot = BTreeMap<String, Option<bool>>;

#[derive(Debug, Clone)]
pub(crate) enum ManagerEvent {
    ModulesChanged { modules: Arc<ModulesSnapshot> },
    Notification { title: String, message: String },
}

/// Per-module lifecycle state. Replaces the parallel maps that were previously keyed by module
/// name (running/discovered/pid/restart_count/pending_shutdown/args), keeping a single source of
/// truth per module.
#[derive(Debug)]
struct Module {
    /// Path to the module binary (from `discover_modules`); fixed after startup.
    path: PathBuf,
    /// `None` = discovered but never started, `Some(true)` = running, `Some(false)` = started at
    /// least once and currently stopped. Drives the tray grouping (started modules first).
    run_state: Option<bool>,
    pid: Option<u32>,
    /// When the current run started, used to tell a crash loop from a crash after a long run.
    started_at: Option<Instant>,
    /// Automatic restarts since the module last ran stably or was started by hand.
    restart_count: u32,
    /// Incremented on every start request (not on the async `Started` message, which can lag),
    /// so a delayed restart can tell whether the module has been started, and possibly stopped
    /// again, since the crash it was scheduled for.
    generation: u64,
    pending_shutdown: bool,
    /// Last-known or configured args, reused on manual or post-crash restart (#131).
    args: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct ManagerState {
    tx: Sender<ModuleMessage>,
    pub server_port: u16,
    modules: BTreeMap<String, Module>,
}

impl ManagerState {
    fn new(tx: Sender<ModuleMessage>, server_port: u16) -> ManagerState {
        // Seed one entry per discovered module, attaching any configured args. Modules that are
        // configured but not discovered are dropped: `start_module` only starts discovered ones.
        let mut configured_args = configured_modules_args();
        let modules = discover_modules()
            .into_iter()
            .map(|(name, path)| {
                let args = configured_args.remove(&name).flatten();
                let module = Module {
                    path,
                    run_state: None,
                    pid: None,
                    started_at: None,
                    restart_count: 0,
                    generation: 0,
                    pending_shutdown: false,
                    args,
                };
                (name, module)
            })
            .collect();

        ManagerState {
            tx,
            server_port,
            modules,
        }
    }

    /// A path-free `name -> run state` view for the tray and mini-mode event loop.
    pub(crate) fn modules_snapshot(&self) -> ModulesSnapshot {
        self.modules
            .iter()
            .map(|(name, module)| (name.clone(), module.run_state))
            .collect()
    }

    fn started_module(&mut self, name: &str, pid: u32, args: Option<Vec<String>>) {
        info!("Started module: {name}");
        if let Some(module) = self.modules.get_mut(name) {
            module.run_state = Some(true);
            module.pid = Some(pid);
            module.started_at = Some(Instant::now());
            module.args = args;
            module.pending_shutdown = false;
        } else {
            warn!("Started unknown module {name}, not in discovered set");
        }
        debug!("Modules: {:?}", self.modules);
    }
    fn stopped_module(&mut self, name: &str) {
        info!("Stopped module: {name}");
        if let Some(module) = self.modules.get_mut(name) {
            module.run_state = Some(false);
            module.pid = None;
            // A module that ran for a while before crashing isn't crash-looping, so give it the
            // full set of retries again instead of carrying old crashes forward forever.
            if module
                .started_at
                .take()
                .is_some_and(|t| t.elapsed() >= STABLE_UPTIME)
            {
                module.restart_count = 0;
            }
        }
    }

    pub fn start_module(&mut self, name: &str, args: Option<&Vec<String>>) {
        if !self.is_module_running(name) {
            if let Some(module) = self.modules.get_mut(name) {
                module.generation += 1;
                // Fall back to the last known (or configured) args for this module, so manual
                // tray restarts and post-crash restarts don't silently drop them (#131).
                let effective_args = args.cloned().or_else(|| module.args.clone());
                start_module_thread(
                    name.to_string(),
                    module.path.clone(),
                    effective_args,
                    self.server_port,
                    self.tx.clone(),
                );
            } else {
                error!("Module {name} not found in PATH");
            }
        }
    }
    pub fn stop_module(&mut self, name: &str) {
        if let Some(module) = self.modules.get_mut(name) {
            if let Some(pid) = module.pid {
                // mark pending shutdown to prevent restart
                module.pending_shutdown = true;
                if let Err(e) = send_sigterm(pid) {
                    error!("Failed to send SIGTERM to module {name}: {e}");
                } else {
                    debug!("Sent SIGTERM to module: {name}");
                }
            }
        }
    }
    pub fn stop_modules(&mut self) {
        let running: Vec<String> = self
            .modules
            .iter()
            .filter(|(_, module)| module.pid.is_some())
            .map(|(name, _)| name.clone())
            .collect();
        for name in running {
            self.stop_module(&name);
        }
    }
    pub fn handle_system_click(&mut self, name: &str) {
        if self.is_module_running(name) {
            self.stop_module(name);
        } else {
            // A manual start is a fresh start: re-arm automatic restarts (even after the limit
            // was hit) and don't report it as a crash recovery.
            if let Some(module) = self.modules.get_mut(name) {
                module.restart_count = 0;
            }
            self.start_module(name, None);
        }
    }
    fn is_module_running(&self, name: &str) -> bool {
        self.modules
            .get(name)
            .is_some_and(|module| module.run_state == Some(true))
    }
}

struct TrayMenuCache {
    // Per-module check items for the running group, keyed by module name, so later events can
    // sync checked-state in place instead of rebuilding the whole tray menu, which previously
    // happened on every Started/Stopped message.
    module_items: HashMap<String, CheckMenuItem<Wry>>,
    // Names of modules in the running group (those started at least once) when the menu was last
    // built. The menu only needs rebuilding when this set changes (a module starts for the first
    // time and must move into the top group); crash-restart loops keep the module in the set and
    // only toggle its checked state, so they take the cheap sync path.
    running_keys: BTreeSet<String>,
}

static TRAY_MENU_CACHE: Mutex<Option<TrayMenuCache>> = Mutex::new(None);

fn update_tray_menu(modules: ModulesSnapshot, event_tx: &Option<Sender<ManagerEvent>>) {
    // In mini mode, forward state to the mini event loop instead of Tauri
    if let Some(tx) = event_tx {
        let _ = tx.send(ManagerEvent::ModulesChanged {
            modules: Arc::new(modules),
        });
        return;
    }
    let modules = &modules;
    if crate::is_daemon_mode() {
        return;
    }
    {
        // Wait until setup has stored the app handle; release the lock before building the menu.
        let (lock, cvar) = &*HANDLE_CONDVAR;
        let mut initialized = lock.lock().expect("Failed to lock HANDLE_CONDVAR");
        debug!("Attempting to get app handle");
        while !*initialized {
            initialized = cvar
                .wait(initialized)
                .expect("Failed to wait on condition variable");
        }
    }
    let app = get_app_handle();
    debug!("App handle acquired");

    // Running group = modules that have been started at least once (run_state is Some).
    let running_keys: BTreeSet<String> = modules
        .iter()
        .filter(|(_, run_state)| run_state.is_some())
        .map(|(name, _)| name.clone())
        .collect();
    let mut cache = TRAY_MENU_CACHE
        .lock()
        .expect("Failed to lock tray menu cache");
    match cache.as_ref() {
        // Same running group as last build: ordering is unchanged, so only sync checked state.
        Some(c) if c.running_keys == running_keys => {
            for (name, run_state) in modules.iter() {
                if let (Some(item), Some(running)) = (c.module_items.get(name), run_state) {
                    if let Err(e) = item.set_checked(*running) {
                        error!("Failed to update tray checked state for {name}: {e}");
                    }
                }
            }
            trace!("synced tray menu state");
        }
        // First build, or a module joined the running group: rebuild to reorder the menu.
        _ => {
            let module_items = build_tray_menu(app, modules);
            *cache = Some(TrayMenuCache {
                module_items,
                running_keys,
            });
            trace!("built tray menu");
        }
    }
}

fn build_tray_menu(
    app: &AppHandle,
    modules: &ModulesSnapshot,
) -> HashMap<String, CheckMenuItem<Wry>> {
    let open = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)
        .expect("failed to create open menu item");
    let quit = MenuItem::with_id(app, "quit", "Quit ActivityWatch", true, None::<&str>)
        .expect("failed to create quit menu item");

    let mut module_items = HashMap::new();
    let mut modules_submenu_builder = SubmenuBuilder::new(app, "Modules");
    // Started modules first, alphabetically (BTreeMap iterates in key order), each with a checkbox.
    for (module, run_state) in modules.iter() {
        if let Some(running) = run_state {
            let module_menu =
                CheckMenuItem::with_id(app, module, module, true, *running, None::<&str>)
                    .expect("Failed to create module menu item");
            modules_submenu_builder = modules_submenu_builder.item(&module_menu);
            module_items.insert(module.clone(), module_menu);
        }
    }

    // Then discovered modules that have never been started, alphabetically.
    for (module, run_state) in modules.iter() {
        if run_state.is_none() {
            let module_menu = MenuItem::with_id(app, module, module, true, None::<&str>)
                .expect("Failed to create module menu item");
            modules_submenu_builder = modules_submenu_builder.item(&module_menu);
        }
    }

    let module_submenu = modules_submenu_builder
        .build()
        .expect("Failed to create module submenu");
    let config_folder = MenuItem::with_id(
        app,
        "config_folder",
        "Open config folder",
        true,
        None::<&str>,
    )
    .expect("Failed to create config folder menu item");

    let log_folder = MenuItem::with_id(app, "log_folder", "Open log folder", true, None::<&str>)
        .expect("Failed to create log folder menu item");
    let autostart_item = crate::autostart::build_menu_item(app);
    let separator = PredefinedMenuItem::separator(app).expect("Failed to create separator");
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &separator,
            &module_submenu,
            &separator,
            &autostart_item,
            &config_folder,
            &log_folder,
            &separator,
            &quit,
        ],
    )
    .expect("Failed to create tray menu");

    let tray_id = get_tray_id();
    app.tray_by_id(tray_id)
        .expect("Failed to get tray by id")
        .set_menu(Some(menu))
        .expect("Failed to set tray menu");

    module_items
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<(), nix::Error> {
    let pid = Pid::from_raw(pid as i32);
    let res = signal::kill(pid, Signal::SIGTERM);
    if let Err(e) = res {
        Err(e)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn send_sigterm(pid: u32) -> Result<(), std::io::Error> {
    let pid = pid as DWORD;

    // Open the process with terminate permission
    let process_handle = unsafe { OpenProcess(PROCESS_TERMINATE, FALSE, pid) };

    if process_handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    // Terminate the process with exit code 1
    let result = unsafe { TerminateProcess(process_handle, 1) };

    // Close the process handle
    unsafe { CloseHandle(process_handle) };

    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn create_job_object() -> Result<HANDLE, std::io::Error> {
    unsafe {
        // Create a new job object
        let job_handle = CreateJobObjectW(null_mut(), null_mut());
        if job_handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        // Set job object to kill all associated processes when it's closed
        let mut job_info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        job_info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        let result = SetInformationJobObject(
            job_handle,
            JobObjectExtendedLimitInformation,
            &mut job_info as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as DWORD,
        );

        if result == 0 {
            CloseHandle(job_handle);
            return Err(std::io::Error::last_os_error());
        }

        Ok(job_handle)
    }
}

/// Have the kernel send SIGTERM to the module if aw-tauri dies.
///
/// PDEATHSIG fires when the *thread* that spawned the child exits, not the process. That holds
/// here because each module thread blocks on its child until the child exits.
///
/// On macOS see the watchdog-based version below; on Windows the job object covers this.
#[cfg(target_os = "linux")]
fn kill_on_parent_death(command: &mut Command) {
    use nix::sys::prctl;
    use std::os::unix::process::CommandExt;

    let parent = nix::unistd::getpid();
    // SAFETY: prctl and getppid are async-signal-safe syscalls and the closure doesn't allocate.
    unsafe {
        command.pre_exec(move || {
            prctl::set_pdeathsig(Signal::SIGTERM)?;
            // aw-tauri may have died between fork and prctl, in which case no signal will come.
            if nix::unistd::getppid() != parent {
                return Err(std::io::Error::other(
                    "aw-tauri exited before module started",
                ));
            }
            Ok(())
        });
    }
}

/// Have modules receive SIGTERM if aw-tauri dies, including by crash, Force Quit or SIGKILL.
///
/// macOS has no PDEATHSIG, so every module joins a process group led by a small watchdog shell.
/// The watchdog polls aw-tauri's pid once a second and, once it's gone, sends SIGTERM to the whole
/// group, which also covers helper processes the modules started themselves.
#[cfg(target_os = "macos")]
fn kill_on_parent_death(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    use std::sync::OnceLock;

    static WATCHDOG_GROUP: OnceLock<Option<i32>> = OnceLock::new();
    let group = *WATCHDOG_GROUP.get_or_init(|| match spawn_watchdog(std::process::id()) {
        Ok(watchdog) => Some(watchdog.id() as i32),
        Err(e) => {
            error!("Failed to start module watchdog; modules may outlive a crash: {e}");
            None
        }
    });
    let Some(group) = group else { return };
    // Joining a group that no longer exists (watchdog killed, no modules left) would make the
    // spawn itself fail, so only join a live one.
    if signal::killpg(Pid::from_raw(group), None).is_ok() {
        command.process_group(group);
    } else {
        warn!("Module watchdog is gone; this module won't be stopped if aw-tauri crashes");
    }
}

/// Spawns the watchdog as the leader of a new process group. It ignores SIGTERM itself so the
/// final `kill -TERM 0` (every process in its group) reaches the modules and then lets it exit.
#[cfg(target_os = "macos")]
fn spawn_watchdog(parent: u32) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let script =
        format!("trap '' TERM; while kill -0 {parent} 2>/dev/null; do sleep 1; done; kill -TERM 0");
    // Long-lived like the modules, so it must not inherit their pipes either.
    let _guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    Command::new("/bin/sh")
        .args(["-c", &script])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

// Splits a configured args string, warning (and falling back to no args) on malformed shell
// quoting instead of silently dropping the args without a diagnostic.
fn split_module_args(module_name: &str, args_str: &str) -> Vec<String> {
    shell_words::split(args_str).unwrap_or_else(|e| {
        warn!("Failed to parse args for module {module_name} ({args_str:?}): {e}");
        Vec::new()
    })
}

// Builds the baseline args used whenever a module is started without explicit args (manual
// tray click, or restart after a crash), sourced from both `module_args` and `autostart.modules`
// entries, so modules don't need to be autostarted to get default args (#131). Where a module
// appears in both, the inline args on its `autostart.modules` entry win, since they're the more
// specific setting.
fn configured_modules_args() -> HashMap<String, Option<Vec<String>>> {
    let config = get_config();
    let mut modules_args = HashMap::new();

    for (name, args_str) in config.module_args.iter() {
        if !args_str.is_empty() {
            modules_args.insert(name.clone(), Some(split_module_args(name, args_str)));
        }
    }

    for module_entry in config.autostart.modules.iter() {
        let args_str = module_entry.args();
        if !args_str.is_empty() {
            modules_args.insert(
                module_entry.name().to_string(),
                Some(split_module_args(module_entry.name(), args_str)),
            );
        }
    }

    modules_args
}

pub fn start_manager() -> Arc<Mutex<ManagerState>> {
    start_manager_inner(get_config().port, None)
}

pub(crate) fn start_manager_with_port(server_port: u16) -> Arc<Mutex<ManagerState>> {
    start_manager_inner(server_port, None)
}

pub(crate) fn start_manager_with_events(
    server_port: u16,
    event_tx: Sender<ManagerEvent>,
) -> Arc<Mutex<ManagerState>> {
    start_manager_inner(server_port, Some(event_tx))
}

fn start_manager_inner(
    server_port: u16,
    event_tx: Option<Sender<ManagerEvent>>,
) -> Arc<Mutex<ManagerState>> {
    let (tx, rx) = channel();
    let state = Arc::new(Mutex::new(ManagerState::new(tx.clone(), server_port)));

    // Start the modules. Args come from the baseline computed in ManagerState::new().
    let config = get_config();
    for module_entry in config.autostart.modules.iter() {
        let name = module_entry.name();
        state
            .lock()
            .expect("Failed to acquire manager_state lock")
            .start_module(name, None);
    }

    // Force an initial tray build even if no modules autostart (no Started message would arrive).
    tx.send(ModuleMessage::Init {})
        .expect("Failed to send \"Module Init\" message");

    let state_clone = Arc::clone(&state);
    thread::spawn(move || {
        handle(rx, state_clone, event_tx);
    });
    state
}

fn handle(
    rx: Receiver<ModuleMessage>,
    state: Arc<Mutex<ManagerState>>,
    event_tx: Option<Sender<ManagerEvent>>,
) {
    loop {
        let msg = rx.recv().expect("Failed to receive Module message");
        let state_clone = Arc::clone(&state);

        // aw-notify notifications are forwarded directly without touching tray state
        if let ModuleMessage::Notification { title, message } = msg {
            route_notification(&event_tx, &title, &message);
            continue;
        }

        let event_tx_for_restart = event_tx.clone();
        let snapshot = {
            let mut state_guard = state.lock().expect("Failed to acquire manager_state lock");
            match msg {
                ModuleMessage::Started { name, pid, args } => {
                    // Capture whether this start follows a crash restart before clearing state.
                    let was_recovery = state_guard
                        .modules
                        .get(&name)
                        .is_some_and(|m| m.restart_count > 0);
                    state_guard.started_module(&name, pid, args);
                    if was_recovery {
                        // Module came back after a crash — update the open alert window in place.
                        show_module_recovered(&event_tx, &name);
                    }
                }
                ModuleMessage::Stopped { name, output } => {
                    state_guard.stopped_module(&name);
                    let name_clone = name.clone();
                    if output.status.success() {
                        info!("Module {name} exited successfully");
                    } else {
                        // Restart path logs up to 3 attempt lines; keep this at debug to avoid spam.
                        debug!("Module {name} exited with error status");
                        thread::spawn(move || {
                            let (should_restart, restart_info) = {
                                let state_guard = state_clone
                                    .lock()
                                    .expect("Failed to acquire manager_state lock");
                                let module = state_guard.modules.get(&name_clone);

                                // If shutdown is pending, exit early
                                if module.is_some_and(|m| m.pending_shutdown) {
                                    return; // Exit the entire thread
                                }

                                let restart_count = module.map_or(0, |m| m.restart_count);
                                let generation = module.map_or(0, |m| m.generation);
                                if restart_count < 3 {
                                    // Exponential backoff: 2^(restart_count + 1) seconds
                                    // restart_count 0 -> 2 seconds, 1 -> 4 seconds, 2 -> 8 seconds
                                    let delay_secs = 2u64.pow(restart_count + 1);
                                    (true, Some((delay_secs, restart_count, generation)))
                                } else {
                                    (false, None)
                                }
                            };

                            if should_restart {
                                if let Some((secs, restart_count, generation)) = restart_info {
                                    let attempt = restart_count + 1;
                                    // One log line per restart attempt (max 3 total with limit case).
                                    error!(
                                        "Module {name_clone} crashed (attempt {attempt} of 3); restarting in {secs}s"
                                    );
                                    show_module_warning(
                                        &event_tx_for_restart,
                                        &name_clone,
                                        &format!(
                                            "Stopped unexpectedly. Restarting in {secs} seconds…\n\
                                             Automatic retry {attempt} of 3 — you don’t need to do anything yet."
                                        ),
                                        module_alert_ui::StatusKind::Warning,
                                    );

                                    thread::sleep(Duration::from_secs(secs));

                                    let mut state_guard = state_clone
                                        .lock()
                                        .expect("Failed to acquire manager_state lock");

                                    let Some(module) = state_guard.modules.get_mut(&name_clone)
                                    else {
                                        return;
                                    };
                                    // The user may have started the module during the backoff (and
                                    // maybe stopped it again, or it crashed and scheduled its own
                                    // retry). Any start since the crash supersedes this retry.
                                    if module.generation != generation || module.pending_shutdown {
                                        debug!("Skipping automatic restart of {name_clone}: state changed during backoff");
                                        return;
                                    }
                                    module.restart_count = restart_count + 1;
                                    // start_module falls back to the args this module was last started with
                                    state_guard.start_module(&name_clone, None);
                                }
                            } else {
                                // Restart limit reached — update UI; no 4th log line (cap is 3).
                                let mut state_guard = state_clone
                                    .lock()
                                    .expect("Failed to acquire manager_state lock");
                                if let Some(module) = state_guard.modules.get_mut(&name_clone) {
                                    module.pending_shutdown = true;
                                }
                                show_module_warning(
                                    &event_tx_for_restart,
                                    &name_clone,
                                    &format!(
                                        "Couldn’t restart after several tries, so it was left stopped.\n\n\
                                         Open the ActivityWatch tray menu and choose Open log folder, \
                                         then check the log for “{name_clone}” for the error details."
                                    ),
                                    module_alert_ui::StatusKind::Warning,
                                );
                            }
                        });

                        debug!(
                            "Module {name} stdout: {}",
                            String::from_utf8_lossy(&output.stdout)
                        );
                        error!(
                            "Module {name} stderr: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                }
                ModuleMessage::Init {} => {}
                // Already handled above via early-continue
                ModuleMessage::Notification { .. } => unreachable!(),
            }

            // Build the snapshot only if something will consume it: the mini event loop
            // (event_tx) or the Tauri tray (GUI mode). In --daemon mode nobody does, so skip it.
            if event_tx.is_some() || !crate::is_daemon_mode() {
                Some(state_guard.modules_snapshot())
            } else {
                None
            }
        };
        if let Some(snapshot) = snapshot {
            update_tray_menu(snapshot, &event_tx);
        }
    }
}

fn start_module_thread(
    name: String,
    path: PathBuf,
    custom_args: Option<Vec<String>>,
    server_port: u16,
    tx: Sender<ModuleMessage>,
) {
    // Special handling for aw-notify module
    if name == "aw-notify" {
        info!("Using special aw-notify handler for module: {name}");
        start_notify_module_thread(name, path, custom_args, server_port, tx);
        return;
    }

    start_generic_module_thread(name, path, custom_args, server_port, tx);
}

fn start_generic_module_thread(
    name: String,
    path: PathBuf,
    custom_args: Option<Vec<String>>,
    server_port: u16,
    tx: Sender<ModuleMessage>,
) {
    thread::spawn(move || {
        // Create job object on Windows to ensure child dies with parent
        #[cfg(windows)]
        let job_handle = match create_job_object() {
            Ok(handle) => Some(handle),
            Err(e) => {
                error!("Failed to create job object for {name}: {e}");
                None
            }
        };

        // Start the child process
        let mut command = Command::new(&path);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        kill_on_parent_death(&mut command);

        // Use custom args if provided, otherwise only pass port arg if it's not the default (5600)
        if let Some(ref args) = custom_args {
            command.args(args);
        } else if server_port != 5600 {
            command.args(["--port", server_port.to_string().as_str()]);
        }

        // Set creation flags on Windows to hide console window
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        // Pipe stderr too: it's where modules report why they crashed, and when inherited it went
        // to aw-tauri's own stderr, which is /dev/null for a GUI app.
        let child = {
            let _guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
        };

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to start module {name}: {e}");
                #[cfg(windows)]
                if let Some(handle) = job_handle {
                    unsafe {
                        CloseHandle(handle);
                    }
                }
                return;
            }
        };

        let child_pid = child.id();

        // On Windows, assign child to job object
        #[cfg(windows)]
        if let Some(handle) = job_handle {
            use std::os::windows::io::AsRawHandle;
            let child_handle = child.as_raw_handle() as HANDLE;
            unsafe {
                if AssignProcessToJobObject(handle, child_handle) == 0 {
                    error!(
                        "Failed to assign child process to job object: {:?}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        }

        // Send a message to the manager that the module has started
        tx.send(ModuleMessage::Started {
            name: name.to_string(),
            pid: child_pid,
            args: custom_args,
        })
        .expect("Failed to send Module Started message");

        // Drain both pipes on their own threads, keeping only a bounded tail of each for the crash
        // log. wait_with_output() would buffer everything a module prints for its whole run,
        // which for a watcher can be weeks.
        let stdout = child.stdout.take().expect("Failed to get stdout");
        let stdout_reader = thread::spawn(move || read_tail(stdout, OUTPUT_TAIL_BYTES));
        let stderr = child.stderr.take().expect("Failed to get stderr");
        let stderr_reader = thread::spawn(move || read_tail(stderr, OUTPUT_TAIL_BYTES));

        // Wait for the child to exit
        let status = child.wait().expect("Failed to wait on child process");
        let output = std::process::Output {
            status,
            stdout: stdout_reader.join().unwrap_or_default(),
            stderr: stderr_reader.join().unwrap_or_default(),
        };

        // Clean up job handle on Windows
        #[cfg(windows)]
        if let Some(handle) = job_handle {
            unsafe {
                CloseHandle(handle);
            }
        }

        // Send the process output to the manager
        tx.send(ModuleMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .expect("Failed to send module stopped message");
    });
}

fn start_notify_module_thread(
    name: String,
    path: PathBuf,
    custom_args: Option<Vec<String>>,
    server_port: u16,
    tx: Sender<ModuleMessage>,
) {
    thread::spawn(move || {
        // Create job object on Windows to ensure child dies with parent
        #[cfg(windows)]
        let job_handle = match create_job_object() {
            Ok(handle) => Some(handle),
            Err(e) => {
                error!("Failed to create job object for {name}: {e}");
                None
            }
        };

        // Start the child process with --output-only flag
        let mut command = Command::new(&path);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        kill_on_parent_death(&mut command);

        // Always add --output-only flag for aw-notify
        let mut args = vec!["--output-only".to_string()];

        // Add port argument if not default (5600)
        if server_port != 5600 {
            args.push("--port".to_string());
            args.push(server_port.to_string());
        }

        // Add any custom args
        if let Some(ref custom) = custom_args {
            args.extend_from_slice(custom);
        }

        command.args(&args);

        // Set creation flags on Windows to hide console window
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let child = {
            let _guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
        };
        let mut child = match child {
            Ok(child) => child,
            Err(e) => {
                // An unsupported --output-only is detected after exit (below), not here: spawn
                // errors are OS errors and never contain the child's usage message.
                error!("Failed to start module {name}: {e}");
                #[cfg(windows)]
                if let Some(handle) = job_handle {
                    unsafe {
                        CloseHandle(handle);
                    }
                }
                return;
            }
        };

        // On Windows, assign child to job object
        #[cfg(windows)]
        if let Some(handle) = job_handle {
            use std::os::windows::io::AsRawHandle;
            let child_handle = child.as_raw_handle() as HANDLE;
            unsafe {
                if AssignProcessToJobObject(handle, child_handle) == 0 {
                    error!(
                        "Failed to assign child process to job object: {:?}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        }

        // Report the caller-provided args, NOT the internally-expanded `args`: the
        // `--output-only`/`--port` flags are re-added on every start, so storing the expanded
        // command line would re-inject them and compound across a stop/start or restart (the
        // module would be relaunched with duplicated flags and fail to come back up).
        tx.send(ModuleMessage::Started {
            name: name.to_string(),
            pid: child.id(),
            args: custom_args.clone(),
        })
        .expect("Failed to send module started message");

        // Drain stderr concurrently: aw-notify logs there, and if nothing reads the pipe it fills
        // up (~64 KiB) and the child blocks on its next log write while we wait on stdout.
        let stderr = child.stderr.take().expect("Failed to get stderr");
        let stderr_reader = thread::spawn(move || read_tail(stderr, OUTPUT_TAIL_BYTES));

        let stdout = child.stdout.take().expect("Failed to get stdout");
        let reader = BufReader::new(stdout);

        for line in reader.lines() {
            match line {
                Ok(line_str) => {
                    info!("aw-notify output: {}", line_str);
                    if line_str.starts_with("{") {
                        if let Ok(notification) =
                            serde_json::from_str::<serde_json::Value>(&line_str)
                        {
                            info!("aw-notify notification: {}", notification);
                            if let (Some(title), Some(message)) = (
                                notification.get("title").and_then(|t| t.as_str()),
                                notification.get("message").and_then(|m| m.as_str()),
                            ) {
                                tx.send(ModuleMessage::Notification {
                                    title: title.to_string(),
                                    message: message.to_string(),
                                })
                                .expect("Failed to send notification message");
                                info!(
                                    "Parsed JSON notification: title='{}', message length={}",
                                    title,
                                    message.len()
                                );
                            } else {
                                debug!("JSON notification missing title or message fields");
                            }
                        } else {
                            debug!("Failed to parse JSON line: {}", line_str);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to read line from aw-notify: {}", e);
                }
            }
        }

        // Wait for the child to exit
        let status = child.wait().expect("Failed to wait on child");
        let output = std::process::Output {
            status,
            // stdout was consumed line by line above
            stdout: Vec::new(),
            stderr: stderr_reader.join().unwrap_or_default(),
        };

        // Check if the process failed due to unsupported --output-only flag
        // Exit code 2 is commonly used by clap/click for argument errors
        if output.status.code() == Some(2) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No such option: --output-only") {
                info!("aw-notify module doesn't support --output-only, falling back to default behavior");

                // Clean up job handle before fallback
                #[cfg(windows)]
                if let Some(handle) = job_handle {
                    unsafe {
                        CloseHandle(handle);
                    }
                }
                // Fallback to generic module handler
                start_generic_module_thread(name, path, custom_args, server_port, tx);
                return;
            }
        }

        // Clean up job handle on Windows
        #[cfg(windows)]
        if let Some(handle) = job_handle {
            unsafe {
                CloseHandle(handle);
            }
        }

        // Send the process output to the manager
        tx.send(ModuleMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .expect("Failed to send module stopped message");
    });
}

/// Serializes module spawns. On macOS, std creates a child's stdio pipes with pipe() and only then
/// marks them close-on-exec, so a module spawned at the same moment from another thread can
/// inherit them. The pipe then stays open after its module exits, the reader never sees EOF, and
/// the Stopped message is delayed until the other module exits too.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// How much of a module's stdout/stderr to keep for crash diagnostics.
const OUTPUT_TAIL_BYTES: usize = 64 * 1024;

/// Reads `reader` to EOF, keeping only the last `max` bytes so a chatty module can't grow memory
/// without bound over a long run.
fn read_tail(mut reader: impl Read, max: usize) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > max {
                    tail.drain(..tail.len() - max);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                warn!("Failed to read module output: {e}");
                break;
            }
        }
    }
    tail
}

/// Route a notification: send via ManagerEvent channel (mini mode) or Tauri (GUI mode).
fn route_notification(event_tx: &Option<Sender<ManagerEvent>>, title: &str, message: &str) {
    if let Some(tx) = event_tx {
        let _ = tx.send(ManagerEvent::Notification {
            title: title.to_string(),
            message: message.to_string(),
        });
        return;
    }
    send_tauri_notification(title, message);
}

/// Compact one-line text for tray notifications (no multi-line body).
fn notification_summary(module_name: &str, message: &str) -> String {
    let first_line = message.lines().next().unwrap_or(message).trim();
    format!("{module_name}: {first_line}")
}

/// Route a module crash warning: mini → notification, daemon → log, GUI → replaceable webview.
fn show_module_warning(
    event_tx: &Option<Sender<ManagerEvent>>,
    module_name: &str,
    message: &str,
    kind: module_alert_ui::StatusKind,
) {
    if let Some(tx) = event_tx {
        let _ = tx.send(ManagerEvent::Notification {
            title: "ActivityWatch".to_string(),
            message: notification_summary(module_name, message),
        });
        return;
    }
    if crate::is_daemon_mode() {
        warn!("{module_name}: {message}");
        return;
    }
    let app = get_app_handle();
    module_alert_ui::show_or_update(app, module_name, message, kind);
}

/// Update the module-alert UI when a module comes back after a crash restart.
/// Does not open a new window if the user already dismissed it.
fn show_module_recovered(event_tx: &Option<Sender<ManagerEvent>>, module_name: &str) {
    let message = "Back up and running again.";
    if let Some(tx) = event_tx {
        let _ = tx.send(ManagerEvent::Notification {
            title: "ActivityWatch".to_string(),
            message: format!("{module_name}: running again"),
        });
        return;
    }
    if crate::is_daemon_mode() {
        info!("{module_name}: recovered and running again");
        return;
    }
    let app = get_app_handle();
    module_alert_ui::update_if_open(app, module_name, message, module_alert_ui::StatusKind::Ok);
}

fn send_tauri_notification(title: &str, message: &str) {
    if crate::is_daemon_mode() {
        info!(
            "Notification (suppressed in daemon mode): {} — {}",
            title, message
        );
        return;
    }
    let result = get_app_handle()
        .notification()
        .builder()
        .title(title)
        .body(message)
        .show();

    match result {
        Ok(_) => {
            trace!(
                "Sent notification: title='{}', message preview='{}'",
                title,
                message.lines().next().unwrap_or("")
            );
        }
        Err(e) => {
            error!("Failed to send notification: {}", e);
        }
    }
}

#[cfg(unix)]
fn discover_modules() -> BTreeMap<String, PathBuf> {
    let excluded = [
        "aw-tauri",
        "aw-client",
        "aw-cli",
        "aw-qt",
        "aw-server",
        "aw-server-rust",
        "aw-watcher-window-macos",
    ];
    let config = crate::get_config();

    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = env::split_paths(&path).collect::<Vec<_>>();

    // check each path in discovery_paths and add it to the start of the paths list if it's not already there
    for path in config.discovery_paths.iter() {
        if !paths.contains(path) {
            paths.insert(0, path.to_owned());
        }
    }

    // Create new PATH-like string
    let new_paths = env::join_paths(paths).unwrap_or_default();

    // Search starting with PATH entries
    let found_modules = find_modules_in(env::split_paths(&new_paths).collect(), &excluded);

    debug!(
        "Discovered modules: {:?}",
        found_modules.keys().collect::<Vec<_>>()
    );
    found_modules
}

/// Searches `dirs_to_search`, and any `aw-*` subdirectories, for executable `aw-*` modules.
#[cfg(unix)]
fn find_modules_in(
    mut dirs_to_search: Vec<PathBuf>,
    excluded: &[&str],
) -> BTreeMap<String, PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let mut found_modules = BTreeMap::new();
    // Use (device, inode) pairs for cycle detection (works across filesystems)
    let mut visited_inodes = HashSet::new();

    // Process directories in depth-first order
    while let Some(dir) = dirs_to_search.pop() {
        // Use (device, inode) tuple to detect cycles (works across different filesystems)
        if let Ok(metadata) = fs::metadata(&dir) {
            let id = (metadata.dev(), metadata.ino());
            if !visited_inodes.insert(id) {
                continue; // Already visited this directory
            }
        } else {
            continue; // Can't access directory
        }

        // Look for aw-* executables in this directory
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                // Filter by name before touching metadata: PATH holds thousands of entries and
                // only a handful are aw-*, so this skips a stat call for almost all of them.
                let file_name = match entry.file_name().into_string() {
                    Ok(name) if name.starts_with("aw-") => name,
                    _ => continue,
                };
                let path = entry.path();

                // Follow symlinks: a symlink's own mode is always 0o777 on Linux, so checking it
                // (as entry.metadata() would) accepted any aw-* link, even one to a
                // non-executable or missing file, and never descended into linked directories.
                let metadata = match fs::metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue, // broken symlink or unreadable
                };

                // If it's a directory starting with "aw-", add to search stack
                if metadata.is_dir() {
                    dirs_to_search.push(path);
                }
                // If it's an executable file
                else if metadata.is_file() {
                    // Skip if has extension or is excluded
                    if file_name.contains('.') || excluded.contains(&file_name.as_str()) {
                        continue;
                    }

                    // Check if executable
                    let is_executable = metadata.permissions().mode() & 0o111 != 0;
                    if is_executable {
                        found_modules.insert(file_name, path);
                    }
                }
            }
        }
    }

    found_modules
}

#[cfg(windows)]
fn discover_modules() -> BTreeMap<String, PathBuf> {
    let excluded = [
        "aw-tauri",
        "aw-client",
        "aw-cli",
        "aw-qt",
        "aw-server",
        "aw-server-rust",
    ];
    let config = crate::get_config();

    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = env::split_paths(&path).collect::<Vec<_>>();

    // check each path in discovery_paths and add it to the start of the paths list if it's not already there
    for path in config.discovery_paths.iter() {
        if !paths.contains(path) {
            paths.insert(0, path.to_owned());
        }
    }

    let new_paths = env::join_paths(paths).unwrap_or_default();

    // Build a set of paths to search
    let mut found_modules = BTreeMap::new();
    let mut visited_dirs = HashSet::new();

    // Create a stack of directories to search, starting with PATH entries
    let mut dirs_to_search: Vec<PathBuf> = env::split_paths(&new_paths).collect();

    // Process directories in depth-first order
    while let Some(dir) = dirs_to_search.pop() {
        // Skip if already visited
        if !visited_dirs.insert(dir.clone()) {
            continue;
        }

        // Look for aw-* executables in this directory
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                // Filter by name before touching metadata: PATH holds thousands of entries and
                // only a handful are aw-*, so this skips a stat call for almost all of them.
                let file_name = match entry.file_name().into_string() {
                    Ok(name) if name.starts_with("aw-") => name,
                    _ => continue,
                };
                let path = entry.path();

                // Skip if not a file or directory
                if let Ok(metadata) = fs::metadata(&path) {
                    // If it's a directory starting with "aw-", add to search stack
                    if metadata.is_dir() {
                        dirs_to_search.push(path);
                    }
                    // If it's an executable file
                    else if metadata.is_file() && file_name.ends_with(".exe") {
                        // Extract name without .exe suffix
                        let name = match file_name.strip_suffix(".exe") {
                            Some(name) => name.to_lowercase(),
                            None => continue,
                        };

                        // Skip if excluded
                        if excluded.contains(&name.as_str()) {
                            continue;
                        }

                        found_modules.insert(name, path);
                    }
                }
            }
        }
    }

    found_modules
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_module(
        name: &str,
        restart_count: u32,
        started_at: Option<Instant>,
    ) -> ManagerState {
        let module = Module {
            path: PathBuf::from("/nonexistent"),
            run_state: Some(true),
            pid: Some(1),
            started_at,
            restart_count,
            generation: 1,
            pending_shutdown: false,
            args: None,
        };
        ManagerState {
            tx: channel().0,
            server_port: 5600,
            modules: BTreeMap::from([(name.to_string(), module)]),
        }
    }

    #[test]
    fn crash_after_stable_run_resets_restart_count() {
        let mut state =
            state_with_module("aw-watcher", 3, Instant::now().checked_sub(STABLE_UPTIME));
        state.stopped_module("aw-watcher");
        assert_eq!(state.modules["aw-watcher"].restart_count, 0);
    }

    #[test]
    fn manual_start_resets_restart_count() {
        let mut state = state_with_module("aw-watcher", 3, None);
        state.stopped_module("aw-watcher");
        state.handle_system_click("aw-watcher");
        assert_eq!(state.modules["aw-watcher"].restart_count, 0);
    }

    #[test]
    fn crash_loop_keeps_restart_count() {
        let mut state = state_with_module("aw-watcher", 2, Some(Instant::now()));
        state.stopped_module("aw-watcher");
        assert_eq!(state.modules["aw-watcher"].restart_count, 2);
    }

    /// Runs `script` through the generic module path and returns its Stopped output.
    #[cfg(unix)]
    fn run_generic_module(script: &str) -> std::process::Output {
        let (tx, rx) = channel();
        let args = vec!["-c".to_string(), script.to_string()];
        start_generic_module_thread("sh".into(), "/bin/sh".into(), Some(args), 5600, tx);
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(ModuleMessage::Stopped { output, .. }) => return output,
                Ok(_) => continue,
                Err(e) => panic!("module never stopped: {e}"),
            }
        }
    }

    /// Starting modules concurrently must not leak one module's output pipes into another:
    /// if a long-running module inherits them, the short one never reports Stopped.
    #[cfg(unix)]
    #[test]
    fn concurrent_spawns_dont_delay_stopped() {
        let (tx, rx) = channel();
        let sh = |script: &str, tx: &Sender<ModuleMessage>| {
            let args = vec!["-c".to_string(), script.to_string()];
            start_generic_module_thread(
                "sh".into(),
                "/bin/sh".into(),
                Some(args),
                5600,
                tx.clone(),
            );
        };
        const QUICK: usize = 40;
        for _ in 0..QUICK {
            sh("sleep 20; : aw-tauri-spawn-test", &tx);
            sh("exit 0", &tx);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stopped = 0;
        while stopped < QUICK {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(ModuleMessage::Stopped { .. }) => stopped += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        // Clean up the sleepers.
        let _ = Command::new("pkill")
            .args(["-f", "aw-tauri-spawn-test"])
            .status();
        assert_eq!(
            stopped, QUICK,
            "some short-lived modules never reported Stopped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn generic_module_captures_stderr() {
        let output = run_generic_module("echo boom >&2; exit 3");
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stderr, b"boom\n");
    }

    #[cfg(unix)]
    #[test]
    fn generic_module_keeps_only_output_tail() {
        // ~1 MiB on stdout, then a marker that must survive in the tail.
        let output =
            run_generic_module("head -c 1048576 /dev/zero | tr '\\0' x; echo; echo tail-marker");
        assert!(output.status.success());
        assert!(output.stdout.len() <= OUTPUT_TAIL_BYTES);
        assert!(output.stdout.ends_with(b"tail-marker\n"));
    }

    #[cfg(unix)]
    #[test]
    fn find_modules_follows_symlinks_to_their_targets() {
        use std::os::unix::fs::symlink;

        let dir = env::temp_dir().join(format!("aw-tauri-discover-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        let nested = dir.join("nested");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&nested).unwrap();

        let make = |path: PathBuf, mode: u32| {
            fs::write(&path, "").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let exec = make(dir.join("real-exec"), 0o755);
        let plain = make(dir.join("real-plain"), 0o644);
        make(nested.join("aw-nested"), 0o755);

        symlink(&exec, bin.join("aw-good")).unwrap();
        symlink(&plain, bin.join("aw-not-executable")).unwrap();
        symlink(dir.join("missing"), bin.join("aw-broken")).unwrap();
        symlink(&nested, bin.join("aw-linked-dir")).unwrap();

        let found = find_modules_in(vec![bin], &[]);
        assert_eq!(
            found.keys().map(String::as_str).collect::<Vec<_>>(),
            ["aw-good", "aw-nested"]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn watchdog_terminates_group_when_parent_dies() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};

        let mut parent = Command::new("sleep").arg("60").spawn().unwrap();
        let mut watchdog = spawn_watchdog(parent.id()).unwrap();
        let mut module = Command::new("sleep")
            .arg("60")
            .process_group(watchdog.id() as i32)
            .spawn()
            .unwrap();

        // Still running while the parent is alive.
        thread::sleep(Duration::from_millis(1500));
        assert!(module.try_wait().unwrap().is_none());

        parent.kill().unwrap();
        parent.wait().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = module.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = module.kill();
                panic!("module outlived its parent");
            }
            thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
        watchdog.wait().unwrap();
    }

    #[test]
    fn read_tail_keeps_everything_under_the_limit() {
        assert_eq!(read_tail(&b"hello"[..], 64), b"hello");
    }

    #[test]
    fn read_tail_keeps_only_the_last_bytes() {
        let input: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(read_tail(&input[..], 1000), &input[input.len() - 1000..]);
    }
}
