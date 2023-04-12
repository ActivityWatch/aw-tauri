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
use std::time::Duration;

use aw_models::Event;

#[derive(Debug)]
enum WatcherMessage {
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
        println!("{} started", name);
        self.watchers_running.insert(name.to_string(), true);
    }
    fn stopped_watcher(&mut self, name: &str) {
        println!("{} stopped", name);
        self.watchers_running.insert(name.to_string(), false);
    }
    fn is_watcher_running(&self, name: &str) -> bool {
        self.watchers_running.get(name).unwrap_or(&false)
    }
}

fn main() {
    let (tx, rx) = channel();
    let state = ManagerState::new();

    // Start the watchers
    start_watcher("aw-watcher-afk", tx.clone());
    start_watcher("aw-watcher-window", tx.clone());

    // Start the manager
    loop {
        let msg = rx.recv().unwrap();
        match msg {
            WatcherMessage::Started { name } => {
                state.started_watcher(&name);
            }
            WatcherMessage::Stopped { name, output } => {
                state.stopped_watcher(&name);
                if output.status.success() {
                    println!("{} exited successfully", name);
                } else {
                    println!("{} exited with error", name);
                    println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
                    println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
                }
            }
        }
    }
}

fn start_watcher(name: &str, tx: Sender<WatcherMessage>) {
    let tx = tx.clone();
    thread::spawn(move || {
        // Start the child process
        let mut child = Command::new(name)
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
            output: output,
        })
        .unwrap();
    });
}
