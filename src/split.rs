use std::io::Result;
use std::sync::Arc;
use std::task::Context;
use std::{pin::Pin, task::Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::*;

pub struct ReadHalfSctp {
    inner: Arc<SctpSocketTokio>,
}
pub struct WriteHalfSctp {
    inner: Arc<SctpSocketTokio>,
}

pub fn split(s: SctpSocketTokio) -> (ReadHalfSctp, WriteHalfSctp) {
    let inner = Arc::new(s);
    (
        ReadHalfSctp {
            inner: inner.clone(),
        },
        WriteHalfSctp { inner },
    )
}

impl ReadHalfSctp {
    pub async fn recvmsg(&self, data: &mut [u8]) -> Result<usize> {
        self.inner.recvmsg(data).await
    }
}

impl WriteHalfSctp {
    pub async fn sendmsg(&self, data: &[u8], info: &SndInfo) -> Result<usize> {
        self.inner.sendmsg(data, info).await
    }
}

impl AsyncRead for ReadHalfSctp {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf,
    ) -> Poll<Result<()>> {
        self.inner.poll_read_priv(cx, buf)
    }
}
impl<'a> AsyncRead for &'a ReadHalfSctp {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf,
    ) -> Poll<Result<()>> {
        self.inner.poll_read_priv(cx, buf)
    }
}
impl AsyncWrite for WriteHalfSctp {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::prelude::v1::Result<usize, std::io::Error>> {
        self.inner.poll_write_priv(cx, buf)
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
        self.inner
            .afd
            .get_ref()
            .shutdown(std::net::Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}
