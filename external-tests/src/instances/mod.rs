use colored::Colorize;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::str;
use std::string::ToString;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use json::{object, JsonValue};
use log::{error, info};
use tokio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::spawn;
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;

pub struct Qemu {
    binary: String,
    args: Vec<String>,
    running: AtomicBool,
    process: Option<Arc<Mutex<Child>>>,
    vm_tx: Option<Sender<u8>>,
    vm_rx: Option<Arc<Mutex<Receiver<u8>>>>,
}

/// `Qemu` is an abstraction to start a QEMU instance with custom args
impl Qemu {
    pub fn new(binary: &str, args: Vec<String>) -> Qemu {
        Self {
            binary: binary.to_string(),
            args,
            running: AtomicBool::new(false),
            process: None,
            vm_tx: None,
            vm_rx: None,
        }
    }

    /// Once the instance launched by calling [`Self::run`], `watch` method will
    /// listen to a [`Receiver`] and asynchronously wait for instructions to come.
    fn watch(&self) -> Result<JoinHandle<()>, String> {
        let rx = self.vm_rx.as_ref().unwrap().clone();
        let process = self.process.as_ref().unwrap().clone();
        if self.process.is_some() {
            Ok(spawn(async move {
                while let Some(i) = rx.lock().await.recv().await {
                    // Code 0 = kill instance
                    if i == 0 {
                        info!("Received kill instruction");
                        info!("Killing instance...");
                        process
                            .lock()
                            .await
                            .kill()
                            .await
                            .expect("Could not kill instance.");
                        break;
                    }
                }
            }))
        } else {
            Err("No instance running".to_string())
        }
    }

    /// Asynchronously sends a message to the running instance.
    pub async fn send_instance(&self, message: u8) -> Result<(), String> {
        let vm_tx = self
            .vm_tx
            .as_ref()
            .ok_or("Cant connect to instance".to_string())?;
        vm_tx
            .send(message)
            .await
            .map_err(|_| "Cant send action to instance".to_string())?;
        Ok(())
    }

    /// Stops the running instance.
    pub async fn stop(&self) -> Result<(), String> {
        self.send_instance(0).await
    }

    /// Launch the instance and opens a QMP socket in order to send
    /// future command to the running QEMU instance by attaching a [`QMP`] to it.
    pub fn run(&mut self) -> Result<JoinHandle<()>, String> {
        let output = Command::new(self.binary.clone())
            .arg("-qmp")
            .arg("unix:external-tests.sock,server,nowait")
            .arg("-d")
            .arg("int")
            .arg("-D")
            .arg("log")
            .args(self.args.clone())
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let process = output.map_err(|e| format!("Can't start QEMU for some reason : {}", e))?;
        self.running = AtomicBool::new(true);
        self.process = Some(Arc::new(Mutex::new(process)));
        let (tx, rx) = channel::<u8>(10);
        self.vm_tx = Some(tx);
        self.vm_rx = Some(Arc::new(Mutex::new(rx)));
        self.watch()
    }
}

/// QMP stands for QEMU Machine Protocol.
/// It is a protocol that provides a kind of API for QEMU instances.
///
/// `QMP` implementation will allow connecting asynchronously to a QMP socket and then
/// discuss with it when needed.
pub struct QMP {
    path: PathBuf,
    socket: Option<Arc<Mutex<UnixStream>>>,
    qmp_query_rx: Option<Arc<Mutex<Receiver<String>>>>,
    qmp_query_tx: Option<Sender<String>>,
    qmp_response_rx: Option<Receiver<String>>,
    qmp_response_tx: Option<Arc<Sender<String>>>,
}

impl QMP {
    /// Creates a `QMP` instance given a path to a QMP socket.
    pub fn new(path: &str) -> Self {
        Self {
            path: PathBuf::from(path.to_string()),
            socket: None,
            qmp_query_rx: None,
            qmp_query_tx: None,
            qmp_response_tx: None,
            qmp_response_rx: None,
        }
    }

    /// Connects to an existing QMP socket
    pub async fn attach(&mut self) -> Result<JoinHandle<io::Result<()>>, String> {
        let socket = UnixStream::connect(&self.path)
            .await
            .map_err(|_| "Cant connect to QMP socket".to_string())?;
        self.socket = Some(Arc::new(Mutex::new(socket)));
        info!(
            "Connected to socket located at : {} ",
            self.path.to_str().unwrap()
        );
        let (restx, resrx) = channel::<String>(10);
        let (querytx, queryrx) = channel::<String>(10);
        self.qmp_query_tx = Some(querytx);
        self.qmp_response_tx = Some(Arc::new(restx));
        self.qmp_query_rx = Some(Arc::new(Mutex::new(queryrx)));
        self.qmp_response_rx = Some(resrx);
        Ok(self.watch())
    }

    /// Given a [`Queriable`], send it and wait for a response.
    pub async fn query_and_wait<T: Queriable>(&mut self, query: T) -> Result<String, String> {
        let query_tx = self
            .qmp_query_tx
            .as_ref()
            .ok_or_else(|| "No connection to running QMP".to_string())?;
        let resp_rx = self
            .qmp_response_rx
            .as_mut()
            .ok_or_else(|| "No connection to running QMP".to_string())?;
        query_tx.send(query.compute()).await.unwrap();
        if let Some(data) = resp_rx.recv().await {
            return Ok(data.clone());
        }
        Err("Unknown error".to_string())
    }

    ///
    fn watch(&self) -> JoinHandle<io::Result<()>> {
        let socket = self.socket.as_ref().unwrap().clone();
        let query_rx = self.qmp_query_rx.as_ref().unwrap().clone();
        let resp_tx = self.qmp_response_tx.as_ref().unwrap().clone();
        spawn(async move {
            socket.lock().await.readable().await?;
            info!("[QMP] Stream is ready");
            Self::read_all(socket.clone()).await;
            while let Some(data) = query_rx.lock().await.recv().await {
                info!("[QMP] Received query : {} ", data);
                let _ = socket.lock().await.write(data.as_bytes()).await.unwrap();
                info!("[QMP] Command sent to QEMU instance");
                let value = Self::read_all(socket.clone()).await;
                resp_tx.send(value).await.unwrap();
            }
            Ok(())
        })
    }

    /// Given a [`UnixStream`] enclosed in an Arc Mutex, read until \\n and returns a [`String`]
    /// containing data.
    async fn read_all(socket: Arc<Mutex<UnixStream>>) -> String {
        let mut string = String::new();
        let mut msg = [0u8; 30];
        loop {
            let n = socket.lock().await.read(&mut msg).await.unwrap();
            string.push_str(str::from_utf8(&msg[0..n]).unwrap());
            if string.ends_with('\n') {
                let _ = string.trim_end();
                return string;
            }
        }
    }
}

#[derive(Clone)]
pub struct Query {
    query_type: QueryType,
    command: String,
    args: Vec<(String, JsonValue)>,
}

impl Query {
    pub fn new(query_type: QueryType, command: &str) -> Self {
        Self {
            query_type,
            command: command.to_string(),
            args: vec![],
        }
    }

    pub fn arg<T: Into<JsonValue>>(mut self, arg: &str, value: T) -> Self {
        self.args.push((arg.to_string(), value.into()));
        self
    }
}

pub trait Queriable {
    fn compute(&self) -> String;
}

impl Queriable for Query {
    fn compute(&self) -> String {
        match self.query_type {
            QueryType::COMMAND => {
                let mut args = JsonValue::new_object();
                for v in &self.args {
                    args[v.0.as_str()] = v.1.clone()
                }
                let query = object! {
                    execute : self.command.clone(),
                    arguments : args
                };
                query.to_string()
            }
        }
    }
}

pub enum ActionBasics {
    NULL,
}

pub enum ActionResultBasics {
    NULL,
}

pub struct Debugger {
    binary: String,
    args: Vec<String>,
    process: Option<Arc<Mutex<Child>>>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    stdout: Option<Arc<Mutex<ChildStdout>>>,
    cmd_tx: Option<Sender<String>>,
    cmd_rx: Option<Arc<Mutex<Receiver<String>>>>,
    res_tx: Option<Arc<Sender<String>>>,
    res_rx: Option<Receiver<String>>,
    breakpoints: Vec<String>,
    vm: Option<Qemu>,
    init: Vec<String>,
    tests: Vec<String>,
    panic_ref: String,
    end_ref: String,
}

impl Debugger {
    pub fn new(binary: &str, args: Vec<String>) -> Debugger {
        Self {
            binary: binary.to_string(),
            args,
            process: None,
            stdin: None,
            stdout: None,
            cmd_tx: None,
            cmd_rx: None,
            res_tx: None,
            res_rx: None,
            breakpoints: vec![],
            vm: None,
            init: vec![],
            tests: vec![],
            panic_ref: String::new(),
            end_ref: String::new(),
        }
    }

    pub async fn send(&self, cmd: &str) -> Result<(), String> {
        let sender = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| "Cant get sender".to_string())?
            .clone();
        sender
            .send(cmd.to_string())
            .await
            .map_err(|_| "No response from watcher".to_string())?;
        Ok(())
    }

    /// Registers the gateway (ie the point reached if the last test is successful)
    pub fn register_gateway(&mut self, end_ref: &str) {
        self.end_ref = end_ref.to_string();
    }

    /// Registers a test given its symbol
    pub fn register_test(&mut self, test_ref: &str) {
        self.tests.push(test_ref.to_string());
    }

    /// Allows to specify a custom breakpoint name for the panic handler
    pub fn register_panic(&mut self, panic_ref: &str) {
        self.panic_ref = panic_ref.to_string();
    }

    /// Runs every steps needed to perform registered tests.
    /// This includes running init commands.
    ///
    /// This method will return either when a test has a fatal outcome or when all test have run
    /// successfully.
    /// It will return a [`Recap`] enclosed in a [`Result`] in case an error occurs.
    pub async fn test(&mut self) -> Result<Result<Recap, Recap>, String> {
        let tests = self.tests.clone();
        for test in tests {
            self.breakpoint(test.as_str());
        }
        let mut successful = Vec::new();
        self.breakpoint(self.panic_ref.clone().as_str());
        self.breakpoint(self.end_ref.clone().as_str());
        let mut recap = Recap::new();
        let mut last = String::new();
        self.init().await.unwrap();
        while let Ok(bp) = self.continue_until().await {
            if bp == self.panic_ref {
                if last.is_empty() {
                    return Err("Panic occurred before any test".to_string());
                }
                error!("{} {} {}", "Test".red(), last.red(), "panicked !".red());
                recap.push(0, ActionResult::FATAL(last.clone()));
                return Ok(Err(recap));
            }
            if bp == self.end_ref {
                info!(
                    "{} {} {}",
                    "Test".green(),
                    last.green(),
                    "exited successfully".green()
                );
                info!("Reached end");
                recap.push(0, ActionResult::SUCCESS(last.clone()));
                successful.push(last);
                return if self.tests.iter().all(|test| successful.contains(test)) {
                    info!("{}", "Every test were successful".green());
                    Ok(Ok(recap))
                } else {
                    error!("{}", "Some test never ran".red());
                    Ok(Err(recap))
                };
            } else if !last.is_empty() {
                info!(
                    "{} {} {}",
                    "Test".green(),
                    last.green(),
                    "exited successfully".green()
                );
                recap.push(0, ActionResult::SUCCESS(last.clone()));
                successful.push(last);
            }

            info!("Reached test {}", bp);
            info!("Running test {}", bp);
            last = bp;
        }
        Ok(Ok(recap))
    }

    /// Sends a command to GDB and hangs until something occurs
    pub async fn send_and_wait(&self, cmd: &str) -> Result<String, String> {
        self.send(cmd).await?;
        let res = Self::read_all(self.stdout.as_ref().unwrap().clone(), "(gdb)").await;
        if res.contains("^error") {
            return Err(res);
        }
        Ok(res)
    }

    /// Appends a GDB command to the init ones.
    /// This method doesn't actually run the command.
    /// To do so, consider calling [`Self::init_commands`].
    pub fn before(&mut self, command: &str) {
        self.init.push(command.to_string());
    }

    pub async fn init(&mut self) -> Result<(), String> {
        let args = vec![
            "-s".to_string(),
            "-S".to_string(),
            "/Users/foxy/Documents/OS/bootloader/build/boot.img".to_string(),
        ];
        let qemu = Qemu::new("qemu-system-x86_64", args);
        self.vm = Some(qemu);
        self.vm
            .as_mut()
            .unwrap()
            .run()
            .expect("Can't start QEMU instance");
        let mut command = Command::new(self.binary.clone())
            .arg("-interpreter")
            .arg("mi")
            .args(&self.args)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|_| "Cant start debugger".to_string())?;

        let (restx, resrx) = channel::<String>(10);
        let (querytx, queryrx) = channel::<String>(10);

        self.cmd_tx = Some(querytx);
        self.res_tx = Some(Arc::new(restx));
        self.cmd_rx = Some(Arc::new(Mutex::new(queryrx)));
        self.res_rx = Some(resrx);

        let stdin = command.stdin.take().unwrap();
        self.stdin = Some(Arc::new(Mutex::new(stdin)));

        let stdout = command.stdout.take().unwrap();
        self.stdout = Some(Arc::new(Mutex::new(stdout)));

        self.process = Some(Arc::new(Mutex::new(command)));

        self.watch();

        sleep(Duration::from_millis(200)).await;

        self.init_commands().await?;
        self.set_breakpoints().await?;
        info!("Initiated");
        Ok(())
    }

    /// Continue execution by running _continue_ command in GDB.
    /// Returns the name of the breakpoint reached if execution is interrupted.
    /// This could never return if the previous condition never meets.
    pub async fn continue_until(&mut self) -> Result<String, String> {
        self.send_and_wait("c").await?;
        let res = Self::read_all(self.stdout.as_mut().unwrap().clone(), "(gdb)").await;
        let nb = Self::get_attribute(res, "bkptno").unwrap();
        let nb: usize = nb.parse().unwrap();
        let breakpoint = self
            .breakpoints
            .get(nb - 1)
            .ok_or_else(|| "Unknown breakpoint".to_string())?;
        Ok(breakpoint.clone())
    }

    /// Runs every init command set up using [`Self::before`].
    pub async fn init_commands(&self) -> Result<(), String> {
        for init in &self.init {
            println!("{}", init);
            self.send_and_wait(init).await?;
        }
        Ok(())
    }

    pub fn watch(&self) {
        let cmd_receiver = self.cmd_rx.as_ref().unwrap().clone();
        let process = self.process.as_ref().unwrap().clone();
        let stdin = self.stdin.as_ref().unwrap().clone();
        let stdout = self.stdout.as_ref().unwrap().clone();

        spawn(async move {
            let _init = Self::read_all(stdout.clone(), "(gdb)").await;
            while let Some(mut cmd) = cmd_receiver.lock().await.recv().await {
                if cmd == *"stop" {
                    info!("Stopping");
                    process
                        .lock()
                        .await
                        .kill()
                        .await
                        .expect("Can't stop processs");
                } else {
                    cmd.push_str("\n");
                    let _n = stdin.lock().await.write(cmd.as_bytes()).await.unwrap();
                }
            }
        });
    }

    /// Appends a breakpoint to the breakpoint list.
    /// This method doesn't actually set the breakpoint in GDB. To do so, consider
    /// using [`Self::set_breakpoints`].
    pub fn breakpoint(&mut self, b: &str) {
        self.breakpoints.push(b.to_string());
    }

    /// Set every breakpoints by sending _breakpoint_ command to GDB/MI
    pub async fn set_breakpoints(&mut self) -> Result<(), String> {
        for breakpoint in &self.breakpoints {
            let mut command = "b ".to_string();
            command.push_str(breakpoint);
            let a = self.send_and_wait(command.as_str()).await?;
            info!("Set breakpoint {}", breakpoint);
            if a.contains("error") {
                return Err(format!("Could not set breakpoint {}", breakpoint));
            };
        }
        Ok(())
    }

    /// Given a [`ChildStdout`] enclosed in an Arc Mutex, read until a given pattern and returns a [`String`]
    /// containing the data.
    async fn read_all(stdout: Arc<Mutex<ChildStdout>>, until: &str) -> String {
        let mut string = String::new();
        let mut msg = [0u8; 30];
        loop {
            let n = stdout.lock().await.read(&mut msg).await.unwrap();
            string.push_str(str::from_utf8(&msg[0..n]).unwrap());
            msg = [0u8; 30];
            if string.contains(until) {
                let _ = string.trim_end();
                return string;
            }
        }
    }

    /// Given the returned message of a GDB/MI command, get the value associated with the given attribute.
    ///
    /// Returns [`None`] if the value cannot be found.
    fn get_attribute(msg: String, pattern: &str) -> Option<String> {
        let mut values: Vec<&str> = msg.split(",").collect();
        values.retain(|&s| s.starts_with(pattern));
        if let Some(value) = values.first() {
            let value = value.strip_prefix(pattern)?;
            let value = value.strip_prefix('=')?;
            let value = value.strip_prefix('\"')?;
            let value = value.strip_suffix('\"')?;
            return Some(value.to_string());
        }
        None
    }
}

#[derive(Debug)]
pub struct Recap {
    failed: bool,
    recap: Vec<(u8, ActionResult)>,
}

impl Recap {
    pub fn new() -> Self {
        Self {
            failed: false,
            recap: vec![],
        }
    }

    pub fn aborted(&mut self) {
        self.failed = true
    }

    pub fn push(&mut self, id: u8, res: ActionResult) {
        self.recap.push((id, res))
    }
}

#[derive(Copy, Clone)]
pub enum QueryType {
    COMMAND,
}

#[derive(Debug)]
pub enum ActionResult {
    SUCCESS(String),
    FAILED(String),
    SATISFYING(String),
    FATAL(String),
    LOG(String),
    SKIP,
}
