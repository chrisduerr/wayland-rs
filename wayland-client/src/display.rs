use std::env;
use std::ffi::OsString;
use std::io;
use std::ops::Deref;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use nix::fcntl;

use crate::{EventQueue, Proxy};

use crate::imp::DisplayInner;

#[cfg(feature = "use_system_lib")]
use wayland_sys::client::wl_display;

/// Enum representing the possible reasons why connecting to the wayland server failed
#[derive(Debug)]
pub enum ConnectError {
    /// The library was compiled with the `dlopen` feature, and the `libwayland-client.so`
    /// library could not be found at runtime
    NoWaylandLib,
    /// The `XDG_RUNTIME_DIR` variable is not set while it should be
    XdgRuntimeDirNotSet,
    /// Any needed library was found, but the listening socket of the server was not.
    ///
    /// Most of the time, this means that the program was not started from a wayland session.
    NoCompositorListening,
    /// The provided socket name is invalid
    InvalidName,
    /// The FD provided in `WAYLAND_SOCKET` was invalid
    InvalidFd,
}

impl ::std::error::Error for ConnectError {}

impl ::std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
        match *self {
            ConnectError::NoWaylandLib => f.write_str("Could not find libwayland-client.so."),
            ConnectError::XdgRuntimeDirNotSet => f.write_str("XDG_RUNTIME_DIR is not set."),
            ConnectError::NoCompositorListening => f.write_str("Could not find a listening wayland compositor."),
            ConnectError::InvalidName => f.write_str("The wayland socket name is invalid."),
            ConnectError::InvalidFd => f.write_str("The FD provided in WAYLAND_SOCKET is invalid."),
        }
    }
}

/// A protocol error
///
/// This kind of error is generated by the server if your client didn't respect
/// the protocol, after which the server will kill your connection.
///
/// If the dispatching methods of `EventQueues` start to fail, you may want to
/// check `Display::protocol_error()` to see if a protocol error was generated.
#[derive(Clone, Debug)]
pub struct ProtocolError {
    /// The error code associated with the error
    ///
    /// It should be interpreted as an instance of the `Error` enum of the
    /// associated interface.
    pub code: u32,
    /// The id of the object that caused the error
    pub object_id: u32,
    /// The interface of the object that caused the error
    pub object_interface: &'static str,
    /// The message sent by the server describing the error
    pub message: String,
}

impl ::std::error::Error for ProtocolError {
    fn description(&self) -> &str {
        "Wayland protocol error"
    }
}

impl ::std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
        write!(
            f,
            "Protocol error {} on object {}@{}: {}",
            self.code, self.object_interface, self.object_id, self.message
        )
    }
}

/// A connection to a wayland server
///
/// This object both represent the connection to the server and contains the
/// primary `WlDisplay` wayland object. As such, it must be kept alive as long
/// as you are connected. You can access the contained `WlDisplay` via `Deref`
/// to create all the objects you need.
#[derive(Clone)]
pub struct Display {
    pub(crate) inner: Arc<DisplayInner>,
}

impl Display {
    /// Attempt to connect to a wayland server using the contents of the environment variables
    ///
    /// First of all, if the `WAYLAND_SOCKET` environment variable is set, it'll try to interpret
    /// it as a FD number to use.
    ///
    /// Otherwise, it will try to connect to the socket name defined in the `WAYLAND_DISPLAY`
    /// environment variable, and error if it is not set.
    ///
    /// This requires the `XDG_RUNTIME_DIR` variable to be properly set.
    pub fn connect_to_env() -> Result<Display, ConnectError> {
        if let Ok(txt) = env::var("WAYLAND_SOCKET") {
            // We should connect to the provided WAYLAND_SOCKET
            let fd = txt.parse::<i32>().map_err(|_| ConnectError::InvalidFd)?;
            // set the CLOEXEC flag on this FD
            let flags = fcntl::fcntl(fd, fcntl::FcntlArg::F_GETFD);
            let result = flags
                .map(|f| fcntl::FdFlag::from_bits(f).unwrap() | fcntl::FdFlag::FD_CLOEXEC)
                .and_then(|f| fcntl::fcntl(fd, fcntl::FcntlArg::F_SETFD(f)));
            match result {
                Ok(_) => {
                    // setting the O_CLOEXEC worked
                    unsafe { Display::from_fd(fd) }
                }
                Err(_) => {
                    // something went wrong in F_GETFD or F_SETFD
                    let _ = ::nix::unistd::close(fd);
                    Err(ConnectError::InvalidFd)
                }
            }
        } else {
            let mut socket_path = env::var_os("XDG_RUNTIME_DIR")
                .map(Into::<PathBuf>::into)
                .ok_or(ConnectError::XdgRuntimeDirNotSet)?;
            socket_path.push(env::var_os("WAYLAND_DISPLAY").ok_or(ConnectError::NoCompositorListening)?);

            let socket = UnixStream::connect(socket_path).map_err(|_| ConnectError::NoCompositorListening)?;
            unsafe { Display::from_fd(socket.into_raw_fd()) }
        }
    }

    /// Attempt to connect to a wayland server socket with given name
    ///
    /// On success, you are given the `Display` object as well as the main `EventQueue` hosting
    /// the `WlDisplay` wayland object.
    ///
    /// This requires the `XDG_RUNTIME_DIR` variable to be properly set.
    pub fn connect_to_name<S: Into<OsString>>(name: S) -> Result<Display, ConnectError> {
        let mut socket_path = env::var_os("XDG_RUNTIME_DIR")
            .map(Into::<PathBuf>::into)
            .ok_or(ConnectError::XdgRuntimeDirNotSet)?;
        socket_path.push(name.into());

        let socket = UnixStream::connect(socket_path).map_err(|_| ConnectError::NoCompositorListening)?;
        unsafe { Display::from_fd(socket.into_raw_fd()) }
    }

    /// Attempt to use an already connected unix socket on given FD to start a wayland connection
    ///
    /// On success, you are given the `Display` object.
    ///
    /// Will take ownership of the FD.
    ///
    /// # Safety
    ///
    /// The file descriptor must be associated to a connected unix socket.
    pub unsafe fn from_fd(fd: RawFd) -> Result<Display, ConnectError> {
        Ok(Display {
            inner: DisplayInner::from_fd(fd)?,
        })
    }

    /// Non-blocking write to the server
    ///
    /// Outgoing messages to the server are buffered by the library for efficiency. This method
    /// flushes the internal buffer to the server socket.
    ///
    /// Will write as many pending requests as possible to the server socket. Never blocks: if not all
    /// requests could be written, will return an io error `WouldBlock`.
    ///
    /// This function is identical to `EventQueue::flush`
    pub fn flush(&self) -> io::Result<()> {
        self.inner.flush()
    }

    /// Create a new event queue associated with this wayland connection
    pub fn create_event_queue(&self) -> EventQueue {
        let evq_inner = DisplayInner::create_event_queue(&self.inner);
        EventQueue::new(evq_inner, self.clone())
    }

    /// Retrieve the last protocol error if any occured
    ///
    /// If your client does not respect some part of a protocol it is using, the server
    /// will send a special "protocol error" event and kill your connection. This method
    /// allows you to retrieve the contents of this event if it occured.
    ///
    /// If the dispatch methods of the `EventQueue` return an error, this is an indication
    /// that a protocol error may have occured. Such errors are not recoverable, but this
    /// method allows you to gracefully display them to the user, along with indications for
    /// submitting a bug-report for example.
    pub fn protocol_error(&self) -> Option<ProtocolError> {
        self.inner.protocol_error()
    }

    /// Retrieve the file descriptor associated with the wayland socket
    ///
    /// This FD should only be used to integrate into a polling mechanism, and should
    /// never be directly read from or written to.
    pub fn get_connection_fd(&self) -> ::std::os::unix::io::RawFd {
        self.inner.get_connection_fd()
    }

    #[cfg(feature = "use_system_lib")]
    /// Create a Display and from an external display
    ///
    /// This allows you to interface with an already-existing wayland connection,
    /// for example provided by a GUI toolkit.
    ///
    /// Note that if you need to retrieve the actual `wl_display` pointer back (rather than
    /// its wrapper), you must use the `get_display_ptr()` method.
    ///
    /// # Safety
    ///
    /// The provided pointer must point to a valid `wl_display` from `libwayland-client`
    pub unsafe fn from_external_display(display_ptr: *mut wl_display) -> Display {
        Display {
            inner: DisplayInner::from_external(display_ptr),
        }
    }

    #[cfg(feature = "use_system_lib")]
    /// Retrieve the `wl_display` pointer
    ///
    /// If this `Display` was created from an external `wl_display`, its `c_ptr()` method will
    /// return a wrapper to the actual display. While this is perfectly good as a `wl_proxy`
    /// pointer, to send requests, this is not the actual `wl_display` and cannot be used as such.
    ///
    /// This method will give you the `wl_display`.
    pub fn get_display_ptr(&self) -> *mut wl_display {
        self.inner.ptr()
    }
}

impl Deref for Display {
    type Target = Proxy<crate::protocol::wl_display::WlDisplay>;
    fn deref(&self) -> &Proxy<crate::protocol::wl_display::WlDisplay> {
        self.inner.get_proxy()
    }
}
