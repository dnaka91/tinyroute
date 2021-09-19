use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use log::error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time;

use crate::agent::{Agent, Message};
use crate::errors::Result;
use crate::frame::{Frame, FrameOutput, FramedMessage};

mod tcp;

use crate::router::{RouterMessage, RouterTx, ToAddress};
pub use tcp::TcpListener;

#[cfg(target_os = "linux")]
mod uds;
#[cfg(target_os = "linux")]
pub use uds::UdsListener;

/// Client payload.
/// Access the bytes through `self.data()`
#[derive(Debug, Clone)]
pub(crate) struct Payload {
    inner: Vec<u8>,
    offset: usize,
}

impl Payload {
    pub(crate) fn new(offset: usize, inner: Vec<u8>) -> Self {
        Self { inner, offset }
    }

    /// Access the inner data of the payload.
    /// This is the data void of the address
    pub(crate) fn data(&self) -> &[u8] {
        &self.inner[self.offset..]
    }
}

/// Some kind of listener
pub trait Listener: Sync {
    /// The reading half of the connection
    type Reader: AsyncRead + Unpin + Send + 'static;
    /// The writing half of the connection
    type Writer: AsyncWrite + Unpin + Send + 'static;

    // Accepts &self as arg
    // Returns a pinned boxed future, where
    // * any reference has to live for at least as long as &self,
    // * and it has to be valid to send this across thread boundaries
    //
    // We need the `Send` part because tokio::spawn might put this on another thread.
    // We need the life time because the thing we return can not hold a reference to
    // anything on &self that might be dropped before self.
    /// Accept incoming connections
    fn accept(&mut self) -> ServerFuture<'_, Self::Reader, Self::Writer>;
}

/// Because writing this entire trait malarkey is messy!
pub type ServerFuture<'a, T, U> = Pin<Box<dyn Future<Output = Result<(T, U)>> + Send + 'a>>;

/// Accept incoming connections and provide agents as an abstraction.
///
/// ```
/// use tinyroute::server::{Server, TcpListener};
///
///
/// #[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
/// struct Address(usize); 
/// # impl tinyroute::ToAddress for Address {
/// #   fn from_bytes(_: &[u8]) -> Option<Self> { None }
/// # }
/// # async fn run(router: tinyroute::Router<Address>) {
/// let tcp_listener = TcpListener::bind("127.0.0.1:5000").await.unwrap();
/// let mut server = Server::new(tcp_listener);
/// let mut id = 0;
/// while let Some(connection) = server.next(
///     router.router_tx(),
///     Address(id), 
///     None, 
///     1024
/// ).await {
///     id += 1;
/// }
/// # }
/// ```
pub struct Server<L: Listener> {
    server: L,
}

impl<L: Listener> Server<L> {
    pub fn new(server: L) -> Self {
        Self { server }
    }

    pub async fn next<A: Sync + ToAddress>(
        &mut self,
        router_tx: RouterTx<A>,
        address: A,
        timeout: Option<Duration>,
        cap: usize,
    ) -> Option<Connection<A, <L as Listener>::Writer>> {
        let (reader, writer) = self.server.accept().await.ok()?;

        // Register the agent
        let (transport_tx, transport_rx) = mpsc::channel(cap);
        router_tx.register_agent(address.clone(), transport_tx).ok()?;
        let agent =
            Agent::new(router_tx.clone(), address.clone(), transport_rx);

        // Spawn the reader
        tokio::spawn(spawn_reader(reader, address, router_tx, timeout));

        Some(Connection::new(agent, writer))
    }
}

async fn spawn_reader<A, R>(
    mut reader: R,
    sender: A,
    router_tx: RouterTx<A>,
    timeout: Option<Duration>,
) where
    R: AsyncRead + Unpin,
    A: ToAddress,
{
    let mut frame = Frame::empty();
    loop {
        let read = async {
            let res = frame.async_read(&mut reader).await;

            'msg: loop {
                match res {
                    Ok(0) => break 'msg false,
                    Ok(_) => {
                        match frame.try_msg() {
                            Ok(Some(FrameOutput::Heartbeat)) => continue,
                            Ok(Some(FrameOutput::Message(msg))) => {
                                let address = msg
                                    .iter()
                                    .cloned()
                                    .take_while(|b| (*b as char) != '|')
                                    .collect::<Vec<u8>>();

                                // return in the event of the index being
                                // larger than the payload it self
                                let index = address.len() + 1;
                                if index >= msg.len() {
                                    return true;
                                }

                                let address = match A::from_bytes(&address) {
                                    Some(a) => a,
                                    None => break 'msg true,
                                };

                                let payload = Payload::new(index, msg);
                                let bytes =
                                    Bytes::from(payload.data().to_vec());

                                match router_tx.send(
                                    RouterMessage::RemoteMessage {
                                        bytes,
                                        sender: sender.clone(),
                                        recipient: address,
                                    },
                                ) {
                                    Ok(_) => continue,
                                    Err(e) => {
                                        error!("failed to send message to router: {:?}", e);
                                        break 'msg false
                                    }
                                }
                            }
                            Ok(None) => break 'msg true,
                            Err(e) => {
                                error!("invalid payload. {:?}", e);
                                break 'msg false
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            "failed to read from the socket. reason: {:?}",
                            e
                        );
                        break 'msg false
                    }
                }
            }
        };

        let restart = match timeout {
            Some(timeout) => {
                tokio::select! {
                    _ = time::sleep(timeout) => true,
                    restart = read => { restart }
                }
            }
            None => read.await,
        };

        if !restart {
            break;
        }
    }

    // Shutdown the agent
    if let Err(e) = router_tx.send(RouterMessage::Shutdown(sender)) {
        error!("failed to shutdown agent: {:?}", e);
    }
}

pub struct Connection<A, W>
where
    A: ToAddress,
    W: AsyncWrite,
{
    agent: Agent<FramedMessage, A>,
    writer: W,
}

impl<A, W> Connection<A, W>
where
    A: ToAddress,
    W: AsyncWrite + Unpin,
{
    pub fn new(agent: Agent<FramedMessage, A>, writer: W) -> Self {
        Self { agent, writer }
    }

    pub async fn recv(&mut self) -> Result<()> {
        if let Message::Value(framed_message, _) = self.agent.recv().await? {
            self.writer.write_all(&framed_message.0).await?;
        }
        Ok(())
    }
}
