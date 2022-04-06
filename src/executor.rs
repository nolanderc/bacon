use {
    crate::*,
    anyhow::Result,
    crossbeam::channel::{bounded, select, unbounded, Receiver, SendError, Sender},
    std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
        sync::{Arc, Mutex},
        thread::{self, JoinHandle},
        time::Duration,
    },
};

/// an executor calling a cargo (or similar) command in a separate
/// thread when asked to and sending the lines of output in a channel,
/// and finishing by None.
/// Channel sizes are designed to avoid useless computations.
pub struct Executor {
    pub line_receiver: Receiver<CommandExecInfo>,
    task_sender: Sender<Task>,
    stop_sender: Sender<()>, // signal for stopping the thread
    thread: thread::JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Task {
    pub backtrace: bool,
}

impl Executor {
    /// launch the commands, send the lines of its stderr on the
    /// line channel.
    /// If `with_stdout` capture and send also its stdout.
    pub fn new(mission: &Mission) -> Result<Self> {
        let mut command = mission.get_command();
        let with_stdout = mission.need_stdout();
        let (task_sender, task_receiver) = bounded::<Task>(1);
        let (stop_sender, stop_receiver) = bounded(0);
        let (line_sender, line_receiver) = unbounded();
        let (error_sender, error_receiver) = bounded(0);
        command
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(if with_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            });

        let thread = thread::spawn(move || {
            // Handle to the currently running task.
            let mut task_handle: Option<TaskHandle> = None;

            loop {
                select! {
                    recv(task_receiver) -> task => {
                        if let Some(old_task) = task_handle.take() {
                            if let Err(e) = old_task.kill() {
                                warn!("failed to kill running task: {}", e);
                                break
                            }
                        }

                        let task = match task {
                            Ok(task) => task,
                            _ => { break; }
                        };

                        match spawn_task(task, &mut command, with_stdout, line_sender.clone(), error_sender.clone()) {
                            Ok(handle) => task_handle = Some(handle),
                            Err(TaskError::ChannelClosed) => break,
                            Err(_) => continue,
                        }
                    }
                    recv(stop_receiver) -> _ => {
                        debug!("stopping executor");
                        if let Some(old_task) = task_handle.take() {
                            if let Err(e) = old_task.kill() {
                                warn!("failed to kill running task: {}", e);
                            }
                        }
                        break;
                    }
                    recv(error_receiver) -> error => {
                        match error {
                            Err(_) => continue,
                            Ok(TaskError::ChannelClosed) => break,
                            Ok(e) => warn!("task failed: {:?}", e),
                        }

                        if let Some(old_task) = task_handle.take() {
                            if let Err(e) = old_task.kill() {
                                warn!("failed to kill running task: {}", e);
                                break
                            }
                        }
                    }
                }
            }

            debug!("executor terminated");
        });
        Ok(Self {
            line_receiver,
            task_sender,
            stop_sender,
            thread,
        })
    }
    /// notify the executor a computation is necessary
    pub fn start(&self, task: Task) -> Result<()> {
        self.task_sender.try_send(task)?;
        Ok(())
    }
    pub fn die(self) -> Result<()> {
        debug!("received kill order");
        self.stop_sender.send(()).unwrap();
        debug!("waiting on executor thread");
        self.thread.join().unwrap();
        debug!("executor exited");
        Ok(())
    }
}

/// Handle to a single task
struct TaskHandle {
    child: Arc<Mutex<std::process::Child>>,
    task_thread: JoinHandle<()>,
}

impl TaskHandle {
    /// Kill the running task
    fn kill(self) -> Result<()> {
        debug!("stopping executor task");
        self.child.lock().unwrap().kill()?;
        debug!("waiting on task thread");
        // self.task_thread.join().unwrap();
        Ok(())
    }
}

#[derive(Debug)]
enum TaskError {
    /// Failed to send on a channel since it was closed
    ChannelClosed,
    /// There was an error running the command
    BadCommand,
    /// The spawned process did not have a handle to stderr.
    MissingStdErr,
    /// The spawned process did not have a handle to stdout.
    MissingStdOut,
}

impl<T> From<SendError<T>> for TaskError {
    fn from(_: SendError<T>) -> Self {
        TaskError::ChannelClosed
    }
}

fn spawn_task(
    task: Task,
    command: &mut Command,
    with_stdout: bool,
    line_sender: Sender<CommandExecInfo>,
    error_sender: Sender<TaskError>,
) -> Result<TaskHandle, TaskError> {
    debug!("starting task {:?}", task);
    command.env("RUST_BACKTRACE", if task.backtrace { "1" } else { "0" });
    let child = command.spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            line_sender.send(CommandExecInfo::Error(format!(
                "command launch failed: {}",
                e
            )))?;
            return Err(TaskError::BadCommand);
        }
    };

    // get rid of stdin so that the child doesn't accidentally block on it
    drop(child.stdin.take());

    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            line_sender.send(CommandExecInfo::Error("taking stderr failed".to_string()))?;
            return Err(TaskError::MissingStdErr);
        }
    };

    let stderr_sender = line_sender.clone();
    let stderr_thread =
        std::thread::spawn(move || consume_stream(stderr, CommandStream::StdErr, stderr_sender));

    // (if somebody knows of an efficient and clean cross-platform way
    // to listen for both stderr and stdout on the same thread, please tell me)
    let stdout_thread = if with_stdout {
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                line_sender.send(CommandExecInfo::Error("taking stdout failed".to_string()))?;
                return Err(TaskError::MissingStdOut);
            }
        };
        let stdout_sender = line_sender.clone();
        Some(std::thread::spawn(move || {
            consume_stream(stdout, CommandStream::StdOut, stdout_sender)
        }))
    } else {
        None
    };

    // Wrap the child in a mutex to allow the executor to kill it
    let child = Arc::new(Mutex::new(child));

    // Spawn another thread to coordinate the execution (when both stdout and stderr are empty, we
    // need to notify the receiver of `line_sender`)
    let task_thread = {
        let child = child.clone();
        std::thread::spawn(move || {
            let wait_on_command = || -> Result<(), TaskError> {
                // make sure we join both threads
                let stderr_result = stderr_thread.join();
                let stdout_result = match stdout_thread {
                    None => Ok(Ok(())),
                    Some(thread) => thread.join(),
                };

                // if any thread panicked, we also panic by unwrapping
                let stderr_result = stderr_result.unwrap();
                let stdout_result = stdout_result.unwrap();

                // if any thread returned an error, we also return it.
                let () = stderr_result?;
                let () = stdout_result?;

                // Wait for the child to terminate
                debug!("waiting on task command");
                let status = loop {
                    // move lock out of match to avoid deadlock
                    // (https://fasterthanli.me/articles/a-rust-match-made-in-hell)
                    let wait_result = child.lock().unwrap().try_wait();
                    match wait_result {
                        Err(e) => {
                            warn!("error in child: {:?}", e);
                            break None;
                        }
                        Ok(Some(status)) => {
                            debug!("exit_status: {:?}", status);
                            break Some(status);
                        }
                        Ok(None) => {
                            // status is not ready yet, wait a bit and try again
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                };

                line_sender.send(CommandExecInfo::End { status })?;

                Ok(())
            };

            if let Err(e) = wait_on_command() {
                error_sender.send(e).expect("executor closed before task");
            }

            debug!("finished command execution");
        })
    };

    Ok(TaskHandle { child, task_thread })
}

fn consume_stream(
    stream: impl std::io::Read,
    origin: CommandStream,
    line_sender: Sender<CommandExecInfo>,
) -> Result<(), TaskError> {
    let mut buf = String::new();
    let mut buf_reader = BufReader::new(stream);
    loop {
        let line = match buf_reader.read_line(&mut buf) {
            Err(e) => CommandExecInfo::Error(e.to_string()),
            // finished
            Ok(0) => return Ok(()),

            Ok(_) => CommandExecInfo::Line(CommandOutputLine {
                origin,
                content: TLine::from_tty(buf.trim_end()),
            }),
        };
        if let Err(e) = line_sender.send(line) {
            debug!("error when sending stdout line: {}", e);
            return Ok(());
        }
        buf.clear();
    }
}
