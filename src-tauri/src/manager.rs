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
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

#[derive(Debug)]
pub enum WatcherMessage {
    Started {
        name: String,
    },
    Stopped {
        name: String,
        output: std::process::Output,
    },
}

struct ManagerState {
    watchers_running: HashMap<String, bool>,
}

impl ManagerState {
    fn new() -> ManagerState {
        ManagerState {
            watchers_running: HashMap::new(),
        }
    }
    fn started_watcher(&mut self, name: &str) {
        println!("started {name}");
        self.watchers_running.insert(name.to_string(), true);
    }
    fn stopped_watcher(&mut self, name: &str) {
        println!("stopped {name}");
        self.watchers_running.insert(name.to_string(), false);
    }
    fn is_watcher_running(&self, name: &str) -> bool {
        *self.watchers_running.get(name).unwrap_or(&false)
    }
}

pub fn start_manager() -> Sender<WatcherMessage> {
    let (tx, rx) = channel();

    // Start the watchers
    let autostart_watchers = ["aw-watcher-afk", "aw-watcher-window"];
    for watcher in autostart_watchers.iter() {
        start_watcher(watcher, tx.clone());
    }

    thread::spawn(move || {
        handle(rx);
    });
    tx
}

fn handle(rx: Receiver<WatcherMessage>) {
    let mut state = ManagerState::new();
    loop {
        let msg = rx.recv().unwrap();
        match msg {
            WatcherMessage::Started { name } => {
                state.started_watcher(&name);
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
        let child = Command::new([name, "--testing"].join(" "))
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to execute child");

        // Send a message to the manager that the watcher has started
        tx.send(WatcherMessage::Started {
            name: name.to_string(),
        })
        .unwrap();

        // Wait for the child to exit
        let output = child.wait_with_output().expect("failed to wait on child");

        // Send the process output to the manager
        tx.send(WatcherMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .unwrap();
    });
}
