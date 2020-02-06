#![forbid(missing_docs)]
/*!
A Mux provides a single receive end and multiple send ends. Data sent to any of the send ends comes
out the receive end, in order, tagged by the sender.

Each send end works as a file descriptor. For instance, with `io-mux` you can collect stdout and
stderr from a process, and highlight any error output from stderr, while preserving the relative
order of data across both stdout and stderr.

# Example

```
# use std::io::Write;
# fn main() -> std::io::Result<()> {
let mut mux = io_mux::Mux::new()?;

let mut child = std::process::Command::new("sh")
    .arg("-c")
    .arg("echo out1 && echo err1 1>&2 && echo out2")
    .stdout(mux.make_untagged_sender()?)
    .stderr(mux.make_tagged_sender("e")?)
    .spawn()?;

let mut done_sender = mux.make_tagged_sender("d")?;
std::thread::spawn(move || match child.wait() {
    Ok(status) if status.success() => {
        let _ = write!(done_sender, "Done\n");
    }
    Ok(status) => {
        let _ = write!(done_sender, "Child process failed\n");
    }
    Err(e) => {
        let _ = write!(done_sender, "Error: {:?}\n", e);
    }
});

loop {
    let tagged_data = mux.read()?;
    if let Some(tag) = tagged_data.tag {
        print!("{}: ", tag);
        if tag == "d" {
            break;
        }
    }
    std::io::stdout().write_all(tagged_data.data)?;
}
#     Ok(())
# }
```

# Internals

Internally, `Mux` creates a UNIX datagram socket for the receive end, and a separate UNIX datagram
socket for each sender. Datagram sockets support `recvfrom`, which provides the address of the
sender, so `Mux::read` can use the sender address as the tag for the packet received.

However, datagram sockets require reading an entire datagram with each `recvfrom` call, so
`Mux::read` needs to find out the size of the next datagram before calling `recvfrom`. Linux
supports directly asking for the next packet size using `recv` with `MSG_PEEK | MSG_TRUNC`. On
other UNIX systems, we have to repeatedly call `recv` with `MSG_PEEK` and an increasingly large
buffer, until we receive the entire packet, then make one more call without `MSG_PEEK` to tell the
OS to discard it.

`Mux` creates UNIX sockets within a temporary directory, removed when dropping the `Mux`.

Note that `Mux::read` cannot provide any indication of end-of-file. When using `Mux`, you will need
to have some other indication that no further output will arrive, such as the exit of the child
process producing output.
*/

#[cfg(not(unix))]
compile_error!("io-mux only runs on UNIX");

use std::io;
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::Path;
use std::process::Stdio;

const DEFAULT_BUF_SIZE: usize = 8192;

/// A `Mux` provides a single receive end and multiple send ends. Data sent to any of the send ends
/// comes out the receive end, in order, tagged by the sender.
pub struct Mux {
    receive: UnixDatagram,
    tempdir: tempfile::TempDir,
    buf: Vec<u8>,
}

/// A send end of a `Mux`. You can convert a `MuxSender` to a `std::process::Stdio` for use with a
/// child process, obtain the underlying file descriptor using `IntoRawFd`, or send data using
/// `std::io::Write`.
pub struct MuxSender(UnixDatagram);

impl AsRawFd for MuxSender {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for MuxSender {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl From<MuxSender> for Stdio {
    fn from(sender: MuxSender) -> Stdio {
        unsafe { Stdio::from_raw_fd(sender.0.into_raw_fd()) }
    }
}

impl io::Write for MuxSender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.send(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Data received through a mux, along with the tag if any.
#[derive(Debug)]
pub struct TaggedData<'a> {
    /// Data received, borrowed from the `Mux`.
    pub data: &'a [u8],
    /// Tag for the sender of this data.
    pub tag: Option<String>,
}

impl Mux {
    /// Create a new `Mux`.
    ///
    /// This will create a temporary directory for all the sockets managed by this `Mux`; dropping
    /// the `Mux` removes the temporary directory.
    pub fn new() -> io::Result<Self> {
        let tempdir = tempfile::tempdir()?;
        std::fs::create_dir(tempdir.path().join("s"))?;
        let receive_path = tempdir.path().join("r");
        let receive = UnixDatagram::bind(&receive_path)?;

        // Shutdown writing to the receive socket, to help catch possible errors. On some targets,
        // this generates spurious errors, such as `Socket is not connected` on FreeBSD. We don't
        // need this shutdown for correctness, so just ignore any errors.
        let _ = receive.shutdown(Shutdown::Write);

        Ok(Mux {
            receive,
            tempdir,
            buf: vec![0; DEFAULT_BUF_SIZE],
        })
    }

    fn config_sender(&self, sender: UnixDatagram) -> io::Result<MuxSender> {
        let receive_path = self.tempdir.path().join("r");
        sender.connect(&receive_path)?;
        sender.shutdown(Shutdown::Read)?;
        Ok(MuxSender(sender))
    }

    /// Create a new `MuxSender` with no tag. Data sent via this `MuxSender` will arrive with a tag
    /// of `None`.
    pub fn make_untagged_sender(&self) -> io::Result<MuxSender> {
        self.config_sender(UnixDatagram::unbound()?)
    }

    /// Create a new `MuxSender` with the specified `tag`. Data sent via this `MuxSender` will
    /// arrive with a tag of `Some(tag)`.
    pub fn make_tagged_sender(&self, tag: &str) -> io::Result<MuxSender> {
        if tag.contains(std::path::is_separator) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Tag must not contain path separator",
            ));
        }
        let sender_path = self.tempdir.path().join("s").join(tag);
        self.config_sender(UnixDatagram::bind(&sender_path)?)
    }

    /// Helper function to call recv on the receive socket and handle errors.
    fn recv(fd: &mut UnixDatagram, buf: &mut [u8], flags: i32) -> io::Result<usize> {
        let ret = unsafe {
            libc::recv(
                fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                flags,
            )
        };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        };
        Ok(ret as usize)
    }

    #[cfg(target_os = "linux")]
    fn recv_from_full<'mux>(&'mux mut self) -> io::Result<(&'mux [u8], SocketAddr)> {
        let next_packet_len = Mux::recv(&mut self.receive, &mut [], libc::MSG_PEEK | libc::MSG_TRUNC)?;
        if next_packet_len > self.buf.len() {
            self.buf.resize(next_packet_len, 0);
        }
        let (bytes, addr) = self.receive.recv_from(&mut self.buf)?;
        Ok((&self.buf[..bytes], addr))
    }

    #[cfg(not(target_os = "linux"))]
    fn recv_from_full<'mux>(&'mux mut self) -> io::Result<(&'mux [u8], SocketAddr)> {
        loop {
            let bytes = Mux::recv(&mut self.receive, &mut self.buf, libc::MSG_PEEK)?;
            // If we filled the buffer, we may have truncated output. Retry with a bigger buffer.
            if bytes == self.buf.len() {
                let new_len = self.buf.len().saturating_mul(2);
                self.buf.resize(new_len, 0);
            } else {
                // Get the packet address, and clear it by fetching into a zero-sized buffer.
                let (_, addr) = self.receive.recv_from(&mut [])?;
                return Ok((&self.buf[..bytes], addr));
            }
        }
    }

    /// Return the next chunk of data, together with its tag.
    ///
    /// This reuses a buffer managed by the `Mux`.
    ///
    /// Note that this provides no "EOF" indication; if no further data arrives, it will block
    /// forever. Avoid calling it after the source of the data exits.
    pub fn read<'mux>(&'mux mut self) -> io::Result<TaggedData<'mux>> {
        let (data, addr) = self.recv_from_full()?;
        let tag = addr
            .as_pathname()
            .and_then(Path::file_name)
            .map(|s| s.to_string_lossy().into_owned());
        Ok(TaggedData { data, tag })
    }
}

#[cfg(test)]
mod test {
    use super::Mux;

    #[test]
    fn test() -> std::io::Result<()> {
        let mut mux = Mux::new()?;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("echo out1 && echo err1 1>&2 && echo out2 && echo err2 1>&2")
            .stdout(mux.make_untagged_sender()?)
            .stderr(mux.make_tagged_sender("e")?)
            .spawn()?;

        let mut done_sender = mux.make_tagged_sender("d")?;
        std::thread::spawn(move || {
            use std::io::Write;
            match child.wait() {
                Ok(status) if status.success() => {
                    let _ = write!(done_sender, "Done\n");
                }
                Ok(_) => {
                    let _ = write!(done_sender, "Child process failed\n");
                }
                Err(e) => {
                    let _ = write!(done_sender, "Error: {:?}\n", e);
                }
            }
        });

        let data1 = mux.read()?;
        assert!(data1.tag.is_none());
        assert_eq!(data1.data, b"out1\n");
        let data2 = mux.read()?;
        assert_eq!(data2.tag.as_deref(), Some("e"));
        assert_eq!(data2.data, b"err1\n");
        let data3 = mux.read()?;
        assert!(data3.tag.is_none());
        assert_eq!(data3.data, b"out2\n");
        let data4 = mux.read()?;
        assert_eq!(data4.tag.as_deref(), Some("e"));
        assert_eq!(data4.data, b"err2\n");
        let done = mux.read()?;
        assert_eq!(done.tag.as_deref(), Some("d"));
        assert_eq!(done.data, b"Done\n");

        Ok(())
    }

    #[test]
    fn test_path_separator() -> std::io::Result<()> {
        let mux = Mux::new()?;
        let result = mux.make_tagged_sender("a/b");
        assert!(result.is_err());
        Ok(())
    }
}
