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
`Mux::read` needs to find out the size of the next datagram before calling `recvfrom`. Finding the
next datagram size requires an OS-specific mechanism, currently only implemented on Linux systems.

`Mux` creates UNIX sockets within a temporary directory, removed when dropping the `Mux`.

Note that `Mux::read` cannot provide any indication of end-of-file. When using `Mux`, you will need
to have some other indication that no further output will arrive, such as the exit of the child
process producing output.
*/

use std::io;
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::process::Stdio;

#[cfg(not(target_os = "linux"))]
compile_error!("io-mux only runs on Linux");

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
        receive.shutdown(Shutdown::Write)?;

        Ok(Mux {
            receive,
            tempdir,
            buf: Vec::new(),
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

    /// Return the next chunk of data, together with its tag.
    ///
    /// This reuses a buffer managed by the `Mux`.
    ///
    /// Note that this provides no "EOF" indication; if no further data arrives, it will block
    /// forever. Avoid calling it after the source of the data exits.
    pub fn read<'mux>(&'mux mut self) -> io::Result<TaggedData<'mux>> {
        let next_packet_len = unsafe {
            libc::recv(
                self.receive.as_raw_fd(),
                std::ptr::null_mut(),
                0,
                libc::MSG_PEEK | libc::MSG_TRUNC,
            )
        };
        if next_packet_len == -1 {
            return Err(io::Error::last_os_error());
        };
        let next_packet_len = next_packet_len as usize;
        if next_packet_len > self.buf.len() {
            self.buf.resize(next_packet_len, 0);
        }
        let (bytes, addr) = self.receive.recv_from(&mut self.buf)?;
        let tag = if let Some(path) = addr.as_pathname() {
            path.file_name().map(|s| s.to_string_lossy().into_owned())
        } else {
            None
        };
        Ok(TaggedData {
            data: &self.buf[..bytes],
            tag,
        })
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
}
