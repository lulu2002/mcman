use std::{process::Stdio, time::Duration, path::PathBuf, sync::{Mutex, Arc}};

use anyhow::Result;
use console::style;
use notify_debouncer_mini::{new_debouncer, Debouncer, notify::{RecommendedWatcher, EventKind, RecursiveMode}};
use pathdiff::diff_paths;
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, sync::mpsc, task::JoinHandle, process::Child};

use crate::core::BuildContext;

use self::config::{HotReloadConfig, HotReloadAction};

pub mod config;
pub mod pattern_serde;

#[derive(Debug)]
pub struct DevSession<'a> {
    pub child: Option<tokio::process::Child>,
    pub command_sender: Option<mpsc::Sender<Command>>,
    pub command_reciever: Option<mpsc::Receiver<Command>>,
    pub builder: BuildContext<'a>,
    pub jar_name: Option<String>,
}

pub enum Command {
    Start,
    Stop,
    Rebuild,
    SendCommand(String),
    WaitUntilExit,
    Bootstrap(PathBuf),
}

pub enum State {
    Starting,
    Stopping,
    Building,
    Online,
}

async fn try_read_line(opt: &mut Option<tokio::io::Lines<BufReader<tokio::process::ChildStdout>>>) -> Result<Option<String>> {
    match opt {
        Some(lines) => Ok(lines.next_line().await?),
        None => Ok(None),
    }
}

// TODO
// [x] fix stdout nesting for some reason
// [x] commands are not being sent properly
// [x] use debouncer for notify
// [ ] reload server.toml properly
// [ ] tests 

impl<'a> DevSession<'a> {
    pub async fn spawn_child(&mut self) -> Result<Child> {
        let platform = if std::env::consts::FAMILY == "windows" {
            "windows"
        } else {
            "linux"
        };

        Ok(
            tokio::process::Command::new("java")
            .args(
                self.builder.app.server
                    .launcher
                    .get_arguments(&self.builder.app.server.jar.get_startup_method(
                        &self.builder.app,
                        &self.jar_name.as_ref().unwrap().clone()
                    ).await?, platform),
            )
            .current_dir(&self.builder.output_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?
        )
    }

    async fn handle_commands(mut self, mut rx: mpsc::Receiver<Command>) -> Result<()> {
        let mp = self.builder.app.multi_progress.clone();

        let mut child: Option<Child> = None;
        //let mut child_stdout = None;

        let mut stdout_lines: Option<tokio::io::Lines<BufReader<tokio::process::ChildStdout>>> = None;

        let mut is_stopping = false;

        let mut stdin_lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

        loop {
            tokio::select! {
                Some(cmd) = rx.recv() => {
                    match cmd {
                        Command::Start => {
                            self.builder.app.info("Starting server process...")?;
                            if child.is_none() {
                                let mut spawned_child = self.spawn_child().await?;
                                stdout_lines = Some(tokio::io::BufReader::new(spawned_child.stdout.take().expect("stdout None")).lines());
                                child = Some(spawned_child);
                            }
                        }
                        Command::Stop => {
                            self.builder.app.info("Killing server process...")?;
                            if let Some(ref mut child) = &mut child {
                                child.kill().await?;
                            }
                            child = None;
                            stdout_lines = None;
                        }
                        Command::SendCommand(command) => {
                            self.builder.app.info(&format!("Sending command: {command}"))?;
                            if let Some(ref mut child) = &mut child {
                                if let Some(ref mut stdin) = &mut child.stdin {
                                    let _ = stdin.write_all(command.as_bytes()).await;
                                }
                            }
                        }
                        Command::WaitUntilExit => {
                            self.builder.app.info("Waiting for process exit...")?;
                            is_stopping = true;
                            if let Some(ref mut child) = &mut child {
                                let should_kill = tokio::select! {
                                    _ = async {
                                        loop {
                                            if let Ok(Some(line)) = try_read_line(&mut stdout_lines).await {
                                                mp.suspend(|| {
                                                    println!(
                                                        "{}{}",
                                                        style("| ").bold(),
                                                        line.trim()
                                                    )
                                                });
                                            }
                                        }
                                    } => false, // should be unreachable..?
                                    _ = child.wait() => false,
                                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                                        self.builder.app.info("Timeout reached, killing...")?;
                                        true
                                    },
                                    _ = tokio::signal::ctrl_c() => {
                                        self.builder.app.info("^C recieved, killing...")?;
                                        true
                                    },
                                };

                                if should_kill {
                                    child.kill().await?;
                                }
                            }
                            is_stopping = false;
                            child = None;
                            stdout_lines = None;
                            self.builder.app.info("Server process ended")?;
                        }
                        Command::Rebuild => {
                            self.builder.app.info("Building...")?;
                            self.jar_name = Some(self.builder.build_all().await?);
                        }
                        Command::Bootstrap(path) => {
                            let rel_path = diff_paths(&path, self.builder.app.server.path.join("config"))
                                .expect("Cannot diff paths");
                            self.builder.app.info(format!("Bootstrapping: {}", rel_path.to_string_lossy().trim()))?;
                            match self.builder.bootstrap_file(&rel_path, None).await {
                                Ok(_) => {},
                                Err(e) => self.builder.app.warn(format!("Error while bootstrapping:
                                - Path: {}
                                - Err: {e}", rel_path.to_string_lossy()))?,
                            }
                        }
                    }
                },
                Ok(Some(line)) = try_read_line(&mut stdout_lines) => {
                    let mut s = line.trim();

                    mp.suspend(|| {
                        println!(
                            "{}{s}",
                            style("| ").bold()
                        )
                    });
                },
                Ok(Some(line)) = stdin_lines.next_line() => {
                    let mut cmd = line.trim();

                    self.builder.app.info(&format!("Sending command: {cmd}"))?;
                    if let Some(ref mut child) = &mut child {
                        if let Some(ref mut stdin) = &mut child.stdin {
                            let _ = stdin.write_all(format!("{cmd}\n").as_bytes()).await;
                        }
                    }
                },
                _ = tokio::signal::ctrl_c() => {
                    if !is_stopping {
                        self.builder.app.info("Stopping development session...")?;
                        break;
                    }
                }
            }
        }

        if let Some(ref mut child) = &mut child {
            self.builder.app.info("Killing undead child process...")?;
            child.kill().await?;
        }

        Ok(())
    }


    pub fn create_hotreload_watcher(
        config: Arc<Mutex<HotReloadConfig>>,
        tx: mpsc::Sender<Command>,
    ) -> Result<Debouncer<RecommendedWatcher>> {
        Ok(new_debouncer(Duration::from_secs(1), move |e| {
            if let Ok(e) = e {
                for e in e {
                    if !matches!(e.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                        continue;
                    };
                    
                    let mut guard = config.lock().unwrap();

                    match HotReloadConfig::load_from(&guard.path) {
                        Ok(updated) => {
                            eprintln!("Updated hotreload.toml :3");
                            *guard = updated;
                        }
                        Err(e) => {
                            eprintln!("hotreload.toml error: {e}");
                            eprintln!("cannot update hotreload.toml");
                        }
                    }
                }
            }
        })?)
    }

    pub fn create_config_watcher(
        config: Arc<Mutex<HotReloadConfig>>,
        tx: mpsc::Sender<Command>,
    ) -> Result<Debouncer<RecommendedWatcher>> {
        Ok(new_debouncer(Duration::from_secs(1), move |e| {
            if let Ok(e) = e {
                for e in e {
                    if !matches!(e.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                        continue;
                    };

                    for path in e.paths {
                        if path.is_dir() {
                            return;
                        }

                        tx.blocking_send(Command::Bootstrap(path.clone())).unwrap();

                        let guard = config.lock().unwrap();
                        let Some(file) = guard.files.iter().find(|f| {
                            f.path.matches_path(&path)
                        }).cloned() else {
                            return;
                        };
                        drop(guard);

                        match &file.action {
                            HotReloadAction::Reload => {
                                tx.blocking_send(Command::SendCommand("reload confirm\n".to_owned()))
                                    .expect("tx send err");
                            }
                            HotReloadAction::Restart => {
                                tx.blocking_send(Command::SendCommand("stop\nend\n".to_owned()))
                                    .expect("tx send err");
                                tx.blocking_send(Command::WaitUntilExit)
                                    .expect("tx send err");
                                tx.blocking_send(Command::Start)
                                    .expect("tx send err");
                            }
                            HotReloadAction::RunCommand(cmd) => {
                                tx.blocking_send(Command::SendCommand(format!("{cmd}\n")))
                                    .expect("tx send err");
                            }
                        }
                    }
                }
            }
        })?)
    }

    pub fn create_servertoml_watcher(tx: mpsc::Sender<Command>) -> Result<Debouncer<RecommendedWatcher>> {
        Ok(new_debouncer(Duration::from_secs(1), move |e| {
            if let Ok(e) = e {
                for e in e {
                    if !matches!(e.kind, EventKind::Modify(_)) {
                        continue;
                    };
                    
                    tx.blocking_send(Command::SendCommand("stop\nend".to_owned()))
                        .expect("tx send err");
                    tx.blocking_send(Command::WaitUntilExit)
                        .expect("tx send err");
                    tx.blocking_send(Command::Rebuild)
                        .expect("tx send err");
                    tx.blocking_send(Command::Start)
                        .expect("tx send err");
                }
            }
        })?)
    }

    pub async fn start(mut self, config: HotReloadConfig) -> Result<()> {
        let (tx, rx) = mpsc::channel(32);

        let cfg_mutex = Arc::new(Mutex::new(config));

        let mut config_watcher = Self::create_config_watcher(cfg_mutex.clone(), tx.clone())?;
        let mut hotreload_watcher = Self::create_hotreload_watcher(cfg_mutex.clone(), tx.clone())?;
        let mut servertoml_watcher = Self::create_servertoml_watcher(tx.clone())?;

        config_watcher.watcher().watch(self.builder.app.server.path.join("config").as_path(), RecursiveMode::Recursive)?;
        servertoml_watcher.watcher().watch(self.builder.app.server.path.join("server.toml").as_path(), RecursiveMode::NonRecursive)?;
        hotreload_watcher.watcher().watch(self.builder.app.server.path.join("hotreload.toml").as_path(), RecursiveMode::NonRecursive)?;

        tx.send(Command::Rebuild).await?;
        tx.send(Command::Start).await?;

        self.handle_commands(rx).await?;

        Ok(())
    }
}
