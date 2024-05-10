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

#[cfg(target_os = "windows")]
use winapi::um::processthreadsapi::{OpenProcess, TerminateProcess};
#[cfg(target_os = "windows")]
use winapi::um::winnt::PROCESS_TERMINATE;

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
    }
    fn stopped_watcher(&mut self, name: &str) {
        println!("stopped {name}");
        self.watchers_running.insert(name.to_string(), false);
        self.watchers_pid.remove(name);
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
    let process_handle = unsafe { OpenProcess(PROCESS_TERMINATE, false, child_pid) };

    if process_handle == null_mut() {
        println!(
            "Failed to open process handle. Error: {}",
            std::io::Error::last_os_error()
        );
        return Err(std::io::Error::last_os_error());
    }

    // Terminate the process
    let result = unsafe { TerminateProcess(process_handle, 0) };

    if result == 0 {
        return Ok(());
    } else {
        return Err(std::io::Error::last_os_error());
    }
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
