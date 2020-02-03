# `io-mux`

A Mux provides a single receive end and multiple send ends. Data sent to any of
the send ends comes out the receive end, in order, tagged by the sender.

Each send end works as a file descriptor. For instance, with `io-mux` you can
collect stdout and stderr from a process, and highlight any error output from
stderr, while preserving the relative order of data across both stdout and
stderr.

[Documentation](https://docs.rs/io-mux)
