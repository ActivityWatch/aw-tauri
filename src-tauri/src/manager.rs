#[cfg(unix)]
use nix::sys::signal::{self, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
/// A process manager for ActivityWatch
///
/// Used to start, stop and manage the lifecycle watchers like aw-watcher-afk and aw-watcher-window.
/// A watcher is a process that runs in the background and sends events to the ActivityWatch server.
///
/// The manager is responsible for starting and stopping the watchers, and for keeping track of
/// their state.
///
/// If a watcher crashes, the manager will notify the user and ask if they want to restart it.
use std::collections::HashMap;
use std::process::Command;
use std::sync::{
    mpsc::{channel, Receiver, Sender},
    Arc, Mutex,
};
use std::thread;
use tauri::{CustomMenuItem, SystemTrayMenu, SystemTrayMenuItem, SystemTraySubmenu};
#[cfg(windows)]
use winapi::shared::minwindef::DWORD;
#[cfg(windows)]
use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};

use crate::{get_app_handle, SHARED_CONDVAR};

#[derive(Debug)]
pub enum WatcherMessage {
    Started {
        name: String,
        pid: u32,
    },
    Stopped {
        name: String,
        output: std::process::Output,
    },
}

#[derive(Debug)]
pub struct ManagerState {
    pub watchers_running: HashMap<String, bool>,
    pub watchers_pid: HashMap<String, u32>,
}

impl ManagerState {
    fn new() -> ManagerState {
        ManagerState {
            watchers_running: HashMap::new(),
            watchers_pid: HashMap::new(),
        }
    }
    fn started_watcher(&mut self, name: &str, pid: u32) {
        println!("started {name}");
        self.watchers_running.insert(name.to_string(), true);
        self.watchers_pid.insert(name.to_string(), pid);
        println!("{:?}", self.watchers_running);
        self.update_tray_menu();
    }
    fn stopped_watcher(&mut self, name: &str) {
        println!("stopped {name}");
        self.watchers_running.insert(name.to_string(), false);
        self.watchers_pid.remove(name);
        self.update_tray_menu();
    }
    fn update_tray_menu(&mut self) {
        let open = CustomMenuItem::new("open".to_string(), "Open");
        let quit = CustomMenuItem::new("quit".to_string(), "Quit");
        let mut module_menu = SystemTrayMenu::new();

        for (module, running) in self.watchers_running.iter() {
            let label = format!(
                "{} ({})",
                module,
                if *running { "Running" } else { "Stopped" }
            );
            module_menu = module_menu.add_item(CustomMenuItem::new(module.clone(), &label));
        }

        let module_submenu = SystemTraySubmenu::new("Modules", module_menu);
        let menu = SystemTrayMenu::new()
            .add_item(open)
            .add_native_item(SystemTrayMenuItem::Separator)
            .add_submenu(module_submenu)
            .add_native_item(SystemTrayMenuItem::Separator)
            .add_item(quit);

        let (lock, cvar) = &*SHARED_CONDVAR;
        let mut state = lock.lock().unwrap();

        while !*state {
            state = cvar.wait(state).unwrap();
        }

        let app = get_app_handle().lock().expect("failed to get app handle");
        let tray_handle = app.tray_handle();
        tray_handle.set_menu(menu).expect("failed to set tray menu");
    }
    pub fn stop_watchers(&mut self) {
        for (name, pid) in self.watchers_pid.iter() {
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
    fn is_watcher_running(&self, name: &str) -> bool {
        *self.watchers_running.get(name).unwrap_or(&false)
    }
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<(), nix::Error> {
    let pid = Pid::from_raw(pid as i32);
    signal::kill(pid, Signal::SIGTERM).unwrap();
    Ok(())
}

#[cfg(windows)]
fn send_sigterm(pid: u32) -> Result<(), std::io::Error> {
    // Get the process ID of the child process
    let pid = pid as DWORD;

    // Send SIGTERM signal to the process
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
        println!("Failed to send SIGTERM signal to the process");
        return Err(std::io::Error::last_os_error());
    } else {
        println!("SIGTERM signal sent successfully to the process");
        return Ok(());
    }
    Ok(())
}
pub fn start_manager() -> (Sender<WatcherMessage>, Arc<Mutex<ManagerState>>) {
    let (tx, rx) = channel();
    let state = Arc::new(Mutex::new(ManagerState::new()));

    // Start the watchers
    let autostart_watchers = ["aw-watcher-afk", "aw-watcher-window"];
    for watcher in autostart_watchers.iter() {
        start_watcher(watcher, tx.clone());
    }

    let state_clone = Arc::clone(&state);
    thread::spawn(move || {
        handle(rx, state_clone);
    });
    (tx, state)
}

fn handle(rx: Receiver<WatcherMessage>, state: Arc<Mutex<ManagerState>>) {
    loop {
        let msg = rx.recv().unwrap();
        let state = &mut state.lock().unwrap();
        match msg {
            WatcherMessage::Started { name, pid } => {
                state.started_watcher(&name, pid);
            }
            WatcherMessage::Stopped { name, output } => {
                state.stopped_watcher(&name);
                if output.status.success() {
                    println!("{name} exited successfully");
                } else {
                    println!("{name} exited with error");
                    println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
                    println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
                }
            }
        }
    }
}

fn start_watcher(name: &'static str, tx: Sender<WatcherMessage>) {
    thread::spawn(move || {
        // Start the child process
        let path = name;
        let args = ["--testing", "--port", "5699"];
        let child = Command::new(path)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .spawn();

        if let Err(e) = child {
            println!("Failed to start {name}: {e}");
            return;
        }

        // Send a message to the manager that the watcher has started
        tx.send(WatcherMessage::Started {
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
        tx.send(WatcherMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .unwrap();
    });
}
