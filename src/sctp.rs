use std::io::{Error, Result};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use os_socketaddr::OsSocketAddr;

#[derive(Debug, Default)]
struct PendingNotification {
    buf: Vec<u8>,
    overflow: bool,
}

// Largest notification that is reassembled from pieces; bigger ones are dropped.
const MAX_NOTIFICATION: usize = 4096;

#[derive(Debug)]
pub struct SctpSocket {
    fd: RawFd,
    pub(crate) ppid: u32,
    notification_queue: Option<tokio::sync::mpsc::Sender<Notification>>,
    // pieces of a notification larger than the read buffer, collected until MSG_EOR
    notification_pending: std::sync::Mutex<PendingNotification>,
    // set when the last data read ended without MSG_EOR, i.e. mid-message
    data_partial: AtomicBool,
    // notifications lost because the channel was full
    notifications_dropped: AtomicU64,
}

const SOL_SCTP: libc::c_int = 132;

// setsockopt
const SCTP_SOCKOPT_BINDX_ADD: libc::c_int = 100;
const SCTP_SOCKOPT_CONNECTX: libc::c_int = 110;
const SCTP_EVENTS: libc::c_int = 11;
const SCTP_RTOINFO: i32 = 0;
const SCTP_INIT_MSG: i32 = 2;
const SCTP_PRIMARY_ADDR: libc::c_int = 6;

// aux info
//const SCTP_INIT: i32 = 0;
const SCTP_SNDRCV: i32 = 1;
const SCTP_SNDINFO: i32 = 2;
//const SCTP_RCVINFO: i32 = 3;
//const SCTP_NXTINFO: i32 = 4;
//const SCTP_PRINFO: i32 = 5;
//const SCTP_DSTADDRV4: i32 = 7;

const SCTP_NODELAY: i32 = 3;

// msg_flags bit set by the kernel when the received message is an SCTP notification
const MSG_NOTIFICATION: libc::c_int = 0x8000;
const SCTP_ASSOC_CHANGE: u16 = 0x8001;
const SCTP_PEER_ADDR_CHANGE: u16 = 0x8002;
const SCTP_SHUTDOWN_EVENT: u16 = 0x8005;

/// Send a message unordered (`snd_flags`); also reported as [`RecvInfo::unordered`].
pub const SCTP_UNORDERED: u16 = 1;

// struct sctp_sndrcvinfo as the kernel fills it in: sinfo_ppid is in network order
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SndRcvInfo {
    sinfo_stream: u16,
    sinfo_ssn: u16,
    sinfo_flags: u16,
    sinfo_ppid: u32,
    sinfo_context: u32,
    sinfo_timetolive: u32,
    sinfo_tsn: u32,
    sinfo_cumtsn: u32,
    sinfo_assoc_id: i32,
}

/// What [`SctpSocket::recvmsg_detailed`] received.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecvInfo {
    /// Bytes written to the buffer. 0 means the peer closed, or the buffer was
    /// empty; all other fields are then zero.
    pub len: usize,
    pub stream: u16,
    pub ssn: u16,
    /// Payload protocol identifier, in host order.
    pub ppid: u32,
    pub unordered: bool,
    /// This read ends a message. False means the rest follows in later reads.
    pub eor: bool,
    pub tsn: u32,
    pub cumtsn: u32,
    pub context: u32,
}

/// Converts what one read returned to the public form, with the PPID in host order.
fn recv_info(len: usize, msg_flags: libc::c_int, raw: Option<SndRcvInfo>) -> Result<RecvInfo> {
    if len == 0 {
        return Ok(RecvInfo::default());
    }
    // every socket enables this info when it is created, so only a change made
    // through the raw fd gets here
    let raw = raw.ok_or_else(|| {
        Error::new(
            std::io::ErrorKind::InvalidData,
            "SCTP_SNDRCV info missing: sctp_data_io_event was disabled on this socket",
        )
    })?;
    Ok(RecvInfo {
        len,
        stream: raw.sinfo_stream,
        ssn: raw.sinfo_ssn,
        ppid: u32::from_be(raw.sinfo_ppid),
        unordered: raw.sinfo_flags & SCTP_UNORDERED != 0,
        eor: msg_flags & libc::MSG_EOR != 0,
        tsn: raw.sinfo_tsn,
        cumtsn: raw.sinfo_cumtsn,
        context: raw.sinfo_context,
    })
}

// One recvmsg of user data.
struct Received {
    len: usize,
    msg_flags: libc::c_int,
    info: Option<SndRcvInfo>,
}

/// Send parameters. All values are in host order, including `snd_ppid`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SndInfo {
    pub snd_sid: u16,
    pub snd_flags: u16,
    /// Payload protocol identifier, in host order.
    pub snd_ppid: u32,
    pub snd_context: u32,
    pub snd_assoc_id: i32,
}

#[repr(C)]
#[derive(Debug)]
pub struct RcvInfo {
    pub rcv_sid: u16,
    pub rcv_ssn: u16,
    pub rcv_flags: u16,
    pub rcv_ppid: u32,
    pub rcv_tsn: u32,
    pub rcv_cumtsn: u32,
    pub rcv_context: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct RtoInfo {
    pub assoc_id: u32,
    pub srto_initial: u32,
    pub srto_max: u32,
    pub srto_min: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct Init {
    pub sinit_num_ostreams: u16,
    pub sinit_max_instreams: u16,
    pub sinit_max_attempts: u16,
    pub sinit_max_init_timeo: u16,
}

#[repr(C)]
#[derive(Debug, Default)]
struct SctpEventSubscribe {
    sctp_data_io_event: u8,
    sctp_association_event: u8,
    sctp_address_event: u8,
    sctp_send_failure_event: u8,
    sctp_peer_error_event: u8,
    sctp_shutdown_event: u8,
    sctp_partial_delivery_event: u8,
    sctp_adaptation_layer_event: u8,
    sctp_authentication_event: u8,
    sctp_sender_dry_event: u8,
    sctp_stream_reset_event: u8,
    sctp_assoc_reset_event: u8,
    sctp_stream_change_event: u8,
    sctp_send_failure_event_event: u8,
}

#[repr(C)]
#[derive(Debug)]
struct NotificationHeader {
    sn_type: u16,
    sn_flags: u16,
    sn_length: u32,
}

// struct sctp_paddr_change, without the trailing spc_assoc_id
#[repr(C)]
#[derive(Debug)]
struct IPaddrChange {
    spc_type: u16,
    spc_flags: u16,
    spc_length: u32,
    spc_addr: [u8; 128],
    spc_state: u32,
    spc_error: u32,
}

// struct sctp_assoc_change, without the variable-length sac_info
#[repr(C)]
#[derive(Debug)]
struct RawAssocChange {
    sac_type: u16,
    sac_flags: u16,
    sac_length: u32,
    sac_state: u16,
    sac_error: u16,
    sac_outbound_streams: u16,
    sac_inbound_streams: u16,
    sac_assoc_id: i32,
}

// struct sctp_shutdown_event
#[repr(C)]
#[derive(Debug)]
struct RawShutdownEvent {
    sse_type: u16,
    sse_flags: u16,
    sse_length: u32,
    sse_assoc_id: i32,
}

// struct sctp_prim: packed, aligned(4), so the address is kept as bytes
#[repr(C)]
struct Prim {
    ssp_assoc_id: i32,
    ssp_addr: [u8; 128],
}

#[repr(C)]
#[derive(Debug)]
pub struct Status {
    pub sstat_assoc_id: u32,
    pub sstat_state: i32,
    pub sstat_rwnd: u32,
    pub sstat_unackdata: u16,
    pub sstat_penddata: u16,
    pub sstat_instrms: u16,
    pub sstat_outstrms: u16,
    pub sstat_fragmentation_point: u32,
    //pub sstat_primary: Struct_sctp_paddrinfo,
}

/// The events to receive, see [`SctpSocket::subscribe`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Events {
    /// Association state changes, [`Notification::AssocChange`].
    pub assoc: bool,
    /// Peer address state changes, [`Notification::PeerAddrChange`].
    pub peer_addr: bool,
    /// The peer started a shutdown, [`Notification::Shutdown`].
    pub shutdown: bool,
}

impl Events {
    pub fn all() -> Self {
        Events {
            assoc: true,
            peer_addr: true,
            shutdown: true,
        }
    }
}

/// An event reported by the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notification {
    AssocChange(AssocChange),
    PeerAddrChange(PeerAddrChange),
    /// The peer sent SHUTDOWN. It accepts no more data; a local shutdown
    /// doesn't produce this event.
    Shutdown,
}

/// SCTP_ASSOC_CHANGE: the association changed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssocChange {
    pub state: AssocState,
    /// `sac_error`: the error cause, if any.
    pub error: u16,
    pub outbound_streams: u16,
    pub inbound_streams: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssocState {
    /// The association is up.
    CommUp,
    /// The association failed: the peer aborted, or it can't be reached.
    CommLost,
    /// The peer restarted. The association stays up, with a new peer instance.
    Restart,
    /// A graceful shutdown finished.
    ShutdownComplete,
    /// The association couldn't be set up.
    CantStartAssoc,
    /// A state this crate doesn't know.
    Other(u16),
}

impl AssocState {
    fn from_raw(state: u16) -> Self {
        match state {
            0 => AssocState::CommUp,
            1 => AssocState::CommLost,
            2 => AssocState::Restart,
            3 => AssocState::ShutdownComplete,
            4 => AssocState::CantStartAssoc,
            other => AssocState::Other(other),
        }
    }
}

/// SCTP_PEER_ADDR_CHANGE: a peer address changed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerAddrChange {
    /// The peer address whose state changed.
    pub addr: std::net::SocketAddr,
    pub state: PeerAddrState,
    /// `spc_error`: the error cause, if any.
    pub error: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAddrState {
    Available,
    Unreachable,
    Removed,
    Added,
    MadePrimary,
    Confirmed,
    PotentiallyFailed,
    /// A state this crate doesn't know.
    Other(u32),
}

impl PeerAddrState {
    fn from_raw(state: u32) -> Self {
        match state {
            0 => PeerAddrState::Available,
            1 => PeerAddrState::Unreachable,
            2 => PeerAddrState::Removed,
            3 => PeerAddrState::Added,
            4 => PeerAddrState::MadePrimary,
            5 => PeerAddrState::Confirmed,
            6 => PeerAddrState::PotentiallyFailed,
            other => PeerAddrState::Other(other),
        }
    }
}

fn handle_libc_error(err: i32) -> Result<()> {
    if err == -1 {
        Err(Error::last_os_error())
    } else {
        Ok(())
    }
}

impl SctpSocket {
    fn from_fd(fd: RawFd) -> Self {
        Self {
            fd,
            ppid: 0,
            notification_queue: None,
            notification_pending: Default::default(),
            data_partial: AtomicBool::new(false),
            notifications_dropped: AtomicU64::new(0),
        }
    }
    fn with_domain(domain: libc::c_int) -> Result<Self> {
        let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, libc::IPPROTO_SCTP) };
        if fd == -1 {
            return Err(Error::last_os_error());
        }
        // from here on Drop closes the fd, also if set_events fails
        let sock = Self::from_fd(fd);
        sock.set_events(Events::default())?;
        Ok(sock)
    }
    pub fn new() -> Result<Self> {
        Self::with_domain(libc::PF_INET)
    }
    pub fn new6() -> Result<Self> {
        Self::with_domain(libc::PF_INET6)
    }
    /// Sets the PPID that `sendmsg_default` and `Write` send, in host order.
    pub fn set_ppid(&mut self, ppid: u32) {
        self.ppid = ppid;
    }
    fn setsockopt<T>(&self, level: libc::c_int, option: libc::c_int, value: T) -> Result<()> {
        handle_libc_error(unsafe {
            libc::setsockopt(
                self.fd,
                level,
                option,
                &value as *const T as *const libc::c_void,
                std::mem::size_of_val(&value) as libc::socklen_t, //std::mem::size_of::<T>() as libc::socklen_t
            )
        })
    }
    fn getsockopt<T>(&self, level: libc::c_int, option: libc::c_int, value: &mut T) -> Result<()> {
        let mut n = std::mem::size_of::<T>() as libc::socklen_t;
        handle_libc_error(unsafe {
            libc::getsockopt(
                self.fd,
                level,
                option,
                value as *const T as *mut T as *mut libc::c_void,
                &mut n, //std::mem::size_of::<T>() as libc::socklen_t
            )
        })
    }
    fn setsockopt_slice(
        &self,
        level: libc::c_int,
        option: libc::c_int,
        value: &[u8],
    ) -> Result<()> {
        handle_libc_error(unsafe {
            libc::setsockopt(
                self.fd,
                level,
                option,
                value as *const _ as *const libc::c_void,
                value.len() as libc::socklen_t,
            )
        })
    }
    pub fn set_nodelay(&self, enable: bool) -> Result<()> {
        let opt: libc::c_int = match enable {
            true => 1,
            false => 0,
        };
        self.setsockopt(SOL_SCTP, SCTP_NODELAY, opt)
    }
    // sctp_data_io_event is always on, so every read can report its SCTP_SNDRCV info
    fn set_events(&self, events: Events) -> Result<()> {
        let subscribe = SctpEventSubscribe {
            sctp_data_io_event: 1,
            sctp_association_event: events.assoc as u8,
            sctp_address_event: events.peer_addr as u8,
            sctp_shutdown_event: events.shutdown as u8,
            ..Default::default()
        };
        self.setsockopt(SOL_SCTP, SCTP_EVENTS, subscribe)
    }
    /// Sends the selected kernel events to `tx`. A second call replaces both.
    ///
    /// - Events are only processed while something reads from the socket, so a
    ///   send-only user needs a reader task to see them.
    /// - An event that doesn't fit in the channel is lost and counted by
    ///   [`SctpSocket::dropped_notifications`].
    /// - Events aren't ordered relative to the data that the reads return.
    /// - An accepted socket inherits the selected events from the listener, but
    ///   not the channel. Call `subscribe` on it before its first read; events
    ///   that are already queued (e.g. `CommUp`) wait in the kernel until then.
    /// - A failed connect is reported by its `Err`, not by an event, because
    ///   the socket is closed before anything can read the event.
    /// - `ShutdownComplete` can arrive after a read returned 0, so a reader
    ///   that stops at EOF may miss it.
    pub fn subscribe(
        &mut self,
        events: Events,
        tx: tokio::sync::mpsc::Sender<Notification>,
    ) -> Result<()> {
        self.set_events(events)?;
        self.notification_queue = Some(tx);
        Ok(())
    }
    /// How many notifications were lost because the channel was full.
    pub fn dropped_notifications(&self) -> u64 {
        self.notifications_dropped.load(Ordering::Relaxed)
    }
    pub fn set_snfbuf(&self, size: i32) -> Result<()> {
        self.setsockopt(libc::SOL_SOCKET, libc::SO_SNDBUF, size)
    }
    pub fn set_rcvbuf(&self, size: i32) -> Result<()> {
        self.setsockopt(libc::SOL_SOCKET, libc::SO_RCVBUF, size)
    }
    pub fn set_noblock(&self) -> Result<()> {
        let flags = unsafe { libc::fcntl(self.fd, libc::F_GETFL) };
        if flags == -1 {
            return Err(Error::last_os_error());
        }
        let res = unsafe { libc::fcntl(self.fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if res == -1 {
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub fn set_init(&self, info: Init) -> Result<()> {
        self.setsockopt(SOL_SCTP, SCTP_INIT_MSG, info)
    }
    pub fn set_rto(&self, info: RtoInfo) -> Result<()> {
        self.setsockopt(SOL_SCTP, SCTP_RTOINFO, info)
    }
    /// Sets the peer address that data is sent to (SCTP_PRIMARY_ADDR). Call it once
    /// the association is up; `addr` must be one of the peer's addresses (EINVAL
    /// otherwise). On an IPv6 socket an IPv4 peer may need its v4-mapped form.
    /// If the primary fails the kernel sends over another path, and returns to the
    /// primary once it is reachable again (SCTP_ADDR_AVAILABLE).
    pub fn set_primary_addr(&self, addr: std::net::SocketAddr) -> Result<()> {
        let osa: OsSocketAddr = addr.into();
        let raw = osa.as_ref();
        let mut prim = Prim {
            ssp_assoc_id: 0,
            ssp_addr: [0; 128],
        };
        prim.ssp_addr[..raw.len()].copy_from_slice(raw);
        self.setsockopt(SOL_SCTP, SCTP_PRIMARY_ADDR, prim)
    }
    /// The current primary peer address. It stays the same while the kernel
    /// sends over another path because the primary is unreachable.
    pub fn primary_addr(&self) -> Result<std::net::SocketAddr> {
        let mut prim = Prim {
            ssp_assoc_id: 0,
            ssp_addr: [0; 128],
        };
        self.getsockopt(SOL_SCTP, SCTP_PRIMARY_ADDR, &mut prim)?;
        let osa = unsafe {
            OsSocketAddr::copy_from_raw(prim.ssp_addr.as_ptr().cast(), prim.ssp_addr.len() as _)
        };
        osa.into_addr()
            .ok_or_else(|| Error::new(std::io::ErrorKind::InvalidData, "Invalid socket address"))
    }
    pub fn shutdown(&self, how: std::net::Shutdown) -> Result<()> {
        let flag = match how {
            std::net::Shutdown::Write => libc::SHUT_WR,
            std::net::Shutdown::Read => libc::SHUT_RD,
            std::net::Shutdown::Both => libc::SHUT_RDWR,
        };
        handle_libc_error(unsafe { libc::shutdown(self.fd, flag) })
    }
    pub fn connect(&self, address: std::net::SocketAddr) -> Result<()> {
        let ossockaddr: OsSocketAddr = (address).into();
        let addrslice = ossockaddr.as_ref();
        handle_libc_error(unsafe {
            libc::connect(
                self.fd.as_raw_fd(),
                addrslice as *const _ as *const libc::sockaddr,
                ossockaddr.len(),
            )
        })
    }

    pub fn bind(&self, address: std::net::SocketAddr) -> Result<()> {
        let ossockaddr: OsSocketAddr = (address).into();
        let addrslice = ossockaddr.as_ref();
        handle_libc_error(unsafe {
            libc::bind(
                self.fd.as_raw_fd(),
                addrslice as *const _ as *const libc::sockaddr,
                ossockaddr.len(),
            )
        })
    }

    pub fn listen(&self, backlog: i32) -> Result<()> {
        handle_libc_error(unsafe { libc::listen(self.fd.as_raw_fd(), backlog) })
    }

    pub fn accept(&self) -> Result<SctpSocket> {
        let r = unsafe {
            libc::accept(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if r < 0 {
            return Err(Error::last_os_error());
        }
        // the selected events, including sctp_data_io_event, are inherited
        Ok(Self::from_fd(r))
    }

    fn socketaddrlist_to_os(addresses: &[std::net::SocketAddr]) -> Vec<u8> {
        let mut addrs_u8: Vec<u8> = Vec::with_capacity(512);
        for addr in addresses {
            let ossockaddr: OsSocketAddr = (*addr).into();
            let slice = ossockaddr.as_ref();
            addrs_u8.extend(slice);
        }
        addrs_u8
    }

    pub fn bindx(&self, addresses: &[std::net::SocketAddr]) -> Result<()> {
        let addrs = Self::socketaddrlist_to_os(addresses);
        self.setsockopt_slice(SOL_SCTP, SCTP_SOCKOPT_BINDX_ADD, addrs.as_slice())
    }

    pub fn connectx(&self, addresses: &[std::net::SocketAddr]) -> Result<()> {
        let addrs = Self::socketaddrlist_to_os(addresses);
        self.setsockopt_slice(SOL_SCTP, SCTP_SOCKOPT_CONNECTX, addrs.as_slice())
    }

    pub fn sendmsg_default(&self, data: &[u8]) -> Result<usize> {
        let info = SndInfo {
            snd_ppid: self.ppid,
            ..Default::default()
        };
        self.sendmsg(data, &info)
    }

    /// Sends one message. `info.snd_ppid` is in host order.
    pub fn sendmsg(&self, data: &[u8], info: &SndInfo) -> Result<usize> {
        let mut iovec_item = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        let mut cbuf = CmsgBuf::new();

        // zeroed must be used to support linux-musl which has some private padding crap
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = &mut iovec_item;
        msghdr.msg_iovlen = 1;
        cbuf.attach(&mut msghdr);
        unsafe { write_sndinfo_cmsg(&mut msghdr, info) };
        // no MSG_DONTWAIT: blocking sockets block, and SctpSocketTokio sets O_NONBLOCK
        let r = unsafe { libc::sendmsg(self.fd, &mut msghdr as *mut libc::msghdr, 0) };
        if r < 0 {
            Err(Error::last_os_error())
        } else {
            Ok(r as usize)
        }
    }

    /// Receives user data. Notifications are consumed (and forwarded to the
    /// notification queue, if one is set) instead of being returned as data.
    ///
    /// An empty `data` returns `Ok(0)` without reading, which isn't EOF there.
    pub fn recvmsg(&self, data: &mut [u8]) -> Result<usize> {
        self.recvmsg_eor(data).map(|(n, _)| n)
    }

    /// Like `recvmsg`, but also returns whether the data ends a message (MSG_EOR).
    pub(crate) fn recvmsg_eor(&self, data: &mut [u8]) -> Result<(usize, bool)> {
        loop {
            if let Some(r) = self.recv_once(data)? {
                return Ok((r.len, r.msg_flags & libc::MSG_EOR != 0));
            }
        }
    }

    /// Receives user data and reports its stream, PPID and so on. See
    /// [`RecvInfo`]. Notifications are consumed as in `recvmsg`.
    ///
    /// An empty `data` returns a zero `len` without reading, which isn't EOF there.
    pub fn recvmsg_detailed(&self, data: &mut [u8]) -> Result<RecvInfo> {
        loop {
            if let Some(info) = self.recvmsg_detailed_once(data)? {
                return Ok(info);
            }
        }
    }

    /// One read for `recvmsg_detailed`; `None` means it consumed a notification.
    pub(crate) fn recvmsg_detailed_once(&self, data: &mut [u8]) -> Result<Option<RecvInfo>> {
        self.recv_once(data)?
            .map(|r| recv_info(r.len, r.msg_flags, r.info))
            .transpose()
    }

    // One recvmsg. `None` means it consumed a notification, or a piece of one.
    fn recv_once(&self, data: &mut [u8]) -> Result<Option<Received>> {
        // a queued notification can't be consumed into an empty buffer, so a
        // caller retrying it would never make progress
        if data.is_empty() {
            return Ok(Some(Received {
                len: 0,
                msg_flags: 0,
                info: None,
            }));
        }
        let mut iovec_item = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };

        let mut cbuf = CmsgBuf::new();

        // zeroed must be used to support linux-musl which has some private padding crap
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = &mut iovec_item;
        msghdr.msg_iovlen = 1;
        cbuf.attach(&mut msghdr);

        let r = unsafe { libc::recvmsg(self.fd, &mut msghdr as *mut libc::msghdr, 0) };
        if r < 0 {
            return Err(Error::last_os_error());
        }
        let len = r as usize;
        if msghdr.msg_flags & MSG_NOTIFICATION != 0 {
            self.handle_notification(&data[..len], msghdr.msg_flags);
            return Ok(None);
        }
        self.track_eor(len, msghdr.msg_flags);
        Ok(Some(Received {
            len,
            msg_flags: msghdr.msg_flags,
            info: unsafe { read_sndrcv_cmsg(&msghdr) },
        }))
    }
    /// Reads and clears the pending socket error (SO_ERROR).
    pub(crate) fn take_error(&self) -> Result<Option<Error>> {
        let mut err: libc::c_int = 0;
        self.getsockopt(libc::SOL_SOCKET, libc::SO_ERROR, &mut err)?;
        Ok((err != 0).then(|| Error::from_raw_os_error(err)))
    }
    pub fn getsocketname(&self) -> Result<std::net::SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        handle_libc_error(unsafe {
            libc::getsockname(
                self.fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        })?;
        let osa = unsafe {
            OsSocketAddr::copy_from_raw(&storage as *const _ as *const libc::sockaddr, len)
        };
        match osa.into_addr() {
            Some(addr) => Ok(addr),
            None => Err(Error::other("Invalid socket address")),
        }
    }
    pub fn getpeername(&self) -> Result<std::net::SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        handle_libc_error(unsafe {
            libc::getpeername(
                self.fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        })?;
        let osa = unsafe {
            OsSocketAddr::copy_from_raw(&storage as *const _ as *const libc::sockaddr, len)
        };
        match osa.into_addr() {
            Some(addr) => Ok(addr),
            None => Err(Error::other("Invalid socket address")),
        }
    }

    // Records whether a data read stopped mid-message. EOF (0 bytes) leaves it unchanged.
    fn track_eor(&self, len: usize, msg_flags: libc::c_int) {
        if len > 0 {
            self.data_partial
                .store(msg_flags & libc::MSG_EOR == 0, Ordering::Relaxed);
        }
    }

    /// True if the last data read ended mid-message.
    pub(crate) fn in_partial_message(&self) -> bool {
        self.data_partial.load(Ordering::Relaxed)
    }

    fn handle_notification(&self, data: &[u8], msg_flags: libc::c_int) {
        // A notification larger than the read buffer arrives in pieces and only the
        // last one has MSG_EOR, so the pieces are collected until then.
        let complete = msg_flags & libc::MSG_EOR != 0;
        let mut pending = self
            .notification_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if complete && pending.buf.is_empty() && !pending.overflow {
            drop(pending);
            self.deliver_notification(data);
            return;
        }
        if pending.buf.len() + data.len() <= MAX_NOTIFICATION {
            pending.buf.extend_from_slice(data);
        } else {
            // e.g. SCTP_SEND_FAILED carrying a large message; none we parse
            pending.overflow = true;
        }
        if complete {
            let p = std::mem::take(&mut *pending);
            drop(pending);
            if !p.overflow {
                self.deliver_notification(&p.buf);
            }
        }
    }

    fn deliver_notification(&self, data: &[u8]) {
        let Some(q) = &self.notification_queue else {
            return;
        };
        let Some(not) = parse_notification(data) else {
            return;
        };
        if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) = q.try_send(not) {
            self.notifications_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// Control buffer for one SCTP cmsg. A plain byte (or i32) array doesn't have the
// alignment cmsghdr needs, so writing the header through it would be UB.
#[repr(C, align(8))]
struct CmsgBuf([u8; 64]);

const _: () = assert!(std::mem::align_of::<CmsgBuf>() >= std::mem::align_of::<libc::cmsghdr>());
const _: () = assert!(
    unsafe { libc::CMSG_SPACE(std::mem::size_of::<SndRcvInfo>() as u32) } as usize
        <= std::mem::size_of::<CmsgBuf>()
);

impl CmsgBuf {
    fn new() -> Self {
        CmsgBuf([0; 64])
    }
    fn attach(&mut self, msghdr: &mut libc::msghdr) {
        msghdr.msg_control = self.0.as_mut_ptr().cast();
        msghdr.msg_controllen = self.0.len() as _;
    }
}

/// Writes an SCTP_SNDINFO cmsg, with the PPID in network order, and shrinks
/// msg_controllen to it.
/// Safety: msg_control must point to an attached `CmsgBuf`.
unsafe fn write_sndinfo_cmsg(msghdr: &mut libc::msghdr, info: &SndInfo) {
    let cmsg = libc::CMSG_FIRSTHDR(msghdr);
    if cmsg.is_null() {
        return;
    }
    let hlen = libc::CMSG_LEN(std::mem::size_of::<SndInfo>() as u32) as _;
    (*cmsg).cmsg_level = libc::IPPROTO_SCTP;
    (*cmsg).cmsg_type = SCTP_SNDINFO;
    (*cmsg).cmsg_len = hlen;
    let wire = SndInfo {
        snd_ppid: info.snd_ppid.to_be(),
        ..*info
    };
    std::ptr::copy_nonoverlapping(
        (&wire as *const SndInfo).cast::<u8>(),
        libc::CMSG_DATA(cmsg),
        std::mem::size_of::<SndInfo>(),
    );
    msghdr.msg_controllen = hlen;
}

/// Returns the SCTP_SNDRCV info from the control data that recvmsg filled in.
/// Safety: msg_control must point to an attached `CmsgBuf`.
unsafe fn read_sndrcv_cmsg(msghdr: &libc::msghdr) -> Option<SndRcvInfo> {
    let need = libc::CMSG_LEN(std::mem::size_of::<SndRcvInfo>() as u32) as usize;
    let mut cmsg = libc::CMSG_FIRSTHDR(msghdr);
    while !cmsg.is_null() {
        if (*cmsg).cmsg_level == libc::IPPROTO_SCTP
            && (*cmsg).cmsg_type == SCTP_SNDRCV
            && (*cmsg).cmsg_len as usize >= need
        {
            return Some(std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast()));
        }
        cmsg = libc::CMSG_NXTHDR(msghdr, cmsg);
    }
    None
}

/// Copies a kernel struct out of `data`, which has alignment 1. `None` if too short.
/// Safety: every bit pattern must be valid for `T`.
unsafe fn read_struct<T>(data: &[u8]) -> Option<T> {
    (data.len() >= std::mem::size_of::<T>()).then(|| std::ptr::read_unaligned(data.as_ptr().cast()))
}

fn parse_notification(data: &[u8]) -> Option<Notification> {
    // Safety: these repr(C) structs only hold integers and integer arrays
    let header: NotificationHeader = unsafe { read_struct(data) }?;
    match header.sn_type {
        SCTP_ASSOC_CHANGE => {
            let change: RawAssocChange = unsafe { read_struct(data) }?;
            Some(Notification::AssocChange(AssocChange {
                state: AssocState::from_raw(change.sac_state),
                error: change.sac_error,
                outbound_streams: change.sac_outbound_streams,
                inbound_streams: change.sac_inbound_streams,
            }))
        }
        SCTP_PEER_ADDR_CHANGE => {
            let paddr: IPaddrChange = unsafe { read_struct(data) }?;
            // None if not an IPv4/IPv6 address
            let addr = unsafe {
                OsSocketAddr::copy_from_raw(
                    paddr.spc_addr.as_ptr().cast(),
                    paddr.spc_addr.len() as _,
                )
            }
            .into_addr()?;
            Some(Notification::PeerAddrChange(PeerAddrChange {
                addr,
                state: PeerAddrState::from_raw(paddr.spc_state),
                error: paddr.spc_error,
            }))
        }
        SCTP_SHUTDOWN_EVENT => {
            let _: RawShutdownEvent = unsafe { read_struct(data) }?;
            Some(Notification::Shutdown)
        }
        _ => None,
    }
}

impl Drop for SctpSocket {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
            //println!("drop socket");
        }
    }
}

impl AsRawFd for SctpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

/*
fn write_fd(fd: RawFd, buf: &[u8]) -> Result<usize> {
    let rv = unsafe {
        libc::write(fd, buf.as_ptr() as *mut libc::c_void, buf.len())
    };
    if rv < 0 {
        return Err(Error::last_os_error());
    }
    Ok(rv as usize)
}*/

impl std::io::Read for SctpSocket {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.recvmsg(buf)
    }
}
impl std::io::Read for &SctpSocket {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.recvmsg(buf)
    }
}

impl std::io::Write for SctpSocket {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        //write_fd(self.fd, buf)
        self.sendmsg_default(buf)
        //self.write_fd(buf)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
impl std::io::Write for &SctpSocket {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        //write_fd(self.fd, buf)
        self.sendmsg_default(buf)
        //self.write_fd(buf)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prim_matches_kernel_layout() {
        // the kernel rejects SCTP_PRIMARY_ADDR unless optlen == sizeof(struct sctp_prim)
        assert_eq!(std::mem::size_of::<Prim>(), 132);
    }

    // An SCTP_PEER_ADDR_CHANGE notification as the kernel delivers it.
    fn paddr_change_bytes(addr: std::net::SocketAddr, state: u32, error: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&SCTP_PEER_ADDR_CHANGE.to_ne_bytes());
        b.extend_from_slice(&0u16.to_ne_bytes());
        b.extend_from_slice(&148u32.to_ne_bytes());
        let mut storage = [0u8; 128];
        let osa: OsSocketAddr = addr.into();
        storage[..osa.as_ref().len()].copy_from_slice(osa.as_ref());
        b.extend_from_slice(&storage);
        b.extend_from_slice(&state.to_ne_bytes());
        b.extend_from_slice(&error.to_ne_bytes());
        b.extend_from_slice(&0i32.to_ne_bytes()); // spc_assoc_id
        b
    }

    #[test]
    fn parse_paddr_change_misaligned() {
        let addr: std::net::SocketAddr = "10.0.1.2:3868".parse().unwrap();
        // offset 1 makes every field misaligned
        let mut buf = vec![0u8];
        buf.extend(paddr_change_bytes(addr, 2, 7));
        match parse_notification(&buf[1..]) {
            Some(Notification::PeerAddrChange(n)) => {
                assert_eq!(n.addr, addr);
                assert_eq!(n.state, PeerAddrState::Removed);
                assert_eq!(n.error, 7);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_short_buffers() {
        let addr: std::net::SocketAddr = "10.0.1.2:3868".parse().unwrap();
        let bytes = paddr_change_bytes(addr, 2, 7);
        for len in [0, 4, 8, 100, std::mem::size_of::<IPaddrChange>() - 1] {
            assert!(parse_notification(&bytes[..len]).is_none(), "len {len}");
        }
    }

    #[test]
    fn parse_other_type_ignored() {
        let mut bytes = paddr_change_bytes("10.0.1.2:3868".parse().unwrap(), 2, 7);
        bytes[..2].copy_from_slice(&0x8004u16.to_ne_bytes()); // SCTP_REMOTE_ERROR
        assert!(parse_notification(&bytes).is_none());
    }

    #[test]
    fn parse_peer_addr_states() {
        let addr: std::net::SocketAddr = "10.0.1.2:3868".parse().unwrap();
        for (raw, state) in [
            (0, PeerAddrState::Available),
            (1, PeerAddrState::Unreachable),
            (2, PeerAddrState::Removed),
            (3, PeerAddrState::Added),
            (4, PeerAddrState::MadePrimary),
            (5, PeerAddrState::Confirmed),
            (6, PeerAddrState::PotentiallyFailed),
            (99, PeerAddrState::Other(99)),
        ] {
            match parse_notification(&paddr_change_bytes(addr, raw, 0)) {
                Some(Notification::PeerAddrChange(n)) => assert_eq!(n.state, state),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    // An SCTP_ASSOC_CHANGE notification as the kernel delivers it.
    fn assoc_change_bytes(state: u16, error: u16, info: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&SCTP_ASSOC_CHANGE.to_ne_bytes());
        b.extend_from_slice(&0u16.to_ne_bytes());
        b.extend_from_slice(&(20 + info.len() as u32).to_ne_bytes());
        b.extend_from_slice(&state.to_ne_bytes());
        b.extend_from_slice(&error.to_ne_bytes());
        b.extend_from_slice(&5u16.to_ne_bytes()); // outbound streams
        b.extend_from_slice(&6u16.to_ne_bytes()); // inbound streams
        b.extend_from_slice(&0i32.to_ne_bytes()); // sac_assoc_id
        b.extend_from_slice(info);
        b
    }

    #[test]
    fn raw_structs_match_kernel_layout() {
        assert_eq!(std::mem::size_of::<RawAssocChange>(), 20);
        assert_eq!(std::mem::size_of::<RawShutdownEvent>(), 12);
    }

    #[test]
    fn parse_assoc_change_states() {
        for (raw, state) in [
            (0, AssocState::CommUp),
            (1, AssocState::CommLost),
            (2, AssocState::Restart),
            (3, AssocState::ShutdownComplete),
            (4, AssocState::CantStartAssoc),
            (99, AssocState::Other(99)),
        ] {
            let expected = Notification::AssocChange(AssocChange {
                state,
                error: 7,
                outbound_streams: 5,
                inbound_streams: 6,
            });
            // offset 1 makes every field misaligned; sac_info is ignored
            let mut buf = vec![0u8];
            buf.extend(assoc_change_bytes(raw, 7, &[1, 2, 3]));
            assert_eq!(parse_notification(&buf[1..]), Some(expected));
        }
    }

    #[test]
    fn parse_shutdown_event() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SCTP_SHUTDOWN_EVENT.to_ne_bytes());
        bytes.extend_from_slice(&0u16.to_ne_bytes());
        bytes.extend_from_slice(&12u32.to_ne_bytes());
        bytes.extend_from_slice(&0i32.to_ne_bytes());
        // offset 1 makes every field misaligned
        let mut buf = vec![0u8];
        buf.extend_from_slice(&bytes);
        assert_eq!(parse_notification(&buf[1..]), Some(Notification::Shutdown));
        for len in [0, 4, 8, 11] {
            assert!(parse_notification(&bytes[..len]).is_none(), "len {len}");
        }
    }

    #[test]
    fn parse_assoc_change_short() {
        let bytes = assoc_change_bytes(0, 0, &[]);
        for len in [0, 4, 8, 19] {
            assert!(parse_notification(&bytes[..len]).is_none(), "len {len}");
        }
    }

    #[test]
    fn recv_info_converts() {
        let raw = SndRcvInfo {
            sinfo_stream: 2,
            sinfo_ssn: 3,
            sinfo_flags: SCTP_UNORDERED,
            sinfo_ppid: 46u32.to_be(),
            sinfo_context: 4,
            sinfo_tsn: 5,
            sinfo_cumtsn: 6,
            ..Default::default()
        };
        let info = recv_info(10, libc::MSG_EOR, Some(raw)).unwrap();
        assert_eq!(
            info,
            RecvInfo {
                len: 10,
                stream: 2,
                ssn: 3,
                ppid: 46,
                unordered: true,
                eor: true,
                tsn: 5,
                cumtsn: 6,
                context: 4,
            }
        );
        let partial = recv_info(10, 0, Some(SndRcvInfo::default())).unwrap();
        assert!(!partial.eor && !partial.unordered);
    }

    #[test]
    fn recv_info_missing_cmsg() {
        // EOF and empty buffers have no info to report
        assert_eq!(recv_info(0, 0, None).unwrap(), RecvInfo::default());
        // data without info must not look like stream 0, PPID 0
        let err = recv_info(1, 0, None).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // needs a real socket
    fn dropped_notifications_counted() {
        let bytes = assoc_change_bytes(0, 0, &[]);
        let mut sock = SctpSocket::new().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        sock.subscribe(Events::all(), tx).unwrap();

        sock.deliver_notification(&bytes);
        assert_eq!(sock.dropped_notifications(), 0);
        sock.deliver_notification(&bytes); // the channel is full
        assert_eq!(sock.dropped_notifications(), 1);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());

        // a closed receiver means nobody is interested, so nothing is lost
        drop(rx);
        sock.deliver_notification(&bytes);
        assert_eq!(sock.dropped_notifications(), 1);
    }

    fn msghdr_with(cbuf: &mut CmsgBuf) -> libc::msghdr {
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        cbuf.attach(&mut msghdr);
        msghdr
    }

    #[test]
    fn sndinfo_cmsg_layout() {
        let info = SndInfo {
            snd_sid: 3,
            snd_flags: 0,
            snd_ppid: 46, // host order; the cmsg carries network order
            snd_context: 9,
            snd_assoc_id: 0,
        };
        let mut cbuf = CmsgBuf::new();
        let mut msghdr = msghdr_with(&mut cbuf);
        unsafe { write_sndinfo_cmsg(&mut msghdr, &info) };

        let hlen = unsafe { libc::CMSG_LEN(std::mem::size_of::<SndInfo>() as u32) } as usize;
        #[allow(clippy::unnecessary_cast)] // msg_controllen is u32 on musl
        let controllen = msghdr.msg_controllen as usize;
        assert_eq!(controllen, hlen);
        // raw pointer: a &cmsghdr would only cover the header, not the data after it
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msghdr) };
        let (level, ty, len) = unsafe {
            (
                (*cmsg).cmsg_level,
                (*cmsg).cmsg_type,
                (*cmsg).cmsg_len as usize,
            )
        };
        assert_eq!((level, ty, len), (libc::IPPROTO_SCTP, SCTP_SNDINFO, hlen));
        let sent: SndInfo = unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast()) };
        assert_eq!(
            (sent.snd_sid, sent.snd_ppid, sent.snd_context),
            (3, 46u32.to_be(), 9)
        );
    }

    // Writes a cmsg the way the kernel does on receive.
    fn put_cmsg(
        msghdr: &mut libc::msghdr,
        cmsg_type: libc::c_int,
        cmsg_len: usize,
        info: SndRcvInfo,
    ) {
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(msghdr);
            (*cmsg).cmsg_level = libc::IPPROTO_SCTP;
            (*cmsg).cmsg_type = cmsg_type;
            (*cmsg).cmsg_len = cmsg_len as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast(), info);
            msghdr.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<SndRcvInfo>() as u32) as _;
        }
    }

    #[test]
    fn sndrcv_cmsg_parsed() {
        let mut cbuf = CmsgBuf::new();
        let mut msghdr = msghdr_with(&mut cbuf);
        let len = unsafe { libc::CMSG_LEN(std::mem::size_of::<SndRcvInfo>() as u32) } as usize;
        let info = SndRcvInfo {
            sinfo_stream: 2,
            sinfo_ppid: 46u32.to_be(),
            sinfo_tsn: 77,
            ..Default::default()
        };
        put_cmsg(&mut msghdr, SCTP_SNDRCV, len, info);

        let got = unsafe { read_sndrcv_cmsg(&msghdr) }.expect("SCTP_SNDRCV not found");
        assert_eq!(
            (got.sinfo_stream, got.sinfo_ppid, got.sinfo_tsn),
            (2, 46u32.to_be(), 77)
        );
    }

    #[test]
    fn sndrcv_cmsg_rejected() {
        let len = unsafe { libc::CMSG_LEN(std::mem::size_of::<SndRcvInfo>() as u32) } as usize;
        // cmsg_len too short for SndRcvInfo
        let mut cbuf = CmsgBuf::new();
        let mut msghdr = msghdr_with(&mut cbuf);
        put_cmsg(&mut msghdr, SCTP_SNDRCV, len - 1, SndRcvInfo::default());
        assert!(unsafe { read_sndrcv_cmsg(&msghdr) }.is_none());
        // other cmsg type
        let mut cbuf = CmsgBuf::new();
        let mut msghdr = msghdr_with(&mut cbuf);
        put_cmsg(&mut msghdr, SCTP_SNDINFO, len, SndRcvInfo::default());
        assert!(unsafe { read_sndrcv_cmsg(&msghdr) }.is_none());
        // no control data at all
        let mut cbuf = CmsgBuf::new();
        let mut msghdr = msghdr_with(&mut cbuf);
        msghdr.msg_controllen = 0;
        assert!(unsafe { read_sndrcv_cmsg(&msghdr) }.is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore)] // needs a real socket
    fn fragmented_notification_reassembled() {
        let addr: std::net::SocketAddr = "10.0.1.2:3868".parse().unwrap();
        let bytes = paddr_change_bytes(addr, 2, 7);
        let mut sock = SctpSocket::new().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        sock.subscribe(Events::all(), tx).unwrap();

        // pieces of one notification, as read with a 64-byte buffer: only the last has MSG_EOR
        sock.handle_notification(&bytes[..64], MSG_NOTIFICATION);
        sock.handle_notification(&bytes[64..128], MSG_NOTIFICATION);
        assert!(rx.try_recv().is_err());
        sock.handle_notification(&bytes[128..], MSG_NOTIFICATION | libc::MSG_EOR);
        match rx.try_recv() {
            Ok(Notification::PeerAddrChange(n)) => {
                assert_eq!(
                    (n.addr, n.state, n.error),
                    (addr, PeerAddrState::Removed, 7)
                )
            }
            other => panic!("unexpected {other:?}"),
        }

        // a complete notification is delivered directly
        sock.handle_notification(&bytes, MSG_NOTIFICATION | libc::MSG_EOR);
        assert!(matches!(rx.try_recv(), Ok(Notification::PeerAddrChange(_))));

        // an oversized notification is dropped, and the next one still works
        let big = vec![0u8; MAX_NOTIFICATION];
        sock.handle_notification(&bytes[..64], MSG_NOTIFICATION);
        sock.handle_notification(&big, MSG_NOTIFICATION);
        sock.handle_notification(&bytes[64..], MSG_NOTIFICATION | libc::MSG_EOR);
        assert!(rx.try_recv().is_err());
        sock.handle_notification(&bytes, MSG_NOTIFICATION | libc::MSG_EOR);
        assert!(matches!(rx.try_recv(), Ok(Notification::PeerAddrChange(_))));
    }
}
