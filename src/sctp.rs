use std::io::{Error, Result};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

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
const SCTP_PEER_ADDR_CHANGE: u16 = 0x8002;

// spc_state values of an address change notification
pub const SCTP_ADDR_AVAILABLE: u32 = 0;
pub const SCTP_ADDR_UNREACHABLE: u32 = 1;
pub const SCTP_ADDR_REMOVED: u32 = 2;
pub const SCTP_ADDR_ADDED: u32 = 3;
pub const SCTP_ADDR_MADE_PRIM: u32 = 4;
pub const SCTP_ADDR_CONFIRMED: u32 = 5;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SndRcvInfo {
    pub sinfo_stream: u16,
    pub sinfo_ssn: u16,
    pub sinfo_flags: u16,
    pub sinfo_ppid: u32,
    pub sinfo_context: u32,
    pub sinfo_timetolive: u32,
    pub sinfo_tsn: u32,
    pub sinfo_cumtsn: u32,
    pub sinfo_assoc_id: i32,
}

#[repr(C)]
#[derive(Debug)]
pub struct SndInfo {
    pub snd_sid: u16,
    pub snd_flags: u16,
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
pub struct NotificationHeader {
    pub sn_type: u16,
    pub sn_flags: u16,
    pub sn_length: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct IPaddrChange {
    pub spc_type: u16,
    pub spc_flags: u16,
    pub spc_length: u32,
    pub spc_addr: [u8; 128],
    pub spc_state: u32,
    pub spc_error: u32,
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

#[derive(Debug)]
pub struct NotificationAddress {
    addr: std::net::SocketAddr,
    state: u32,
    error: u32,
}

impl NotificationAddress {
    /// The peer address whose state changed.
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }
    /// `spc_state`: SCTP_ADDR_AVAILABLE (0), SCTP_ADDR_UNREACHABLE (1), SCTP_ADDR_REMOVED (2),
    /// SCTP_ADDR_ADDED (3), SCTP_ADDR_MADE_PRIM (4), SCTP_ADDR_CONFIRMED (5), ...
    pub fn state(&self) -> u32 {
        self.state
    }
    /// `spc_error`: the error cause, if any.
    pub fn error(&self) -> u32 {
        self.error
    }
}
#[derive(Debug)]
pub enum Notification {
    Address(NotificationAddress),
    Shutdown(),
}

fn handle_libc_error(err: i32) -> Result<()> {
    if err == -1 {
        Err(Error::last_os_error())
    } else {
        Ok(())
    }
}

impl SctpSocket {
    pub fn new() -> Result<Self> {
        let fd = unsafe { libc::socket(libc::PF_INET, libc::SOCK_STREAM, libc::IPPROTO_SCTP) };
        if fd == -1 {
            return Err(Error::last_os_error());
        }
        Ok(Self {
            fd,
            ppid: 0,
            notification_queue: None,
            notification_pending: Default::default(),
            data_partial: AtomicBool::new(false),
        })
    }
    pub fn new6() -> Result<Self> {
        let fd = unsafe { libc::socket(libc::PF_INET6, libc::SOCK_STREAM, libc::IPPROTO_SCTP) };
        if fd == -1 {
            return Err(Error::last_os_error());
        }
        Ok(Self {
            fd,
            ppid: 0,
            notification_queue: None,
            notification_pending: Default::default(),
            data_partial: AtomicBool::new(false),
        })
    }
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
    pub fn subscribe_notifications(&mut self, s: tokio::sync::mpsc::Sender<Notification>) {
        self.notification_queue = Some(s);
    }
    pub fn set_snfbuf(&self, size: i32) -> Result<()> {
        self.setsockopt(libc::SOL_SOCKET, libc::SO_SNDBUF, size)
    }
    pub fn set_rcvbuf(&self, size: i32) -> Result<()> {
        self.setsockopt(libc::SOL_SOCKET, libc::SO_RCVBUF, size)
    }
    pub fn subscribe_aux(&self) -> Result<()> {
        let mut events = SctpEventSubscribe::default();
        self.getsockopt(SOL_SCTP, SCTP_EVENTS, &mut events)?;
        events.sctp_data_io_event = 1;
        self.setsockopt(SOL_SCTP, SCTP_EVENTS, events)
    }
    pub fn subscribe_addr(&self) -> Result<()> {
        let mut events = SctpEventSubscribe::default();
        self.getsockopt(SOL_SCTP, SCTP_EVENTS, &mut events)?;
        events.sctp_address_event = 1;
        self.setsockopt(SOL_SCTP, SCTP_EVENTS, events)
    }
    pub fn subscribe_assoc(&self) -> Result<()> {
        let mut events = SctpEventSubscribe::default();
        self.getsockopt(SOL_SCTP, SCTP_EVENTS, &mut events)?;
        events.sctp_association_event = 1;
        self.setsockopt(SOL_SCTP, SCTP_EVENTS, events)
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
        if let Some(q) = &self.notification_queue {
            let _ = q.try_send(Notification::Shutdown());
        }
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
        Ok(SctpSocket {
            fd: r,
            ppid: 0,
            notification_queue: None,
            notification_pending: Default::default(),
            data_partial: AtomicBool::new(false),
        })
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
        let info: SndInfo = SndInfo {
            snd_sid: 0,
            snd_flags: 0,
            snd_ppid: self.ppid.to_be(),
            snd_context: 0,
            snd_assoc_id: 0,
        };
        self.sendmsg(data, &info)
    }

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
    pub fn recvmsg(&self, data: &mut [u8]) -> Result<usize> {
        self.recvmsg_eor(data).map(|(n, _)| n)
    }

    /// Like `recvmsg`, but also returns whether the data ends a message (MSG_EOR).
    pub(crate) fn recvmsg_eor(&self, data: &mut [u8]) -> Result<(usize, bool)> {
        loop {
            let mut iovec_item = libc::iovec {
                iov_base: data.as_ptr() as *mut libc::c_void,
                iov_len: data.len(),
            };

            // zeroed must be used to support linux-musl which has some private padding crap
            // no control buffer: ancillary data is not reported by this method
            let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
            msghdr.msg_iov = &mut iovec_item;
            msghdr.msg_iovlen = 1;

            let r = unsafe { libc::recvmsg(self.fd, &mut msghdr as *mut libc::msghdr, 0) };
            if r < 0 {
                return Err(Error::last_os_error());
            }
            if msghdr.msg_flags & MSG_NOTIFICATION != 0 {
                self.handle_notification(&data[..r as usize], msghdr.msg_flags);
                continue;
            }
            let eor = self.track_eor(r as usize, msghdr.msg_flags);
            return Ok((r as usize, eor));
        }
    }

    pub fn recvmsg_detailed(&self, data: &mut [u8], info: &mut SndRcvInfo) -> Result<usize> {
        loop {
            let r = self.recvmsg_detailed_i(data, info);
            match r {
                Ok(s) => {
                    if s != 0x99999999 {
                        return r;
                    }
                }
                Err(_) => return r,
            }
        }
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
    fn track_eor(&self, len: usize, msg_flags: libc::c_int) -> bool {
        let eor = msg_flags & libc::MSG_EOR != 0;
        if len > 0 {
            self.data_partial.store(!eor, Ordering::Relaxed);
        }
        eor
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
        if let Some(q) = &self.notification_queue {
            if let Some(not) = parse_notification(data) {
                let _ = q.try_send(not);
            }
        }
    }

    pub fn recvmsg_detailed_i(&self, data: &mut [u8], info: &mut SndRcvInfo) -> Result<usize> {
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

        let r = unsafe { libc::recvmsg(self.fd, &mut msghdr as *mut libc::msghdr, 0) };
        if r < 0 {
            Err(Error::last_os_error())
        } else {
            if msghdr.msg_flags & MSG_NOTIFICATION != 0 {
                self.handle_notification(&data[..r as usize], msghdr.msg_flags);
                return Ok(0x99999999);
            }
            self.track_eor(r as usize, msghdr.msg_flags);
            if let Some(i) = unsafe { read_sndrcv_cmsg(&msghdr) } {
                *info = i;
            }
            Ok(r as usize)
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

/// Writes an SCTP_SNDINFO cmsg and shrinks msg_controllen to it.
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
    std::ptr::copy_nonoverlapping(
        (info as *const SndInfo).cast::<u8>(),
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

fn parse_notification(data: &[u8]) -> Option<Notification> {
    // data has alignment 1, so the kernel structs are copied out with read_unaligned
    if data.len() < std::mem::size_of::<NotificationHeader>() {
        return None;
    }
    let header: NotificationHeader = unsafe { std::ptr::read_unaligned(data.as_ptr().cast()) };
    if header.sn_type != SCTP_PEER_ADDR_CHANGE {
        return None;
    }
    if data.len() < std::mem::size_of::<IPaddrChange>() {
        return None;
    }
    let paddr: IPaddrChange = unsafe { std::ptr::read_unaligned(data.as_ptr().cast()) };
    // None if not an IPv4/IPv6 address
    let addr = unsafe {
        OsSocketAddr::copy_from_raw(paddr.spc_addr.as_ptr().cast(), paddr.spc_addr.len() as _)
    }
    .into_addr()?;
    Some(Notification::Address(NotificationAddress {
        addr,
        state: paddr.spc_state,
        error: paddr.spc_error,
    }))
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
            Some(Notification::Address(n)) => {
                assert_eq!(n.addr, addr);
                assert_eq!(n.state, 2);
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
        bytes[..2].copy_from_slice(&0x8001u16.to_ne_bytes()); // SCTP_ASSOC_CHANGE
        assert!(parse_notification(&bytes).is_none());
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
            snd_ppid: 46u32.to_be(),
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
        sock.subscribe_notifications(tx);

        // pieces of one notification, as read with a 64-byte buffer: only the last has MSG_EOR
        sock.handle_notification(&bytes[..64], MSG_NOTIFICATION);
        sock.handle_notification(&bytes[64..128], MSG_NOTIFICATION);
        assert!(rx.try_recv().is_err());
        sock.handle_notification(&bytes[128..], MSG_NOTIFICATION | libc::MSG_EOR);
        match rx.try_recv() {
            Ok(Notification::Address(n)) => assert_eq!((n.addr, n.state, n.error), (addr, 2, 7)),
            other => panic!("unexpected {other:?}"),
        }

        // a complete notification is delivered directly
        sock.handle_notification(&bytes, MSG_NOTIFICATION | libc::MSG_EOR);
        assert!(matches!(rx.try_recv(), Ok(Notification::Address(_))));

        // an oversized notification is dropped, and the next one still works
        let big = vec![0u8; MAX_NOTIFICATION];
        sock.handle_notification(&bytes[..64], MSG_NOTIFICATION);
        sock.handle_notification(&big, MSG_NOTIFICATION);
        sock.handle_notification(&bytes[64..], MSG_NOTIFICATION | libc::MSG_EOR);
        assert!(rx.try_recv().is_err());
        sock.handle_notification(&bytes, MSG_NOTIFICATION | libc::MSG_EOR);
        assert!(matches!(rx.try_recv(), Ok(Notification::Address(_))));
    }
}
