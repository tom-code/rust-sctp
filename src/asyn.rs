use std::io::Result;
use std::{
    io::{Error, Read, Write},
    pin::Pin,
    task::{Context, Poll},
};

use crate::*;
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, Interest, ReadBuf};

#[derive(Debug)]
pub struct SctpSocketTokio {
    pub(crate) afd: AsyncFd<SctpSocket>,
}

impl AsyncRead for SctpSocketTokio {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf,
    ) -> Poll<Result<()>> {
        self.poll_read_priv(cx, buf)
    }
}

impl AsyncRead for &SctpSocketTokio {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf,
    ) -> Poll<Result<()>> {
        self.poll_read_priv(cx, buf)
    }
}

impl AsyncWrite for SctpSocketTokio {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::prelude::v1::Result<usize, std::io::Error>> {
        self.poll_write_priv(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::prelude::v1::Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::prelude::v1::Result<(), std::io::Error>> {
        self.afd.get_ref().shutdown(std::net::Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}

impl SctpSocketTokio {
    pub fn new() -> Result<Self> {
        let fdx = SctpSocket::new()?;
        fdx.set_noblock()?;
        let afd = AsyncFd::new(fdx)?;
        Ok(Self { afd })
    }
    pub fn new6() -> Result<Self> {
        let fdx = SctpSocket::new6()?;
        fdx.set_noblock()?;
        let afd = AsyncFd::new(fdx)?;
        Ok(Self { afd })
    }

    pub fn bind(&self, address: std::net::SocketAddr) -> Result<()> {
        self.afd.get_ref().bind(address)
    }
    pub fn bindx(&self, addresses: &[std::net::SocketAddr]) -> Result<()> {
        self.afd.get_ref().bindx(addresses)
    }

    /// Connects to `address` and returns the connected socket.
    ///
    /// The socket is consumed: if the connect fails or the future is dropped, the
    /// socket is closed, and a retry needs a new one. tokio can't reset the
    /// write-closed readiness a failed connect leaves on a socket.
    pub async fn connect(self, address: std::net::SocketAddr) -> Result<Self> {
        if let Err(e) = self.afd.get_ref().connect(address) {
            if e.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(e);
            }
        }
        self.wait_connected().await?;
        Ok(self)
    }

    /// Connects to a peer with several addresses and returns the connected socket.
    /// The socket is consumed, see [`SctpSocketTokio::connect`].
    pub async fn connectx(self, addresses: &[std::net::SocketAddr]) -> Result<Self> {
        if let Err(e) = self.afd.get_ref().connectx(addresses) {
            if e.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(e);
            }
        }
        self.wait_connected().await?;
        Ok(self)
    }

    async fn wait_connected(&self) -> Result<()> {
        loop {
            // A failed connect can report EPOLLERR|EPOLLIN when an event is queued
            // (e.g. SCTP_ASSOC_CHANGE), which writable() alone would never see.
            let mut guard = self.afd.ready(Interest::WRITABLE | Interest::ERROR).await?;
            if let Some(e) = self.afd.get_ref().take_error()? {
                return Err(e);
            }
            match self.afd.get_ref().getpeername() {
                Ok(_) => return Ok(()),
                Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => {
                    // closed readiness can't be cleared, so waiting again would spin
                    if guard.ready().is_write_closed() {
                        return Err(e);
                    }
                    guard.clear_ready();
                }
                Err(e) => return Err(e),
            }
        }
    }
    /// Sends the selected kernel events to `tx`. See [`SctpSocket::subscribe`];
    /// call it before the socket is shared, and on each accepted socket.
    pub fn subscribe(
        &mut self,
        events: Events,
        tx: tokio::sync::mpsc::Sender<Notification>,
    ) -> Result<()> {
        self.afd.get_mut().subscribe(events, tx)
    }
    /// How many notifications were lost because the channel was full.
    pub fn dropped_notifications(&self) -> u64 {
        self.afd.get_ref().dropped_notifications()
    }

    /// Sends one message; `info.snd_ppid` is in host order. Errors are returned
    /// as is and leave the association alone; the kernel reports its state on
    /// later calls.
    pub async fn sendmsg(&self, data: &[u8], info: &SndInfo) -> Result<usize> {
        loop {
            let mut guard = self.afd.writable().await?;
            let res = self.afd.get_ref().sendmsg(data, info);
            if let Err(e) = &res {
                if e.raw_os_error().is_some_and(|f| f == libc::EWOULDBLOCK) {
                    guard.clear_ready();
                    continue;
                }
            }
            break res;
        }
    }

    pub async fn sendmsg_def(&self, data: &[u8]) -> Result<usize> {
        let info = SndInfo {
            snd_ppid: self.afd.get_ref().ppid,
            ..Default::default()
        };
        self.sendmsg(data, &info).await
    }

    pub async fn recvmsg(&self, data: &mut [u8]) -> Result<usize> {
        loop {
            let mut guard = self.afd.readable().await?;
            let res = self.afd.get_ref().recvmsg(data);
            if let Err(e) = &res {
                if e.raw_os_error().is_some_and(|f| f == libc::EWOULDBLOCK) {
                    guard.clear_ready();
                    continue;
                }
            }
            break res;
        }
    }
    /// Receives one complete SCTP message into `msg`, reading until MSG_EOR.
    /// Returns the message length, or 0 when the peer has shut down.
    ///
    /// Cancel safe when the same `msg` is passed again: if the future is
    /// dropped mid-message, the bytes read so far stay in `msg` and the next
    /// call appends the rest. Otherwise `msg` is cleared first. This also
    /// applies after another receive method stopped mid-message.
    pub async fn recvmsg_full(&self, msg: &mut Vec<u8>) -> Result<usize> {
        const MIN_READ: usize = 4096;
        if !self.afd.get_ref().in_partial_message() {
            msg.clear();
        }
        loop {
            let mut guard = self.afd.readable().await?;
            // grow msg only after the await, so a cancelled call leaves no padding in it
            let start = msg.len();
            if msg.capacity() - start < MIN_READ {
                msg.reserve(MIN_READ.max(start));
            }
            msg.resize(msg.capacity(), 0);
            let res = self.afd.get_ref().recvmsg_eor(&mut msg[start..]);
            match res {
                Ok((n, eor)) => {
                    msg.truncate(start + n);
                    if n == 0 {
                        if start == 0 {
                            return Ok(0);
                        }
                        return Err(Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "connection closed mid-message",
                        ));
                    }
                    if eor {
                        return Ok(msg.len());
                    }
                }
                Err(e) => {
                    msg.truncate(start);
                    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                        guard.clear_ready();
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
    /// Receives user data and reports its stream, PPID and so on. See
    /// [`SctpSocket::recvmsg_detailed`].
    pub async fn recvmsg_detailed(&self, data: &mut [u8]) -> Result<RecvInfo> {
        loop {
            let mut guard = self.afd.readable().await?;
            match self.afd.get_ref().recvmsg_detailed_once(data) {
                Ok(Some(info)) => return Ok(info),
                // notification consumed; more data may already be queued, so keep readiness
                Ok(None) => continue,
                Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => guard.clear_ready(),
                Err(e) => return Err(e),
            }
        }
    }
    pub async fn accept(&self) -> Result<SctpSocketTokio> {
        loop {
            let ret = self.afd.get_ref().accept();
            match ret {
                Ok(new) => {
                    new.set_noblock()?;
                    let afd = AsyncFd::new(new)?;
                    return Ok(SctpSocketTokio { afd });
                }
                Err(e) => match e.raw_os_error() {
                    Some(libc::EWOULDBLOCK) => {
                        self.afd.readable().await?.clear_ready();
                    }
                    _ => return Err(e),
                },
            };
        }
    }
    pub fn listen(&self, backlog: i32) -> Result<()> {
        self.afd.get_ref().listen(backlog)
    }
    pub fn set_init(&self, info: Init) -> Result<()> {
        self.afd.get_ref().set_init(info)
    }
    pub fn set_rto(&self, info: RtoInfo) -> Result<()> {
        self.afd.get_ref().set_rto(info)
    }
    pub fn set_nodelay(&self, enable: bool) -> Result<()> {
        self.afd.get_ref().set_nodelay(enable)
    }
    /// See [`SctpSocket::set_primary_addr`].
    pub fn set_primary_addr(&self, addr: std::net::SocketAddr) -> Result<()> {
        self.afd.get_ref().set_primary_addr(addr)
    }
    pub fn primary_addr(&self) -> Result<std::net::SocketAddr> {
        self.afd.get_ref().primary_addr()
    }
    pub fn set_rcvbuf(&self, size: i32) -> Result<()> {
        self.afd.get_ref().set_rcvbuf(size)
    }
    pub fn getsocketname(&self) -> Result<std::net::SocketAddr> {
        self.afd.get_ref().getsocketname()
    }
    pub fn getpeername(&self) -> Result<std::net::SocketAddr> {
        self.afd.get_ref().getpeername()
    }

    pub fn shutdown(&self, how: std::net::Shutdown) -> Result<()> {
        self.afd.get_ref().shutdown(how)
    }

    pub(crate) fn poll_read_priv(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf,
    ) -> Poll<Result<()>> {
        loop {
            //let mut guard = ready!(self.afd.poll_read_ready(cx))?;
            let mut guard = match self.afd.poll_read_ready(cx) {
                core::task::Poll::Ready(t) => t,
                core::task::Poll::Pending => return core::task::Poll::Pending,
            }?;
            match guard.try_io(|inner| inner.get_ref().read(buf.initialize_unfilled())) {
                Ok(result) => {
                    buf.advance(result?);
                    return Poll::Ready(Ok(()));
                }
                Err(_would_block) => continue,
            }
        }
    }

    pub(crate) fn poll_write_priv(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::prelude::v1::Result<usize, std::io::Error>> {
        loop {
            let mut guard = match self.afd.poll_write_ready(cx) {
                core::task::Poll::Ready(t) => t,
                core::task::Poll::Pending => return core::task::Poll::Pending,
            }?;

            match guard.try_io(|inner| inner.get_ref().write(buf)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }
    /// Sets the PPID that `sendmsg_def` and `AsyncWrite` send, in host order.
    pub fn set_ppid(&mut self, ppid: u32) {
        self.afd.get_mut().set_ppid(ppid)
    }
}
