# `io-mux`

A Mux provides a single receive end and multiple send ends. Data sent to any of
the send ends comes out the receive end, in order, tagged by the sender.

Each send end works as a file descriptor. For instance, with `io-mux` you can
collect stdout and stderr from a process, and highlight any error output from
stderr, while preserving the relative order of data across both stdout and
stderr.

Note that reading provides no "EOF" indication; if no further data arrives, it
will block forever. Avoid reading after the source of the data exits.

[Documentation](https://docs.rs/io-mux)

## async

If you enable the `async` feature, `io-mux` additionally provides an `AsyncMux`
type, which allows processing data asynchronously.

You may want to use this with
[async-process](https://crates.io/crates/async-process) or
[async-pidfd](https://crates.io/crates/async-pidfd) to concurrently wait on the
exit of a process and the muxed output and error of that process. Until the
process exits, call `AsyncMux::read()` to get the next bit of output, awaiting
that concurrently with the exit of the process. Once the process exits and will
thus produce no further output, call `AsyncMux::read_nonblock` until it returns
`None` to drain the remaining output out of the mux.

## Portability

`io-mux` uses UNIX sockets, so it only runs on UNIX platforms. Support for
non-Linux platforms is experimental, and has a major caveat in its semantics;
please see the [documentation](https://docs.rs/io-mux) for more details.
