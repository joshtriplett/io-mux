#![forbid(missing_docs)]
/*!
A Mux provides a single receive end and multiple send ends. Data sent to any of the send ends comes
out the receive end, in order, tagged by the sender.

Each send end works as a file descriptor. For instance, with `io-mux` you can collect stdout and
stderr from a process, and highlight any error output from stderr, while preserving the relative
order of data across both stdout and stderr.

Note that reading provides no "EOF" indication; if no further data arrives, it
will block forever. Avoid reading after the source of the data exits.

# Example

```
# use std::io::Write;
# fn main() -> std::io::Result<()> {
use io_mux::{Mux, TaggedData};
let mut mux = Mux::new()?;

let (out_tag, out_sender) = mux.make_sender()?;
let (err_tag, err_sender) = mux.make_sender()?;
let mut child = std::process::Command::new("sh")
    .arg("-c")
    .arg("echo out1 && echo err1 1>&2 && echo out2")
    .stdout(out_sender)
    .stderr(err_sender)
    .spawn()?;

let (done_tag, mut done_sender) = mux.make_sender()?;
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

let mut done = false;
while !done {
    let TaggedData { data, tag } = mux.read()?;
    if tag == out_tag {
        print!("out: ");
    } else if tag == err_tag {
        print!("err: ");
    } else if tag == done_tag {
        done = true;
    } else {
        panic!("Unexpected tag");
    }
    std::io::stdout().write_all(data)?;
}
#     Ok(())
# }
```

# async

If you enable the `async` feature, `io-mux` additionally provides an `AsyncMux` type, which allows
processing data asynchronously.

You may want to use this with [async-process](https://crates.io/crates/async-process) or
[async-pidfd](https://crates.io/crates/async-pidfd) to concurrently wait on the exit of a process
and the muxed output and error of that process. Until the process exits, call `AsyncMux::read()` to
get the next bit of output, awaiting that concurrently with the exit of the process. Once the
process exits and will thus produce no further output, call `AsyncMux::read_nonblock` until it
returns `None` to drain the remaining output out of the mux.

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

# Portability
Mux can theoretically run on any UNIX system. However, on some non-Linux systems, when the buffers
for a UNIX socket fill up, writing to the UNIX socket may return an `ENOBUFS` error rather than
blocking. Thus, on non-Linux systems, the process writing to a `MuxSender` may encounter an error
if the receiving process does not process its buffers quickly enough. This does not match the
behavior of a pipe. As this may result in surprising behavior, by default io-mux does not compile
on non-Linux systems. If you want to use io-mux on a non-Linux system, and your use case does not
need the same semantics as a pipe, and *in particular* it will not cause a problem in your use case
if writing to a `MuxSender` may produce an `ENOBUFS` error if you do not read from the receive end
quickly enough, then you can compile `io-mux` on non-Linux platforms by enabling the
`experimental-unix-support` feature of `io-mux`.

If you have another UNIX platform which blocks on writes to a UNIX datagram socket with full
buffers, as Linux does, then please send a note to the io-mux maintainer to mark support for your
platform as non-experimental.
*/

#[cfg(not(unix))]
compile_error!("io-mux only runs on UNIX");

#[cfg(all(
    unix,
    not(target_os = "linux"),
    not(feature = "experimental-unix-support")
))]
compile_error!(
    "io-mux support for non-Linux platforms is experimental.
Please read the portability note in the io-mux documentation for more information
and potential caveats, before enabling io-mux's experimental UNIX support."
);

use std::io;
use std::net::Shutdown;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::linux::net::SocketAddrExt;
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::Path;
use std::process::Stdio;

#[cfg(feature = "async")]
use async_io::Async;
use rustix::net::RecvFlags;

const DEFAULT_BUF_SIZE: usize = 8192;

/// A `Mux` provides a single receive end and multiple send ends. Data sent to any of the send ends
/// comes out the receive end, in order, tagged by the sender.
///
/// `Mux` implements `AsFd` solely to support polling the underlying file descriptor for data to
/// read. Always use `Mux` to perform the actual read.
pub struct Mux {
    receive: UnixDatagram,
    receive_addr: SocketAddr,
    tempdir: Option<tempfile::TempDir>,
    buf: Vec<u8>,
}

impl AsFd for Mux {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.receive.as_fd()
    }
}

impl AsRawFd for Mux {
    fn as_raw_fd(&self) -> RawFd {
        self.receive.as_raw_fd()
    }
}

/// A send end of a `Mux`. You can convert a `MuxSender` to a `std::process::Stdio` for use with a
/// child process, obtain the underlying file descriptor as an `OwnedFd`, or send data using
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

impl AsFd for MuxSender {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl From<MuxSender> for OwnedFd {
    fn from(sender: MuxSender) -> OwnedFd {
        sender.0.into()
    }
}

impl From<MuxSender> for Stdio {
    fn from(sender: MuxSender) -> Stdio {
        Stdio::from(OwnedFd::from(sender))
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

/// A unique tag associated with a sender.
#[derive(Clone, Debug)]
pub struct Tag(SocketAddr);

impl PartialEq<Tag> for Tag {
    fn eq(&self, rhs: &Tag) -> bool {
        #[cfg(target_os = "linux")]
        if let (Some(lhs), Some(rhs)) = (self.0.as_abstract_name(), rhs.0.as_abstract_name()) {
            return lhs == rhs;
        }
        if let (Some(lhs), Some(rhs)) = (self.0.as_pathname(), rhs.0.as_pathname()) {
            return lhs == rhs;
        }
        self.0.is_unnamed() && rhs.0.is_unnamed()
    }
}

impl Eq for Tag {}

/// Data received through a mux, along with the tag.
#[derive(Debug, Eq, PartialEq)]
pub struct TaggedData<'a> {
    /// Data received, borrowed from the `Mux`.
    pub data: &'a [u8],
    /// Tag for the sender of this data.
    pub tag: Tag,
}

impl Mux {
    /// Create a new `Mux`, using Linux abstract sockets.
    #[cfg(target_os = "linux")]
    pub fn new_abstract() -> io::Result<Self> {
        // It should be incredibly unlikely to have a collision, so if we have multiple in a row,
        // something strange is likely going on, and we might continue to get the same error
        // indefinitely. Bail after a large number of retries, so that we don't loop forever.
        for _ in 0..32768 {
            let receive_addr =
                SocketAddr::from_abstract_name(format!("io-mux-{:x}", fastrand::u128(..)))?;
            match Self::new_with_addr(receive_addr, None) {
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
                result => return result,
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "couldn't create unique socket name",
        ))
    }

    /// Create a new `Mux`.
    ///
    /// This will create a temporary directory for all the sockets managed by this `Mux`; dropping
    /// the `Mux` removes the temporary directory.
    pub fn new() -> io::Result<Self> {
        Self::new_with_tempdir(tempfile::tempdir()?)
    }

    /// Create a new `Mux`, with temporary directory under the specified path.
    ///
    /// This will create a temporary directory for all the sockets managed by this `Mux`; dropping
    /// the `Mux` removes the temporary directory.
    pub fn new_in<P: AsRef<Path>>(dir: P) -> io::Result<Self> {
        Self::new_with_tempdir(tempfile::tempdir_in(dir)?)
    }

    fn new_with_tempdir(tempdir: tempfile::TempDir) -> io::Result<Self> {
        let receive_addr = SocketAddr::from_pathname(tempdir.path().join("r"))?;
        Self::new_with_addr(receive_addr, Some(tempdir))
    }

    fn new_with_addr(
        receive_addr: SocketAddr,
        tempdir: Option<tempfile::TempDir>,
    ) -> io::Result<Self> {
        let receive = UnixDatagram::bind_addr(&receive_addr)?;

        // Shutdown writing to the receive socket, to help catch possible errors. On some targets,
        // this generates spurious errors, such as `Socket is not connected` on FreeBSD. We don't
        // need this shutdown for correctness, so just ignore any errors.
        let _ = receive.shutdown(Shutdown::Write);

        Ok(Mux {
            receive,
            receive_addr,
            tempdir,
            buf: vec![0; DEFAULT_BUF_SIZE],
        })
    }

    /// Create a new `MuxSender` and associated unique `Tag`. Data sent via the returned
    /// `MuxSender` will arrive with the corresponding `Tag`.
    pub fn make_sender(&self) -> io::Result<(Tag, MuxSender)> {
        if let Some(ref tempdir) = self.tempdir {
            self.make_sender_with_retry(|n| {
                SocketAddr::from_pathname(tempdir.path().join(format!("{n:x}")))
            })
        } else {
            #[cfg(target_os = "linux")]
            return self.make_sender_with_retry(|n| {
                SocketAddr::from_abstract_name(format!("io-mux-send-{n:x}"))
            });
            #[cfg(not(target_os = "linux"))]
            panic!("Mux without tempdir on non-Linux platform")
        }
    }

    fn make_sender_with_retry(
        &self,
        make_sender_addr: impl Fn(u128) -> io::Result<SocketAddr>,
    ) -> io::Result<(Tag, MuxSender)> {
        // It should be incredibly unlikely to have collisions, but avoid looping forever in case
        // something strange is going on (e.g. weird seccomp filter).
        for _ in 0..32768 {
            let sender_addr = make_sender_addr(fastrand::u128(..))?;
            let sender = match UnixDatagram::bind_addr(&sender_addr) {
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
                result => result,
            }?;
            sender.connect_addr(&self.receive_addr)?;
            sender.shutdown(Shutdown::Read)?;
            return Ok((Tag(sender_addr), MuxSender(sender)));
        }
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "couldn't create unique socket name",
        ))
    }

    #[cfg(all(target_os = "linux", not(feature = "test-portable")))]
    fn recv_from_full<'mux>(&'mux mut self) -> io::Result<(&'mux [u8], SocketAddr)> {
        let next_packet_len = rustix::net::recv(
            &mut self.receive,
            &mut [],
            RecvFlags::PEEK | RecvFlags::TRUNC,
        )?;
        if next_packet_len > self.buf.len() {
            self.buf.resize(next_packet_len, 0);
        }
        let (bytes, addr) = self.receive.recv_from(&mut self.buf)?;
        Ok((&self.buf[..bytes], addr))
    }

    #[cfg(not(all(target_os = "linux", not(feature = "test-portable"))))]
    fn recv_from_full<'mux>(&'mux mut self) -> io::Result<(&'mux [u8], SocketAddr)> {
        loop {
            let bytes = rustix::net::recv(&mut self.receive, &mut self.buf, RecvFlags::PEEK)?;
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
        let tag = Tag(addr);
        Ok(TaggedData { data, tag })
    }
}

/// Asynchronous version of `Mux`.
#[cfg(feature = "async")]
pub struct AsyncMux(Async<Mux>);

#[cfg(feature = "async")]
impl AsyncMux {
    /// Create a new `Mux`, using Linux abstract sockets.
    #[cfg(target_os = "linux")]
    pub fn new_abstract() -> io::Result<Self> {
        Ok(Self(Async::new(Mux::new_abstract()?)?))
    }

    /// Create a new `AsyncMux`.
    ///
    /// This will create a temporary directory for all the sockets managed by this `AsyncMux`;
    /// dropping the `AsyncMux` removes the temporary directory.
    pub fn new() -> io::Result<Self> {
        Ok(Self(Async::new(Mux::new()?)?))
    }

    /// Create a new `AsyncMux`, with temporary directory under the specified path.
    ///
    /// This will create a temporary directory for all the sockets managed by this `AsyncMux`;
    /// dropping the `AsyncMux` removes the temporary directory.
    pub fn new_in<P: AsRef<Path>>(dir: P) -> io::Result<Self> {
        Ok(Self(Async::new(Mux::new_in(dir)?)?))
    }

    /// Create a new `MuxSender` and associated unique `Tag`. Data sent via the returned
    /// `MuxSender` will arrive with the corresponding `Tag`.
    pub fn make_sender(&self) -> io::Result<(Tag, MuxSender)> {
        self.0.get_ref().make_sender()
    }

    /// Return the next chunk of data, together with its tag.
    ///
    /// This reuses a buffer managed by the `AsyncMux`.
    ///
    /// Note that this provides no "EOF" indication; if no further data arrives, it will block
    /// forever. Avoid calling it after the source of the data exits. Once the source of the data
    /// exits, call `read_nonblock` instead, until it returns None.
    pub async fn read<'mux>(&'mux mut self) -> io::Result<TaggedData<'mux>> {
        self.0.readable().await?;
        let m = unsafe { self.0.get_mut() };
        m.read()
    }

    /// Return the next chunk of data, together with its tag, if available immediately, or None if
    /// the read would block.
    ///
    /// This reuses a buffer managed by the `AsyncMux`.
    ///
    /// Use this if you know no more data will get sent and you want to drain the remaining data.
    pub fn read_nonblock<'mux>(&'mux mut self) -> io::Result<Option<TaggedData<'mux>>> {
        let m = unsafe { self.0.get_mut() };
        match m.read() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            ret => ret.map(Some),
        }
    }
}

#[cfg(test)]
mod test {
    #[cfg(feature = "async")]
    use super::AsyncMux;
    use super::Mux;

    #[test]
    fn test() -> std::io::Result<()> {
        test_with_mux(Mux::new()?)
    }

    #[test]
    fn test_new_in() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let dir_entries = || -> std::io::Result<usize> {
            Ok(dir.path().read_dir()?.collect::<Result<Vec<_>, _>>()?.len())
        };
        assert_eq!(dir_entries()?, 0);
        let mux = Mux::new_in(dir.path())?;
        assert_eq!(dir_entries()?, 1);
        test_with_mux(mux)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_abstract() -> std::io::Result<()> {
        test_with_mux(Mux::new_abstract()?)
    }

    fn test_with_mux(mut mux: Mux) -> std::io::Result<()> {
        let (out_tag, out_sender) = mux.make_sender()?;
        let (err_tag, err_sender) = mux.make_sender()?;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("echo out1 && echo err1 1>&2 && echo out2 && echo err2 1>&2")
            .stdout(out_sender)
            .stderr(err_sender)
            .spawn()?;

        let (done_tag, mut done_sender) = mux.make_sender()?;
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
        assert_eq!(data1.tag, out_tag);
        assert_eq!(data1.data, b"out1\n");
        let data2 = mux.read()?;
        assert_eq!(data2.tag, err_tag);
        assert_eq!(data2.data, b"err1\n");
        let data3 = mux.read()?;
        assert_eq!(data3.tag, out_tag);
        assert_eq!(data3.data, b"out2\n");
        let data4 = mux.read()?;
        assert_eq!(data4.tag, err_tag);
        assert_eq!(data4.data, b"err2\n");
        let done = mux.read()?;
        assert_eq!(done.tag, done_tag);
        assert_eq!(done.data, b"Done\n");

        Ok(())
    }

    #[cfg(feature = "async")]
    fn test_with_async_mux(mut mux: AsyncMux) -> std::io::Result<()> {
        use futures_lite::{future, FutureExt};

        future::block_on(async {
            let (out_tag, out_sender) = mux.make_sender()?;
            let (err_tag, err_sender) = mux.make_sender()?;
            let mut child = async_process::Command::new("sh")
                .arg("-c")
                .arg("echo out1 && echo err1 1>&2 && echo out2 && echo err2 1>&2")
                .stdout(out_sender)
                .stderr(err_sender)
                .spawn()?;
            let mut expected = vec![
                (out_tag.clone(), b"out1\n"),
                (err_tag.clone(), b"err1\n"),
                (out_tag, b"out2\n"),
                (err_tag, b"err2\n"),
            ];
            let mut expected = expected.drain(..);
            let mut status = None;
            while status.is_none() {
                async {
                    status = Some(child.status().await?);
                    Ok::<(), std::io::Error>(())
                }
                .or(async {
                    let data = mux.read().await?;
                    let (expected_tag, expected_data) = expected.next().unwrap();
                    assert_eq!(data.tag, expected_tag);
                    assert_eq!(data.data, expected_data);
                    Ok(())
                })
                .await?;
            }
            while let Some(data) = mux.read_nonblock()? {
                let (expected_tag, expected_data) = expected.next().unwrap();
                assert_eq!(data.tag, expected_tag);
                assert_eq!(data.data, expected_data);
            }
            assert!(status.unwrap().success());
            assert_eq!(expected.next(), None);
            Ok(())
        })
    }

    #[cfg(feature = "async")]
    #[test]
    fn test_async() -> std::io::Result<()> {
        test_with_async_mux(AsyncMux::new()?)
    }

    #[cfg(all(feature = "async", target_os = "linux"))]
    #[test]
    fn test_abstract_async() -> std::io::Result<()> {
        test_with_async_mux(AsyncMux::new_abstract()?)
    }
}
