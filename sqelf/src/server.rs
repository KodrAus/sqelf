use std::{
    marker::Unpin,
    net::SocketAddr,
    str::FromStr,
    time::Duration,
};

use futures::{
    future::{
        BoxFuture,
        Either,
    },
    select,
};

use tokio::{
    net::signal::ctrl_c,
    prelude::*,
    runtime::Runtime,
    sync::oneshot,
};

use bytes::{
    Bytes,
    BytesMut,
};

use crate::{
    diagnostics::*,
    error::Error,
    receive::Message,
};

metrics! {
    receive_ok,
    receive_err,
    process_ok,
    process_err,
    tcp_conn_accept,
    tcp_conn_close,
    tcp_conn_timeout,
    tcp_msg_overflow
}

/**
Server configuration.
*/
#[derive(Debug, Clone)]
pub struct Config {
    /**
    The address to bind the server to.
    */
    pub bind: Bind,
    /**
    The duration to keep client TCP connections alive for.

    If the client doesn't complete a message within the period
    then the connection will be closed.
    */
    pub tcp_keep_alive_secs: u64,
    /**
    The maximum size of a single event before it'll be discarded.
    */
    pub tcp_max_size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Bind {
    pub addr: String,
    pub protocol: Protocol,
}

#[derive(Debug, Clone, Copy)]
pub enum Protocol {
    Udp,
    Tcp,
}

impl FromStr for Bind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.get(0..6) {
            Some("tcp://") => Ok(Bind {
                addr: s[6..].to_owned(),
                protocol: Protocol::Tcp,
            }),
            Some("udp://") => Ok(Bind {
                addr: s[6..].to_owned(),
                protocol: Protocol::Udp,
            }),
            _ => Ok(Bind {
                addr: s.to_owned(),
                protocol: Protocol::Udp,
            }),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: Bind {
                addr: "0.0.0.0:12201".to_owned(),
                protocol: Protocol::Udp,
            },
            tcp_keep_alive_secs: 2 * 60,    // 2 minutes
            tcp_max_size_bytes: 1024 * 256, // 256kiB
        }
    }
}

/**
A GELF server.
*/
pub struct Server {
    fut: BoxFuture<'static, ()>,
    handle: Option<Handle>,
}

impl Server {
    pub fn take_handle(&mut self) -> Option<Handle> {
        self.handle.take()
    }

    pub fn run(self) -> Result<(), Error> {
        // Run the server on a fresh runtime
        // We attempt to shut this runtime down cleanly to release
        // any used resources
        let runtime = Runtime::new().expect("failed to start new Runtime");

        runtime.block_on(self.fut);
        runtime.shutdown_now();

        Ok(())
    }
}

/**
A handle to a running GELF server that can be used to interact with it
programmatically.
*/
pub struct Handle {
    close: oneshot::Sender<()>,
}

impl Handle {
    /**
    Close the server.
    */
    pub fn close(self) -> bool {
        self.close.send(()).is_ok()
    }
}

/**
Build a server to receive GELF messages and process them.
*/
pub fn build(
    config: Config,
    receive: impl FnMut(Bytes) -> Result<Option<Message>, Error> + Send + Sync + Unpin + Clone + 'static,
    mut process: impl FnMut(Message) -> Result<(), Error> + Send + Sync + Unpin + Clone + 'static,
) -> Result<Server, Error> {
    emit("Starting GELF server");

    let addr = config.bind.addr.parse()?;
    let (handle_tx, handle_rx) = oneshot::channel();

    // Build a handle
    let handle = Some(Handle { close: handle_tx });
    let ctrl_c = ctrl_c()?;

    let server = async move {
        let incoming = match config.bind.protocol {
            Protocol::Udp => {
                let server = udp::Server::bind(&addr).await?.build(receive);

                Either::Left(server)
            }
            Protocol::Tcp => {
                let server = tcp::Server::bind(&addr).await?.build(
                    Duration::from_secs(config.tcp_keep_alive_secs),
                    config.tcp_max_size_bytes as usize,
                    receive,
                );

                Either::Right(server)
            }
        };

        let mut close = handle_rx.fuse();
        let mut ctrl_c = ctrl_c.fuse();
        let mut incoming = incoming.fuse();

        // NOTE: We don't use `?` here because we never want to carry results
        // We always want to match them and deal with error cases directly
        loop {
            select! {
                // A message that's ready to process
                msg = incoming.next() => match msg {
                    // A complete message has been received
                    Some(Ok(Received::Complete(msg))) => {
                        increment!(server.receive_ok);

                        // Process the received message
                        match process(msg) {
                            Ok(()) => {
                                increment!(server.process_ok);
                            }
                            Err(err) => {
                                increment!(server.process_err);
                                emit_err(&err, "GELF processing failed");
                            }
                        }
                    },
                    // A chunk of a message has been received
                    Some(Ok(Received::Incomplete)) => {
                        continue;
                    },
                    // An error occurred receiving a chunk
                    Some(Err(err)) => {
                        increment!(server.receive_err);
                        emit_err(&err, "GELF processing failed");
                    },
                    None => {
                        unreachable!("receiver stream should never terminate")
                    },
                },
                // A termination signal from the programmatic handle
                _ = close => {
                    emit("Handle closed; shutting down");
                    break;
                },
                // A termination signal from the environment
                _ = ctrl_c.next() => {
                    emit("Termination signal received; shutting down");
                    break;
                },
            };
        }

        emit("Stopping GELF server");

        Result::Ok::<(), Error>(())
    };

    Ok(Server {
        fut: Box::pin(async move {
            if let Err(err) = server.await {
                emit_err(&err, "GELF server failed");
            }
        }),
        handle,
    })
}

enum Received {
    Incomplete,
    Complete(Message),
}

trait OptionMessageExt {
    fn into_received(self) -> Option<Received>;
}

impl OptionMessageExt for Option<Message> {
    fn into_received(self) -> Option<Received> {
        match self {
            Some(msg) => Some(Received::Complete(msg)),
            None => Some(Received::Incomplete),
        }
    }
}

mod udp {
    use super::*;

    use tokio::{
        codec::Decoder,
        net::udp::{
            UdpFramed,
            UdpSocket,
        },
    };

    pub(super) struct Server(UdpSocket);

    impl Server {
        pub(super) async fn bind(addr: &SocketAddr) -> Result<Self, Error> {
            let sock = UdpSocket::bind(&addr).await?;

            Ok(Server(sock))
        }

        pub(super) fn build(
            self,
            receive: impl FnMut(Bytes) -> Result<Option<Message>, Error> + Unpin,
        ) -> impl Stream<Item = Result<Received, Error>> {
            emit("Setting up for UDP");

            UdpFramed::new(self.0, Decode(receive)).map(|r| r.map(|(msg, _)| msg))
        }
    }

    struct Decode<F>(F);

    impl<F> Decoder for Decode<F>
    where
        F: FnMut(Bytes) -> Result<Option<Message>, Error> + Unpin,
    {
        type Item = Received;
        type Error = Error;

        fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            // All datagrams are considered a valid message
            let src = src.take().freeze();

            Ok((self.0)(src)?.into_received())
        }
    }
}

mod tcp {
    use super::*;

    use std::{
        cmp,
        pin::Pin,
    };

    use futures::{
        future,
        stream::{
            futures_unordered::FuturesUnordered,
            Fuse,
            Stream,
            StreamFuture,
        },
        task::{
            Context,
            Poll,
        },
    };

    use pin_utils::unsafe_pinned;

    use tokio::{
        codec::{
            Decoder,
            FramedRead,
        },
        net::tcp::TcpListener,
        timer::Timeout,
    };

    pub(super) struct Server(TcpListener);

    impl Server {
        pub(super) async fn bind(addr: &SocketAddr) -> Result<Self, Error> {
            let listener = TcpListener::bind(&addr).await?;

            Ok(Server(listener))
        }

        pub(super) fn build(
            self,
            keep_alive: Duration,
            max_size_bytes: usize,
            receive: impl FnMut(Bytes) -> Result<Option<Message>, Error>
                + Send
                + Sync
                + Unpin
                + Clone
                + 'static,
        ) -> impl Stream<Item = Result<Received, Error>> {
            emit("Setting up for TCP");

            self.0
                .incoming()
                .filter_map(move |conn| {
                    match conn {
                        // The connection was successfully established
                        // Create a new protocol reader over it
                        // It'll get added to the connection pool
                        Ok(conn) => {
                            let decode = Decode::new(max_size_bytes, receive.clone());
                            let protocol = FramedRead::new(conn, decode);

                            // NOTE: The timeout stream wraps _the protocol_
                            // That means it'll close the connection if it doesn't
                            // produce a valid message within the timeframe, not just
                            // whether or not it writes to the stream
                            future::ready(Some(TimeoutStream::new(protocol, keep_alive)))
                        }
                        // The connection could not be established
                        // Just ignore it
                        Err(_) => future::ready(None),
                    }
                })
                .listen(1024)
        }
    }

    struct Listen<S>
    where
        S: Stream,
        S::Item: Stream,
    {
        accept: Fuse<S>,
        connections: FuturesUnordered<StreamFuture<S::Item>>,
        max: usize,
    }

    impl<S> Listen<S>
    where
        S: Stream,
        S::Item: Stream,
    {
        unsafe_pinned!(accept: Fuse<S>);
        unsafe_pinned!(connections: FuturesUnordered<StreamFuture<S::Item>>);
    }

    impl<S, T> Stream for Listen<S>
    where
        S: Stream + Unpin,
        S::Item: Stream<Item = Result<T, Error>> + Unpin,
    {
        type Item = Result<T, Error>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
            'poll_conns: loop {
                // Fill up our accepted connections
                'fill_conns: while self.connections.len() < self.max {
                    let conn = match self.as_mut().accept().poll_next(cx) {
                        Poll::Ready(Some(s)) => s.into_future(),
                        Poll::Ready(None) | Poll::Pending => break 'fill_conns,
                    };

                    self.connections.push(conn);
                }

                // Try polling the stream
                // NOTE: We're assuming the unordered list will
                // always make forward progress polling futures
                // even if one future is particularly chatty
                match self.as_mut().connections().poll_next(cx) {
                    // We have an item from a connection
                    Poll::Ready(Some((Some(item), conn))) => {
                        match item {
                            // A valid item was produced
                            // Return it and put the connection back in the pool.
                            Ok(item) => {
                                self.connections.push(conn.into_future());

                                return Poll::Ready(Some(Ok(item)));
                            }
                            // An error occurred, probably IO-related
                            // In this case the connection isn't returned to the pool.
                            // It's closed on drop and the error is returned.
                            Err(err) => {
                                return Poll::Ready(Some(Err(err.into())));
                            }
                        }
                    }
                    // A connection has closed
                    // Drop the connection and loop back
                    // This will mean attempting to accept a new connection
                    Poll::Ready(Some((None, _conn))) => continue 'poll_conns,
                    // The queue is empty or nothing is ready
                    Poll::Ready(None) | Poll::Pending => break 'poll_conns,
                }
            }

            // If we've gotten this far, then there are no events for us to process
            // and nothing was ready, so figure out if we're not done yet  or if
            // we've reached the end.
            if self.accept.is_done() {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        }
    }

    trait StreamListenExt: Stream {
        fn listen(self, max_connections: usize) -> Listen<Self>
        where
            Self: Sized + Unpin,
            Self::Item: Stream + Unpin,
        {
            Listen {
                accept: self.fuse(),
                connections: FuturesUnordered::new(),
                max: max_connections,
            }
        }
    }

    impl<S> StreamListenExt for S where S: Stream {}

    struct Decode<F> {
        max_size_bytes: usize,
        read_head: usize,
        discarding: bool,
        receive: F,
    }

    impl<F> Decode<F> {
        pub fn new(max_size_bytes: usize, receive: F) -> Self {
            Decode {
                read_head: 0,
                discarding: false,
                max_size_bytes,
                receive,
            }
        }
    }

    impl<F> Decoder for Decode<F>
    where
        F: FnMut(Bytes) -> Result<Option<Message>, Error>,
    {
        type Item = Received;
        type Error = Error;

        fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            'read_frame: loop {
                let read_to = cmp::min(self.max_size_bytes.saturating_add(1), src.len());

                // Messages are separated by null bytes
                let sep_offset = src[self.read_head..].iter().position(|b| *b == b'\0');

                match (self.discarding, sep_offset) {
                    // A delimiter was found
                    // Split it from the buffer and return
                    (false, Some(offset)) => {
                        let frame_end = offset + self.read_head;

                        // The message is technically sitting right there
                        // for us, but since it's bigger than our max capacity
                        // we still discard it
                        if frame_end > self.max_size_bytes {
                            increment!(server.tcp_msg_overflow);

                            self.discarding = true;

                            continue 'read_frame;
                        }

                        self.read_head = 0;
                        let src = src.split_to(frame_end + 1).freeze();

                        return Ok((self.receive)(src.slice_to(src.len() - 1))?.into_received());
                    }
                    // A delimiter wasn't found, but the incomplete
                    // message is too big. Start discarding the input
                    (false, None) if src.len() > self.max_size_bytes => {
                        increment!(server.tcp_msg_overflow);

                        self.discarding = true;

                        continue 'read_frame;
                    }
                    // A delimiter wasn't found
                    // Move the read head forward so we'll check
                    // from that position next time data arrives
                    (false, None) => {
                        self.read_head = read_to;

                        // As per the contract of `Decoder`, we return `None`
                        // here to indicate more data is needed to complete a frame
                        return Ok(None);
                    }
                    // We're discarding input and have reached the end of the message
                    // Advance the source buffer to the end of that message and try again
                    (true, Some(offset)) => {
                        src.advance(offset + self.read_head + 1);
                        self.discarding = false;
                        self.read_head = 0;

                        continue 'read_frame;
                    }
                    // We're discarding input but haven't reached the end of the message yet
                    (true, None) => {
                        src.advance(read_to);
                        self.read_head = 0;

                        if src.is_empty() {
                            // We still return `Ok` here, even though we have no intention
                            // of processing those bytes. Our maximum buffer size should still
                            // be limited by the initial capacity, since we're responsible for
                            // reserving additional capacity and aren't doing that
                            return Ok(None);
                        }

                        continue 'read_frame;
                    }
                }
            }
        }

        fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            Ok(match self.decode(src)? {
                Some(frame) => Some(frame),
                None => {
                    if src.is_empty() {
                        None
                    } else {
                        let src = src.take().freeze();
                        self.read_head = 0;

                        (self.receive)(src)?.into_received()
                    }
                }
            })
        }
    }

    struct TimeoutStream<S> {
        stream: Timeout<S>,
    }

    impl<S> TimeoutStream<S>
    where
        S: Stream,
    {
        fn new(stream: S, keep_alive: Duration) -> Self {
            increment!(server.tcp_conn_accept);

            TimeoutStream {
                stream: Timeout::new(stream, keep_alive),
            }
        }
    }

    impl<S> Drop for TimeoutStream<S> {
        fn drop(&mut self) {
            increment!(server.tcp_conn_close);
        }
    }

    impl<S> TimeoutStream<S> {
        unsafe_pinned!(stream: Timeout<S>);
    }

    impl<S> Stream for TimeoutStream<S>
    where
        S: Stream,
    {
        type Item = S::Item;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
            match self.stream().poll_next(cx) {
                // The timeout has elapsed
                Poll::Ready(Some(Err(_))) => {
                    increment!(server.tcp_conn_timeout);

                    Poll::Ready(None)
                }
                // The stream has produced an item
                Poll::Ready(Some(Ok(item))) => Poll::Ready(Some(item)),
                // The stream has completed
                Poll::Ready(None) => Poll::Ready(None),
                // The timeout hasn't elapsed and the stream hasn't produced an item
                Poll::Pending => Poll::Pending,
            }
        }
    }
}
