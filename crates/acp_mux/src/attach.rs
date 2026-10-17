use anyhow::{Context as _, Result, bail};
use std::{
    fs::OpenOptions,
    io::{self, Write as _},
    net::Shutdown,
    os::unix::{net::UnixStream, process::CommandExt as _},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    control::{self, AgentSpec, HandshakeRequest},
    log_path, socket_path,
};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(agent: String, key_env: Vec<String>, command: Vec<String>) -> Result<()> {
    let Some((program, args)) = command.split_first() else {
        bail!("missing agent command");
    };

    // Login commands (terminal authentication) reuse the agent command but run
    // interactively in a terminal, and must not be attached to the shared agent.
    // SAFETY: isatty only inspects the file descriptor.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        let error = std::process::Command::new(program).args(args).exec();
        return Err(error).with_context(|| format!("executing {program}"));
    }

    let spec = AgentSpec {
        program: program.clone(),
        args: args.to_vec(),
        env: std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect(),
        cwd: std::env::current_dir()
            .ok()
            .map(|cwd| cwd.to_string_lossy().into_owned()),
        key_env,
    };

    let stream = connect_or_spawn_daemon()?;
    let mut from_daemon = control::handshake(
        &stream,
        HandshakeRequest::Attach {
            agent,
            command: spec,
        },
    )?;

    let stdin_closed = Arc::new(AtomicBool::new(false));
    let to_stdout = std::thread::spawn({
        let stdin_closed = stdin_closed.clone();
        move || {
            let mut stdout = io::stdout().lock();
            let result = io::copy(&mut from_daemon, &mut stdout).and_then(|_| stdout.flush());
            if let Err(error) = result {
                eprintln!("acp-mux: forwarding agent output failed: {error}");
            }
            // The main thread is blocked reading stdin, so exit from here when
            // the daemon ends the connection first (for example, the agent exited).
            if !stdin_closed.load(Ordering::SeqCst) {
                eprintln!("acp-mux: the daemon closed the connection");
                std::process::exit(1);
            }
        }
    });

    let mut to_daemon = stream.try_clone()?;
    let mut stdin = io::stdin().lock();
    let result = io::copy(&mut stdin, &mut to_daemon);
    stdin_closed.store(true, Ordering::SeqCst);
    // Zed closed the agent's stdin: detach and let the daemon close its side.
    to_daemon.shutdown(Shutdown::Write).ok();
    if to_stdout.join().is_err() {
        bail!("the output thread panicked");
    }
    result.context("forwarding input to the acp-mux daemon")?;
    Ok(())
}

/// Connects to the daemon, starting it first if it is not running.
pub fn connect_or_spawn_daemon() -> Result<UnixStream> {
    let socket = socket_path();
    match UnixStream::connect(&socket) {
        Ok(stream) => return Ok(stream),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => {
            return Err(error).with_context(|| format!("connecting to {}", socket.display()));
        }
    }

    spawn_daemon()?;

    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    let mut delay = Duration::from_millis(10);
    loop {
        std::thread::sleep(delay);
        match UnixStream::connect(&socket) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() >= deadline => {
                return Err(error).with_context(|| {
                    format!(
                        "the acp-mux daemon did not start; see {}",
                        log_path().display()
                    )
                });
            }
            Err(_) => delay = (delay * 2).min(Duration::from_millis(500)),
        }
    }
}

fn spawn_daemon() -> Result<()> {
    let executable = std::env::current_exe().context("resolving the current executable")?;
    let log_path = log_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;

    let mut command = std::process::Command::new(executable);
    command.args(["acp-mux", "daemon"]).current_dir("/");
    // SAFETY: setsid is async-signal-safe. A new session detaches the daemon
    // from the SSH session, so it survives the connection closing.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut command = smol::process::Command::from(command);
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    command.spawn().context("starting the acp-mux daemon")?;
    Ok(())
}
