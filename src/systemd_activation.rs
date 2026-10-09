//! Minimal systemd socket-activation boundary.
//!
//! HNSSH accepts exactly one native systemd listening descriptor. Keeping the
//! environment/descriptor handling here prevents production activation details
//! from leaking into the SSH server and leaves direct binding portable.

use std::env;
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::io;
use std::net::TcpListener;

use thiserror::Error;

const LISTEN_PID: &str = "LISTEN_PID";
const LISTEN_FDS: &str = "LISTEN_FDS";
const LISTEN_FDNAMES: &str = "LISTEN_FDNAMES";
#[cfg(target_os = "linux")]
const SD_LISTEN_FDS_START: libc::c_int = 3;

#[derive(Debug, Error)]
pub(crate) enum ActivationError {
    #[error("incomplete systemd socket-activation environment")]
    IncompleteEnvironment,
    #[error("invalid LISTEN_PID in systemd socket-activation environment")]
    InvalidProcessId,
    #[error("invalid LISTEN_FDS in systemd socket-activation environment")]
    InvalidDescriptorCount,
    #[error("HNSSH requires exactly one systemd SSH listener, received {0}")]
    UnexpectedDescriptorCount(usize),
    #[cfg(not(target_os = "linux"))]
    #[error("systemd socket activation is supported only on Linux")]
    UnsupportedPlatform,
    #[cfg(target_os = "linux")]
    #[error("invalid inherited systemd SSH listener")]
    InvalidListener(#[source] io::Error),
}

#[derive(Default)]
struct ActivationEnvironment {
    process_id: Option<OsString>,
    descriptor_count: Option<OsString>,
    descriptor_names: Option<OsString>,
}

impl ActivationEnvironment {
    fn take() -> Self {
        let environment = Self {
            process_id: env::var_os(LISTEN_PID),
            descriptor_count: env::var_os(LISTEN_FDS),
            descriptor_names: env::var_os(LISTEN_FDNAMES),
        };
        if environment.is_present() {
            // production_main calls this before constructing the Tokio runtime,
            // so no application-created thread can concurrently access the
            // process environment. Clearing mirrors sd_listen_fds(3) and keeps
            // descendants from accidentally reusing the descriptors.
            unsafe {
                env::remove_var(LISTEN_PID);
                env::remove_var(LISTEN_FDS);
                env::remove_var(LISTEN_FDNAMES);
            }
        }
        environment
    }

    fn is_present(&self) -> bool {
        self.process_id.is_some()
            || self.descriptor_count.is_some()
            || self.descriptor_names.is_some()
    }

    fn descriptor_count_for(
        &self,
        current_process_id: u32,
    ) -> Result<Option<usize>, ActivationError> {
        if !self.is_present() {
            return Ok(None);
        }
        let process_id = self
            .process_id
            .as_deref()
            .ok_or(ActivationError::IncompleteEnvironment)?
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or(ActivationError::InvalidProcessId)?;

        // systemd activation variables can be inherited by a child process.
        // As sd_listen_fds(3) does, ignore a set intended for another PID.
        if process_id != current_process_id {
            return Ok(None);
        }

        let descriptor_count = self
            .descriptor_count
            .as_deref()
            .ok_or(ActivationError::IncompleteEnvironment)?
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or(ActivationError::InvalidDescriptorCount)?;
        if descriptor_count != 1 {
            return Err(ActivationError::UnexpectedDescriptorCount(descriptor_count));
        }
        Ok(Some(descriptor_count))
    }
}

/// Takes the single TCP listener supplied through the native systemd protocol.
/// Returns `None` when this process was not socket activated.
pub(crate) fn take_listener() -> Result<Option<TcpListener>, ActivationError> {
    let environment = ActivationEnvironment::take();
    if environment
        .descriptor_count_for(std::process::id())?
        .is_none()
    {
        return Ok(None);
    }

    #[cfg(target_os = "linux")]
    {
        inherited_linux_listener().map(Some)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(ActivationError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "linux")]
fn inherited_linux_listener() -> Result<TcpListener, ActivationError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let descriptor = SD_LISTEN_FDS_START;
    // Validate that fd 3 exists before assuming ownership. The process exits on
    // every later validation error, so no activation descriptor can be reused.
    let descriptor_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if descriptor_flags == -1 {
        return Err(ActivationError::InvalidListener(io::Error::last_os_error()));
    }
    if unsafe {
        libc::fcntl(
            descriptor,
            libc::F_SETFD,
            descriptor_flags | libc::FD_CLOEXEC,
        )
    } == -1
    {
        return Err(ActivationError::InvalidListener(io::Error::last_os_error()));
    }

    // SAFETY: a matching LISTEN_PID/LISTEN_FDS pair assigns ownership of fd 3
    // to this process according to the systemd socket-activation protocol.
    let listener = unsafe { TcpListener::from_raw_fd(descriptor) };
    let socket_type = socket_option(listener.as_raw_fd(), libc::SO_TYPE)?;
    let accepting = socket_option(listener.as_raw_fd(), libc::SO_ACCEPTCONN)?;
    if socket_type != libc::SOCK_STREAM || accepting != 1 {
        return Err(ActivationError::InvalidListener(io::Error::new(
            io::ErrorKind::InvalidInput,
            "descriptor is not a listening TCP stream socket",
        )));
    }
    // TcpListener's address conversion rejects non-IP socket families.
    listener
        .local_addr()
        .map_err(ActivationError::InvalidListener)?;
    listener
        .set_nonblocking(true)
        .map_err(ActivationError::InvalidListener)?;
    Ok(listener)
}

#[cfg(target_os = "linux")]
fn socket_option(
    descriptor: std::os::fd::RawFd,
    option: libc::c_int,
) -> Result<libc::c_int, ActivationError> {
    let mut value = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            option,
            std::ptr::addr_of_mut!(value).cast(),
            std::ptr::addr_of_mut!(length),
        )
    };
    if result == -1 {
        Err(ActivationError::InvalidListener(io::Error::last_os_error()))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activation(
        process_id: Option<&str>,
        descriptor_count: Option<&str>,
    ) -> ActivationEnvironment {
        ActivationEnvironment {
            process_id: process_id.map(OsString::from),
            descriptor_count: descriptor_count.map(OsString::from),
            descriptor_names: None,
        }
    }

    #[test]
    fn absent_or_other_process_activation_falls_back_to_direct_binding() {
        assert_eq!(
            ActivationEnvironment::default()
                .descriptor_count_for(42)
                .expect("absent environment"),
            None
        );
        assert_eq!(
            activation(Some("41"), Some("1"))
                .descriptor_count_for(42)
                .expect("parent environment"),
            None
        );
    }

    #[test]
    fn matching_activation_requires_exactly_one_descriptor() {
        assert_eq!(
            activation(Some("42"), Some("1"))
                .descriptor_count_for(42)
                .expect("one descriptor"),
            Some(1)
        );
        assert!(matches!(
            activation(Some("42"), Some("0")).descriptor_count_for(42),
            Err(ActivationError::UnexpectedDescriptorCount(0))
        ));
        assert!(matches!(
            activation(Some("42"), Some("2")).descriptor_count_for(42),
            Err(ActivationError::UnexpectedDescriptorCount(2))
        ));
    }

    #[test]
    fn malformed_activation_fails_instead_of_silently_binding() {
        assert!(matches!(
            activation(None, Some("1")).descriptor_count_for(42),
            Err(ActivationError::IncompleteEnvironment)
        ));
        assert!(matches!(
            activation(Some("not-a-pid"), Some("1")).descriptor_count_for(42),
            Err(ActivationError::InvalidProcessId)
        ));
        assert!(matches!(
            activation(Some("42"), None).descriptor_count_for(42),
            Err(ActivationError::IncompleteEnvironment)
        ));
        assert!(matches!(
            activation(Some("42"), Some("many")).descriptor_count_for(42),
            Err(ActivationError::InvalidDescriptorCount)
        ));
    }
}
