#![cfg(target_os = "linux")]

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use russh::keys::{Algorithm, PrivateKey, ssh_key};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn production_binary_accepts_the_native_systemd_listener_and_drains_on_sigterm() -> TestResult {
    let key_directory = tempfile::tempdir()?;
    let host_key_path = key_directory.path().join("host-key");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    fs::write(
        &host_key_path,
        host_key.to_openssh(ssh_key::LineEnding::LF)?.as_bytes(),
    )?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    let address_text = address.to_string();
    let host_key_text = host_key_path.to_str().ok_or("non-UTF-8 test path")?;
    let listener_descriptor = listener.as_raw_fd();
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "LISTEN_PID=$$; export LISTEN_PID; exec \"$@\"",
            "hnssh-socket-activation",
            env!("CARGO_BIN_EXE_hnssh"),
            "--host-key",
            host_key_text,
            "--listen-address",
            &address_text,
            "--operations-address",
            "127.0.0.1:0",
        ])
        .env("LISTEN_FDS", "1")
        .env("LISTEN_FDNAMES", "ssh")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // SAFETY: pre_exec performs only async-signal-safe fcntl/dup2 operations.
    // It makes the pre-bound listener the descriptor mandated by sd_listen_fds.
    unsafe {
        command.pre_exec(move || {
            if listener_descriptor != 3 && libc::dup2(listener_descriptor, 3) == -1 {
                return Err(io::Error::last_os_error());
            }
            let flags = libc::fcntl(3, libc::F_GETFD);
            if flags == -1 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    drop(listener);
    let result = (|| -> TestResult {
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut greeting = String::new();
        BufReader::new(stream).read_line(&mut greeting)?;
        if greeting.starts_with("SSH-2.0-") {
            Ok(())
        } else {
            Err(format!("inherited listener did not reach HNSSH: {greeting:?}").into())
        }
    })();
    if let Err(error) = result {
        if child.try_wait()?.is_none() {
            child.kill()?;
            child.wait()?;
        }
        return Err(error);
    }

    // SAFETY: the child PID came from this live Child handle, and SIGTERM has
    // no pointer or shared-memory requirements.
    if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } == -1 {
        let error = io::Error::last_os_error();
        if child.try_wait()?.is_none() {
            child.kill()?;
            child.wait()?;
        }
        return Err(error.into());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(format!("HNSSH did not exit cleanly after SIGTERM: {status}").into());
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Err("HNSSH did not finish draining within five seconds".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
