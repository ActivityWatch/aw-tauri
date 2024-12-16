#[cfg(unix)]
use nix::sys::signal::{self, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
/// A process manager for ActivityWatch
///
/// Used to start, stop and manage the lifecycle modules like aw-watcher-afk and aw-watcher-window.
/// A module is a process that runs in the background and sends events to the ActivityWatch server.
///
/// The manager is responsible for starting and stopping the modules, and for keeping track of
/// their state.
///
/// If a module crashes, the manager will notify the user and ask if they want to restart it.
use std::collections::{BTreeMap, BTreeSet, HashMap};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{
    mpsc::{channel, Receiver, Sender},
    Arc, Mutex,
};
use std::time::Duration;
// use std::thread::sleep;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use std::{env, fs, thread};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, SubmenuBuilder};

// use tauri::{CustomMenuItem, SystemTrayMenu, SystemTrayMenuItem, SystemTraySubmenu};
#[cfg(windows)]
use winapi::shared::minwindef::DWORD;
#[cfg(windows)]
use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};

use crate::{get_app_handle, get_config, get_tray_id, HANDLE_CONDVAR};

#[derive(Debug)]
pub enum ModuleMessage {
    Started {
        name: String,
        pid: u32,
    },
    Stopped {
        name: String,
        output: std::process::Output,
    },
    Init {},
}

#[derive(Debug)]
pub struct ManagerState {
    tx: Sender<ModuleMessage>,
    pub modules_running: BTreeMap<String, bool>,
    pub modules_in_path: BTreeSet<String>,
    pub modules_pid: HashMap<String, u32>,
    pub modules_menu_set: bool,
}

impl ManagerState {
    fn new(tx: Sender<ModuleMessage>) -> ManagerState {
        ManagerState {
            tx,
            modules_running: BTreeMap::new(),
            modules_in_path: get_modules_in_path(),
            modules_pid: HashMap::new(),
            modules_menu_set: false,
        }
    }
    fn started_module(&mut self, name: &str, pid: u32) {
        println!("started {name}");
        self.modules_running.insert(name.to_string(), true);
        self.modules_pid.insert(name.to_string(), pid);
        println!("{:?}", self.modules_running);
        self.update_tray_menu();
    }
    fn stopped_module(&mut self, name: &str) {
        println!("stopped {name}");
        self.modules_running.insert(name.to_string(), false);
        self.modules_pid.remove(name);
        self.update_tray_menu();
    }
    fn update_tray_menu(&mut self) {
        let (lock, cvar) = &*HANDLE_CONDVAR;
        let mut state = lock.lock().unwrap();

        println!("trying to get app handle");
        while !*state {
            state = cvar.wait(state).unwrap();
        }
        println!("cvar set");
        let app = &*get_app_handle().lock().expect("failed to get app handle");
        println!("got app handle");

        let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)
            .expect("failed to create open menu item");
        let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
            .expect("failed to create quit menu item");
        // let mut module_menu = SystemTrayMenu::new();

        let mut modules_submenu_builder = SubmenuBuilder::new(app, "Modules");
        for (module, running) in self.modules_running.iter() {
            let label = module;
            let module_menu =
                CheckMenuItem::with_id(app, module, &label, true, *running, None::<&str>)
                    .expect("failed to create module menu item");
            modules_submenu_builder = modules_submenu_builder.item(&module_menu);
        }

        for module in self.modules_in_path.iter() {
            if !self.modules_running.contains_key(module) {
                let module_menu = MenuItem::with_id(app, module, module, true, None::<&str>)
                    .expect("failed to create module menu item");
                modules_submenu_builder = modules_submenu_builder.item(&module_menu);
            }
        }

        let module_submenu = modules_submenu_builder
            .build()
            .expect("failed to create module submenu");
        let menu = Menu::with_items(app, &[&open, &module_submenu, &quit])
            .expect("failed to create tray menu");

        let tray_id = get_tray_id();
        app.tray_by_id(tray_id)
            .expect("failed to get tray by id")
            .set_menu(Some(menu))
            .unwrap();
        println!("set tray menu");
    }
    pub fn start_module(&self, name: &str) {
        if !self.is_module_running(name) {
            start_module_thread(name.to_string(), self.tx.clone());
        }
    }
    pub fn stop_module(&self, name: &str) {
        if let Some(pid) = self.modules_pid.get(name) {
            match send_sigterm(*pid) {
                Ok(_) => {
                    println!("sent SIGTERM to {name}");
                }
                Err(e) => {
                    println!("failed to send SIGTERM to {name}: {e}");
                }
            }
        }
    }
    pub fn stop_modules(&self) {
        for (name, _pid) in self.modules_pid.iter() {
            self.stop_module(name);
        }
    }
    pub fn handle_system_click(&mut self, name: &str) {
        if self.is_module_running(name) {
            self.stop_module(name);
        } else {
            self.start_module(name);
        }
    }
    fn is_module_running(&self, name: &str) -> bool {
        *self.modules_running.get(name).unwrap_or(&false)
    }
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
    // Get the process ID of the child process
    let pid = pid as DWORD;

    // Send SIGTERM signal to the process
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
        return Err(std::io::Error::last_os_error());
    } else {
        return Ok(());
    }
    Ok(())
}
pub fn start_manager() -> Arc<Mutex<ManagerState>> {
    let (tx, rx) = channel();
    let state = Arc::new(Mutex::new(ManagerState::new(tx.clone())));

    // Start the modules
    let autostart_modules = &*get_config().autostart_modules;
    for module in autostart_modules.iter() {
        state.lock().unwrap().start_module(module);
    }

    // populate the tray menu if not yet already done
    let modules_menu_set = state.lock().unwrap().modules_menu_set;
    if !modules_menu_set {
        tx.send(ModuleMessage::Init {}).unwrap();
    }

    let state_clone = Arc::clone(&state);
    thread::spawn(move || {
        handle(rx, state_clone);
    });
    state
}

fn handle(rx: Receiver<ModuleMessage>, state: Arc<Mutex<ManagerState>>) {
    loop {
        let msg = rx.recv().unwrap();
        let state_clone = Arc::clone(&state);
        let state = &mut state.lock().unwrap();
        match msg {
            ModuleMessage::Started { name, pid } => {
                state.started_module(&name, pid);
            }
            ModuleMessage::Stopped { name, output } => {
                state.stopped_module(&name);
                let name_clone = name.clone();
                if output.status.success() {
                    println!("{name} exited successfully");
                } else {
                    thread::spawn(move || {
                        thread::sleep(Duration::from_secs(1));
                        let state = &mut state_clone.lock().unwrap();
                        state.start_module(&name_clone);
                    });

                    let app = &*get_app_handle().lock().expect("failed to get app handle");

                    app.dialog()
                        .message(format!("{name} crashed. Restarting..."))
                        .kind(MessageDialogKind::Error)
                        .title("Warning")
                        .show(|_| {});

                    println!("{name} exited with error");
                    println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
                    println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
                }
            }
            ModuleMessage::Init {} => state.update_tray_menu(),
        }
    }
}

fn start_module_thread(name: String, tx: Sender<ModuleMessage>) {
    thread::spawn(move || {
        // Start the child process
        let port_string = get_config().port.to_string();
        let args = ["--port", port_string.as_str()];
        let child = Command::new(&name)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .spawn();

        if let Err(e) = child {
            println!("Failed to start {name}: {e}");
            return;
        }

        // Send a message to the manager that the module has started
        tx.send(ModuleMessage::Started {
            name: name.to_string(),
            pid: child.as_ref().unwrap().id(),
        })
        .unwrap();

        // Wait for the child to exit
        let output = child
            .unwrap()
            .wait_with_output()
            .expect("failed to wait on child");

        // Send the process output to the manager
        tx.send(ModuleMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .unwrap();
    });
}

#[cfg(unix)]
fn get_modules_in_path() -> BTreeSet<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();

    if let Some(paths) = env::var_os("PATH") {
        for path in env::split_paths(&paths) {
            if let Ok(entries) = fs::read_dir(&path) {
                for entry in entries.flatten() {
                    if let Ok(metadata) = entry.metadata() {
                        if (metadata.is_file() || metadata.is_symlink())
                            && metadata.permissions().mode() & 0o111 != 0
                        {
                            if let Some(file_name) = entry.file_name().to_str() {
                                if file_name.starts_with("aw") && !file_name.contains(".") {
                                    // starts with aw and doesn't have an extension
                                    set.insert(file_name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    set.remove("awk"); // common in most unix systems
    set.remove("aw-tauri");
    set.remove("aw-client");
    set.remove("aw-cli");

    set
}

#[cfg(windows)]
fn get_modules_in_path() -> BTreeSet<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();

    if let Some(paths) = env::var_os("PATH") {
        for path in env::split_paths(&paths) {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.filter_map(Result::ok) {
                    let path = entry.path();
                    if path.is_file() && path.extension().map_or(false, |ext| ext == "exe") {
                        set.insert(path.file_stem().unwrap().to_str().unwrap().to_string());
                    }
                }
            }
        }
    }
    set.remove("aw-tauri");
    set.remove("aw-client");
    set.remove("aw-cli");

    set
}
