use std::io::Result;
use std::{
    io::{Error, Read, Write},
    os::fd::AsRawFd,
    pin::Pin,
    task::{Context, Poll},
};

use crate::*;
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, ReadBuf};

#[derive(Debug)]
pub struct SctpSocketTokio {
    pub(crate) afd: AsyncFd<SctpSocket>,
    failed: std::sync::atomic::AtomicBool
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

impl<'a> AsyncRead for &'a SctpSocketTokio {
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
        let afd = AsyncFd::new(fdx)?;
        Ok(Self { afd, failed: std::sync::atomic::AtomicBool::new(false) })
    }
    pub fn new6() -> Result<Self> {
        let fdx = SctpSocket::new6()?;
        let afd = AsyncFd::new(fdx)?;
        Ok(Self { afd, failed: std::sync::atomic::AtomicBool::new(false) })
    }

    pub fn bind(&self, address: std::net::SocketAddr) -> Result<()> {
        self.afd.get_ref().bind(address)
    }
    pub fn bindx(&self, addresses: &[std::net::SocketAddr]) -> Result<()> {
        self.afd.get_ref().bindx(addresses)
    }

    pub async fn connect(&self, address: std::net::SocketAddr) -> Result<()> {
        //self.afd.get_ref().subscribe_aux()?;
        self.afd.get_ref().subscribe_addr()?;
        self.afd.get_ref().set_noblock()?;

        if let Err(e) = self.afd.get_ref().connect(address) {
            if !e.raw_os_error().is_some_and(|e| e == libc::EINPROGRESS) {
                return Err(e);
            }
        }
        let _guard = self.afd.writable().await?;

        let (so_error, getsockopt_result) = unsafe {
            let err: libc::c_int = 0;
            let len: libc::socklen_t = 4;
            let res = libc::getsockopt(
                self.afd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &err as *const libc::c_int as *mut libc::c_void,
                &len as *const libc::socklen_t as *mut libc::socklen_t,
            );
            (err, res)
        };
        if getsockopt_result != 0 {
            return Err(Error::last_os_error());
        }
        if so_error != 0 {
            Err(Error::from_raw_os_error(so_error))
        } else {
            Ok(())
        }
    }

    pub async fn connectx(&self, addresses: &[std::net::SocketAddr]) -> Result<()> {
        self.afd.get_ref().set_noblock()?;

        if let Err(e) = self.afd.get_ref().connectx(addresses) {
            if !e.raw_os_error().is_some_and(|e| e == libc::EINPROGRESS) {
                return Err(e);
            }
        }
        let _guard = self.afd.writable().await?;

        let (so_error, getsockopt_result) = unsafe {
            let err: libc::c_int = 0;
            let len: libc::socklen_t = 4;
            let res = libc::getsockopt(
                self.afd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &err as *const libc::c_int as *mut libc::c_void,
                &len as *const libc::socklen_t as *mut libc::socklen_t,
            );
            (err, res)
        };
        if getsockopt_result != 0 {
            return Err(Error::last_os_error());
        }
        if so_error != 0 {
            Err(Error::from_raw_os_error(so_error))
        } else {
            Ok(())
        }
    }
    pub fn subscribe_notifications(&mut self, s: tokio::sync::mpsc::Sender<sctp::Notification>) {
        self.afd.get_mut().subscribe_notifications(s);
    }

    pub async fn sendmsg(&self, data: &[u8], info: &SndInfo) -> Result<usize> {
        loop {
            let mut guard = self.afd.writable().await?;
            let res = self.afd.get_ref().sendmsg(data, info);
            if let Err(e) = &res {
                if e.raw_os_error().is_some_and(|f| f == libc::EWOULDBLOCK) {
                    guard.clear_ready();
                    continue;
                } else {
                    self.failed.store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = self.afd.get_ref().shutdown(std::net::Shutdown::Both);
                }
            }
            break res;
        }
    }

    pub async fn sendmsg_def(&self, data: &[u8]) -> Result<usize> {
        let info = SndInfo {
            snd_sid: 0,
            snd_flags: 0,
            snd_ppid: self.afd.get_ref().ppid.to_be(),
            snd_context: 0,
            snd_assoc_id: 0,
        };
        loop {
            let mut guard = self.afd.writable().await?;
            let res = self.afd.get_ref().sendmsg(data, &info);
            if let Err(e) = &res {
                if e.raw_os_error().is_some_and(|f| f == libc::EWOULDBLOCK) {
                    guard.clear_ready();
                    continue;
                } else {
                    self.failed.store(true, std::sync::atomic::Ordering::SeqCst);
                    println!("set to failed");
                    let _ = self.afd.get_ref().shutdown(std::net::Shutdown::Both);
                }
            }
            break res;
        }
    }

    pub async fn recvmsg(&self, data: &mut [u8]) -> Result<usize> {
        loop {
            if self.failed.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(0)
            }
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
    pub async fn recvmsg_detailed(&self, data: &mut [u8], info: &mut SndRcvInfo) -> Result<usize> {
        loop {
            if self.failed.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(0)
            }
            let mut guard = self.afd.readable().await?;
            let res = self.afd.get_ref().recvmsg_detailed(data, info);
            if let Err(e) = &res {
                if e.raw_os_error().is_some_and(|f| f == libc::EWOULDBLOCK) {
                    guard.clear_ready();
                    continue;
                }
            }
            break res;
        }
    }
    pub async fn accept(&self) -> Result<SctpSocketTokio> {
        self.afd.get_ref().set_noblock()?;
        loop {
            let ret = self.afd.get_ref().accept();
            match ret {
                Ok(new) => {
                    let afd = AsyncFd::new(new)?;
                    afd.get_ref().set_noblock()?;
                    return Ok(SctpSocketTokio { afd, failed: std::sync::atomic::AtomicBool::new(false) });
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
    pub fn subscribe_aux(&self) -> Result<()> {
        self.afd.get_ref().subscribe_aux()
    }
    pub fn subscribe_addr(&self) -> Result<()> {
        self.afd.get_ref().subscribe_addr()
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
    pub fn set_rcvbuf(&self, size: i32) -> Result<()> {
        self.afd.get_ref().set_rcvbuf(size)
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
    pub fn set_ppid(&mut self, ppid: u32) {
        self.afd.get_mut().set_ppid(ppid)
    }
}
