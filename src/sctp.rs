use std::io::{Error, Result};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
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
const SCTP_ASSOCINFO: libc::c_int = 1;
const SCTP_PEER_ADDR_PARAMS: libc::c_int = 9;
const SCTP_STATUS: libc::c_int = 14;
const SCTP_GET_PEER_ADDRS: libc::c_int = 108;
const SCTP_GET_LOCAL_ADDRS: libc::c_int = 109;

// spp_flags
const SPP_HB_ENABLE: u32 = 1 << 0;
const SPP_HB_DISABLE: u32 = 1 << 1;
const SPP_HB_TIME_IS_ZERO: u32 = 1 << 7;

// struct sctp_getaddrs before the variable-length addresses
const GETADDRS_HEADER: usize = 8;
// Limits for the address list buffer. The kernel kmallocs the local list's
// buffer, so it starts small and only grows when the kernel asks for more.
const ADDR_LIST_MIN: usize = 4096;
const ADDR_LIST_MAX: usize = 1024 * 1024;

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

// struct sctp_paddrinfo: packed, aligned(4), so the address is kept as bytes
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RawPaddrInfo {
    spinfo_assoc_id: i32,
    spinfo_address: [u8; 128],
    spinfo_state: i32,
    spinfo_cwnd: u32,
    spinfo_srtt: u32,
    spinfo_rto: u32,
    spinfo_mtu: u32,
}

// struct sctp_status
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RawStatus {
    sstat_assoc_id: i32,
    sstat_state: i32,
    sstat_rwnd: u32,
    sstat_unackdata: u16,
    sstat_penddata: u16,
    sstat_instrms: u16,
    sstat_outstrms: u16,
    sstat_fragmentation_point: u32,
    sstat_primary: RawPaddrInfo,
}

// struct sctp_assocparams
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct RawAssocParams {
    sasoc_assoc_id: i32,
    sasoc_asocmaxrxt: u16,
    sasoc_number_peer_destinations: u16,
    sasoc_peer_rwnd: u32,
    sasoc_local_rwnd: u32,
    sasoc_cookie_life: u32,
}

// struct sctp_paddrparams is packed, aligned(4): spp_pathmtu is at offset 138, and
// the trailing padding makes it 156 bytes. Rust can't put packed and align on one
// struct, so the wrapper adds the alignment. Fields can only be read by value.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct PaddrParamsFields {
    spp_assoc_id: i32,
    spp_address: [u8; 128],
    spp_hbinterval: u32,
    spp_pathmaxrxt: u16,
    spp_pathmtu: u32,
    spp_sackdelay: u32,
    spp_flags: u32,
    spp_ipv6_flowlabel: u32,
    spp_dscp: u8,
}

#[repr(C, align(4))]
#[derive(Clone, Copy)]
struct RawPaddrParams(PaddrParamsFields);

impl RawPaddrParams {
    // A zero address selects the association (or the socket when there is none).
    fn new(addr: Option<std::net::SocketAddr>) -> Self {
        RawPaddrParams(PaddrParamsFields {
            spp_assoc_id: 0,
            spp_address: addr.map_or([0; 128], addr_to_storage),
            spp_hbinterval: 0,
            spp_pathmaxrxt: 0,
            spp_pathmtu: 0,
            spp_sackdelay: 0,
            spp_flags: 0,
            spp_ipv6_flowlabel: 0,
            spp_dscp: 0,
        })
    }
}

fn addr_to_storage(addr: std::net::SocketAddr) -> [u8; 128] {
    let osa: OsSocketAddr = addr.into();
    let raw = osa.as_ref();
    let mut storage = [0; 128];
    storage[..raw.len()].copy_from_slice(raw);
    storage
}

fn storage_to_addr(storage: &[u8; 128]) -> Result<std::net::SocketAddr> {
    let osa = unsafe { OsSocketAddr::copy_from_raw(storage.as_ptr().cast(), storage.len() as _) };
    osa.into_addr()
        .ok_or_else(|| Error::new(std::io::ErrorKind::InvalidData, "Invalid socket address"))
}

/// Parses the `count` addresses of a struct sctp_getaddrs. Entries have the length
/// of their address family, so the list can only be walked from the start.
fn parse_addr_list(mut data: &[u8], count: usize) -> Result<Vec<std::net::SocketAddr>> {
    let bad = || Error::new(std::io::ErrorKind::InvalidData, "Invalid address list");
    let mut addrs = Vec::new();
    for _ in 0..count {
        let family = data.get(..2).ok_or_else(bad)?;
        let len = match u16::from_ne_bytes([family[0], family[1]]) as libc::c_int {
            libc::AF_INET => std::mem::size_of::<libc::sockaddr_in>(),
            libc::AF_INET6 => std::mem::size_of::<libc::sockaddr_in6>(),
            _ => return Err(bad()),
        };
        let entry = data.get(..len).ok_or_else(bad)?;
        let osa = unsafe { OsSocketAddr::copy_from_raw(entry.as_ptr().cast(), len as _) };
        addrs.push(osa.into_addr().ok_or_else(bad)?);
        data = &data[len..];
    }
    Ok(addrs)
}

fn invalid_zero(what: &str) -> Error {
    Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("{what} can't be 0: the kernel would keep the current value"),
    )
}

fn path_params_raw(
    addr: Option<std::net::SocketAddr>,
    params: PathParams,
) -> Result<RawPaddrParams> {
    let mut raw = RawPaddrParams::new(addr);
    match params.heartbeat {
        None => {}
        Some(Heartbeat::Disabled) => raw.0.spp_flags = SPP_HB_DISABLE,
        Some(Heartbeat::Enabled) => raw.0.spp_flags = SPP_HB_ENABLE,
        Some(Heartbeat::Interval(0)) => raw.0.spp_flags = SPP_HB_ENABLE | SPP_HB_TIME_IS_ZERO,
        Some(Heartbeat::Interval(ms)) => {
            raw.0.spp_flags = SPP_HB_ENABLE;
            raw.0.spp_hbinterval = ms;
        }
    }
    if let Some(n) = params.max_retrans {
        if n == 0 {
            return Err(invalid_zero("max_retrans"));
        }
        raw.0.spp_pathmaxrxt = n;
    }
    Ok(raw)
}

fn assoc_params_raw(params: AssocParams) -> Result<RawAssocParams> {
    let mut raw = RawAssocParams::default();
    if let Some(n) = params.max_retrans {
        if n == 0 {
            return Err(invalid_zero("max_retrans"));
        }
        raw.sasoc_asocmaxrxt = n;
    }
    if let Some(ms) = params.cookie_life_ms {
        if ms == 0 {
            return Err(invalid_zero("cookie_life_ms"));
        }
        raw.sasoc_cookie_life = ms;
    }
    Ok(raw)
}

fn linger_value(linger: Option<std::time::Duration>) -> libc::linger {
    match linger {
        None => libc::linger {
            l_onoff: 0,
            l_linger: 0,
        },
        Some(d) => {
            // SO_LINGER has whole seconds; rounding down could turn a short linger
            // into 0, which aborts instead of waiting
            let secs = d.as_secs().saturating_add(u64::from(d.subsec_nanos() > 0));
            libc::linger {
                l_onoff: 1,
                l_linger: secs.min(libc::c_int::MAX as u64) as libc::c_int,
            }
        }
    }
}

/// What [`SctpSocket::status`] reports about the association (SCTP_STATUS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssocStatus {
    pub state: AssocStatusState,
    /// The peer's current receive window.
    pub peer_rwnd: u32,
    /// Data chunks sent and not yet acknowledged.
    pub unacked_data: u16,
    /// Data chunks waiting to be read.
    pub pending_data: u16,
    /// Negotiated stream counts. A stream id must be below `outbound_streams`
    /// to be sent on.
    pub inbound_streams: u16,
    pub outbound_streams: u16,
    pub fragmentation_point: u32,
    /// The primary path.
    pub primary: PathInfo,
}

/// The state of one path, see [`AssocStatus::primary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathInfo {
    pub addr: std::net::SocketAddr,
    pub state: PathState,
    pub cwnd: u32,
    pub rto_ms: u32,
    pub mtu: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssocStatusState {
    Empty,
    Closed,
    CookieWait,
    CookieEchoed,
    Established,
    ShutdownPending,
    ShutdownSent,
    ShutdownReceived,
    ShutdownAckSent,
    /// A state this crate doesn't know.
    Other(i32),
}

impl AssocStatusState {
    fn from_raw(state: i32) -> Self {
        match state {
            0 => AssocStatusState::Empty,
            1 => AssocStatusState::Closed,
            2 => AssocStatusState::CookieWait,
            3 => AssocStatusState::CookieEchoed,
            4 => AssocStatusState::Established,
            5 => AssocStatusState::ShutdownPending,
            6 => AssocStatusState::ShutdownSent,
            7 => AssocStatusState::ShutdownReceived,
            8 => AssocStatusState::ShutdownAckSent,
            other => AssocStatusState::Other(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathState {
    Inactive,
    PotentiallyFailed,
    Active,
    Unconfirmed,
    /// A state this crate doesn't know.
    Other(i32),
}

impl PathState {
    fn from_raw(state: i32) -> Self {
        match state {
            0 => PathState::Inactive,
            1 => PathState::PotentiallyFailed,
            2 => PathState::Active,
            3 => PathState::Unconfirmed,
            other => PathState::Other(other),
        }
    }
}

/// How a path sends heartbeats, see [`PathParams`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heartbeat {
    Disabled,
    /// Enabled, with the interval unchanged.
    Enabled,
    /// Enabled, with this interval in milliseconds. The kernel adds the path's
    /// RTO to it. Values are rounded up to a kernel timer tick.
    Interval(u32),
}

/// Path settings to change (SCTP_PEER_ADDR_PARAMS). `None` leaves a setting
/// unchanged. See [`SctpSocket::set_path_params`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathParams {
    pub heartbeat: Option<Heartbeat>,
    /// Retransmissions before a path is unreachable. Can't be 0.
    pub max_retrans: Option<u16>,
}

/// Current path settings, see [`SctpSocket::path_params`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathConfig {
    pub heartbeat_enabled: bool,
    pub heartbeat_interval_ms: u32,
    pub max_retrans: u16,
    /// Path MTU, or 0 when there is no association yet.
    pub mtu: u32,
}

/// Association settings to change (SCTP_ASSOCINFO). `None` leaves a setting
/// unchanged. See [`SctpSocket::set_assoc_params`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssocParams {
    /// Retransmissions before the association fails. Can't be 0. With several
    /// paths the kernel rejects a value above the sum of their path limits.
    pub max_retrans: Option<u16>,
    /// Can't be 0.
    pub cookie_life_ms: Option<u32>,
}

/// Current association settings, see [`SctpSocket::assoc_params`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssocConfig {
    pub max_retrans: u16,
    pub peer_destinations: u16,
    pub peer_rwnd: u32,
    pub local_rwnd: u32,
    pub cookie_life_ms: u32,
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
    fn getsockopt_slice(
        &self,
        level: libc::c_int,
        option: libc::c_int,
        value: &mut [u8],
    ) -> Result<()> {
        let mut n = value.len() as libc::socklen_t;
        handle_libc_error(unsafe {
            libc::getsockopt(self.fd, level, option, value.as_mut_ptr().cast(), &mut n)
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
    pub fn set_sndbuf(&self, size: i32) -> Result<()> {
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
        let prim = Prim {
            ssp_assoc_id: 0,
            ssp_addr: addr_to_storage(addr),
        };
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
        storage_to_addr(&prim.ssp_addr)
    }
    fn read_addrs(&self, option: libc::c_int, assoc_id: i32) -> Result<Vec<std::net::SocketAddr>> {
        let mut size = ADDR_LIST_MIN;
        loop {
            let mut buf = vec![0u8; size];
            buf[..4].copy_from_slice(&assoc_id.to_ne_bytes());
            match self.getsockopt_slice(SOL_SCTP, option, &mut buf) {
                Ok(()) => {
                    // not optlen: for the local list it leaves out the header
                    let count = u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]);
                    return parse_addr_list(&buf[GETADDRS_HEADER..], count as usize);
                }
                Err(e) if e.raw_os_error() == Some(libc::ENOMEM) && size < ADDR_LIST_MAX => {
                    size *= 2;
                }
                Err(e) => return Err(e),
            }
        }
    }
    /// The addresses of the peer. Fails with `EINVAL` without an association. On
    /// an IPv6 socket an IPv4 peer may be reported in its v4-mapped form.
    pub fn peer_addrs(&self) -> Result<Vec<std::net::SocketAddr>> {
        self.read_addrs(SCTP_GET_PEER_ADDRS, 0)
    }
    /// The local addresses. With an association these are the ones it uses,
    /// which are what the peer knows about. Otherwise these are the bound
    /// addresses: for a wildcard bind all addresses of the machine, and none for
    /// an unbound socket. An IPv4 address on an IPv6 socket may be reported in
    /// its v4-mapped form.
    pub fn local_addrs(&self) -> Result<Vec<std::net::SocketAddr>> {
        // only this option doesn't ignore the id: 0 means the bound addresses
        let id = self.raw_status().map_or(0, |s| s.sstat_assoc_id);
        match self.read_addrs(SCTP_GET_LOCAL_ADDRS, id) {
            // the association ended since the status was read
            Err(e) if id != 0 && e.raw_os_error() == Some(libc::EINVAL) => {
                self.read_addrs(SCTP_GET_LOCAL_ADDRS, 0)
            }
            res => res,
        }
    }
    fn raw_status(&self) -> Result<RawStatus> {
        // Safety: the struct only holds integers and integer arrays
        let mut raw: RawStatus = unsafe { std::mem::zeroed() };
        self.getsockopt(SOL_SCTP, SCTP_STATUS, &mut raw)?;
        Ok(raw)
    }
    /// The state of the association, including the negotiated stream counts.
    /// Fails with `EINVAL` without an association.
    pub fn status(&self) -> Result<AssocStatus> {
        let raw = self.raw_status()?;
        let primary = raw.sstat_primary;
        Ok(AssocStatus {
            state: AssocStatusState::from_raw(raw.sstat_state),
            peer_rwnd: raw.sstat_rwnd,
            unacked_data: raw.sstat_unackdata,
            pending_data: raw.sstat_penddata,
            inbound_streams: raw.sstat_instrms,
            outbound_streams: raw.sstat_outstrms,
            fragmentation_point: raw.sstat_fragmentation_point,
            // spinfo_srtt is left out: unlike spinfo_rto the kernel doesn't convert
            // it to milliseconds
            primary: PathInfo {
                addr: storage_to_addr(&primary.spinfo_address)?,
                state: PathState::from_raw(primary.spinfo_state),
                cwnd: primary.spinfo_cwnd,
                rto_ms: primary.spinfo_rto,
                mtu: primary.spinfo_mtu,
            },
        })
    }
    /// Changes path settings. `Some(addr)` selects one peer address (`EINVAL` if
    /// it isn't one; on an IPv6 socket an IPv4 peer may need its v4-mapped form).
    /// `None` selects all paths of the association, or without an association
    /// the defaults for future ones, which accepted sockets get from a listener.
    pub fn set_path_params(
        &self,
        addr: Option<std::net::SocketAddr>,
        params: PathParams,
    ) -> Result<()> {
        self.setsockopt(
            SOL_SCTP,
            SCTP_PEER_ADDR_PARAMS,
            path_params_raw(addr, params)?,
        )
    }
    /// The current path settings, with the same `addr` rules as
    /// [`SctpSocket::set_path_params`].
    pub fn path_params(&self, addr: Option<std::net::SocketAddr>) -> Result<PathConfig> {
        let mut raw = RawPaddrParams::new(addr);
        self.getsockopt(SOL_SCTP, SCTP_PEER_ADDR_PARAMS, &mut raw)?;
        let fields = raw.0;
        Ok(PathConfig {
            heartbeat_enabled: fields.spp_flags & SPP_HB_ENABLE != 0,
            heartbeat_interval_ms: fields.spp_hbinterval,
            max_retrans: fields.spp_pathmaxrxt,
            mtu: fields.spp_pathmtu,
        })
    }
    /// Changes association settings: those of the association, or without one the
    /// defaults for future ones, which accepted sockets get from a listener.
    pub fn set_assoc_params(&self, params: AssocParams) -> Result<()> {
        self.setsockopt(SOL_SCTP, SCTP_ASSOCINFO, assoc_params_raw(params)?)
    }
    /// The current association settings, or without an association the defaults.
    pub fn assoc_params(&self) -> Result<AssocConfig> {
        let mut raw = RawAssocParams::default();
        self.getsockopt(SOL_SCTP, SCTP_ASSOCINFO, &mut raw)?;
        Ok(AssocConfig {
            max_retrans: raw.sasoc_asocmaxrxt,
            peer_destinations: raw.sasoc_number_peer_destinations,
            peer_rwnd: raw.sasoc_peer_rwnd,
            local_rwnd: raw.sasoc_local_rwnd,
            cookie_life_ms: raw.sasoc_cookie_life,
        })
    }
    /// Sets SO_REUSEADDR. Call it before `bind`. A port can only be shared if both
    /// sockets set this and the other one isn't listening, e.g. to restart a
    /// listener while its accepted sockets stay up. They inherit the option.
    pub fn set_reuse_addr(&self, enable: bool) -> Result<()> {
        let opt = enable as libc::c_int;
        self.setsockopt(libc::SOL_SOCKET, libc::SO_REUSEADDR, opt)
    }
    /// Sets SO_LINGER. With a non-zero duration, closing the socket blocks the
    /// calling thread until the association has shut down or the time has passed,
    /// also in `Drop`. Rounds up to whole seconds. `Some(Duration::ZERO)` is the
    /// same as [`SctpSocket::set_abort_on_close`].
    pub fn set_linger(&self, linger: Option<std::time::Duration>) -> Result<()> {
        self.setsockopt(libc::SOL_SOCKET, libc::SO_LINGER, linger_value(linger))
    }
    /// With `true`, closing the socket sends ABORT right away instead of a graceful
    /// SHUTDOWN, and data that wasn't sent yet is discarded. It doesn't block.
    pub fn set_abort_on_close(&self, enable: bool) -> Result<()> {
        self.set_linger(enable.then_some(std::time::Duration::ZERO))
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

impl AsFd for SctpSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // Safety: self owns the fd and closes it only in Drop
        unsafe { BorrowedFd::borrow_raw(self.fd) }
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

    #[test]
    fn option_structs_match_kernel_layout() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<RawPaddrInfo>(), 152);
        assert_eq!(size_of::<RawStatus>(), 176);
        assert_eq!(offset_of!(RawStatus, sstat_primary), 24);
        assert_eq!(size_of::<RawAssocParams>(), 20);
        // the kernel rejects other sizes for SCTP_PEER_ADDR_PARAMS
        assert_eq!(size_of::<RawPaddrParams>(), 156);
        assert_eq!(offset_of!(PaddrParamsFields, spp_hbinterval), 132);
        assert_eq!(offset_of!(PaddrParamsFields, spp_pathmaxrxt), 136);
        assert_eq!(offset_of!(PaddrParamsFields, spp_pathmtu), 138);
        assert_eq!(offset_of!(PaddrParamsFields, spp_flags), 146);
        assert_eq!(offset_of!(PaddrParamsFields, spp_dscp), 154);
    }

    fn addr_bytes(addr: &str) -> Vec<u8> {
        let addr: std::net::SocketAddr = addr.parse().unwrap();
        let osa: OsSocketAddr = addr.into();
        osa.as_ref().to_vec()
    }

    #[test]
    fn parse_addr_list_entries() {
        let (v4, v6) = ("10.0.1.2:3868", "[2001:db8::1]:3868");
        // offset 1 makes every entry misaligned
        let mut buf = vec![0u8];
        buf.extend(addr_bytes(v4));
        buf.extend(addr_bytes(v6));
        buf.extend(addr_bytes(v4));
        let expected: Vec<std::net::SocketAddr> =
            [v4, v6, v4].iter().map(|a| a.parse().unwrap()).collect();
        assert_eq!(parse_addr_list(&buf[1..], 3).unwrap(), expected);
        // what follows the counted entries is ignored
        assert_eq!(parse_addr_list(&buf[1..], 1).unwrap(), expected[..1]);
        assert!(parse_addr_list(&[], 0).unwrap().is_empty());
    }

    #[test]
    fn parse_addr_list_invalid() {
        let bytes = addr_bytes("10.0.1.2:3868");
        for len in [0, 1, 2, bytes.len() - 1] {
            let err = parse_addr_list(&bytes[..len], 1).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "len {len}");
        }
        // more entries announced than present
        assert!(parse_addr_list(&bytes, 2).is_err());
        let mut unknown = bytes.clone();
        unknown[..2].copy_from_slice(&(libc::AF_UNIX as u16).to_ne_bytes());
        assert!(parse_addr_list(&unknown, 1).is_err());
    }

    #[test]
    fn option_states_convert() {
        for (raw, state) in [
            (0, AssocStatusState::Empty),
            (1, AssocStatusState::Closed),
            (2, AssocStatusState::CookieWait),
            (3, AssocStatusState::CookieEchoed),
            (4, AssocStatusState::Established),
            (5, AssocStatusState::ShutdownPending),
            (6, AssocStatusState::ShutdownSent),
            (7, AssocStatusState::ShutdownReceived),
            (8, AssocStatusState::ShutdownAckSent),
            (99, AssocStatusState::Other(99)),
        ] {
            assert_eq!(AssocStatusState::from_raw(raw), state);
        }
        for (raw, state) in [
            (0, PathState::Inactive),
            (1, PathState::PotentiallyFailed),
            (2, PathState::Active),
            (3, PathState::Unconfirmed),
            (0xffff, PathState::Other(0xffff)),
        ] {
            assert_eq!(PathState::from_raw(raw), state);
        }
    }

    #[test]
    fn path_params_flags() {
        let raw_for = |heartbeat| {
            let params = PathParams {
                heartbeat,
                max_retrans: None,
            };
            let raw = path_params_raw(None, params).unwrap().0;
            (raw.spp_flags, raw.spp_hbinterval)
        };
        assert_eq!(raw_for(None), (0, 0));
        assert_eq!(raw_for(Some(Heartbeat::Disabled)), (SPP_HB_DISABLE, 0));
        assert_eq!(raw_for(Some(Heartbeat::Enabled)), (SPP_HB_ENABLE, 0));
        assert_eq!(raw_for(Some(Heartbeat::Interval(50))), (SPP_HB_ENABLE, 50));
        assert_eq!(
            raw_for(Some(Heartbeat::Interval(0))),
            (SPP_HB_ENABLE | SPP_HB_TIME_IS_ZERO, 0)
        );
    }

    #[test]
    fn zero_values_rejected() {
        let zero_path = PathParams {
            max_retrans: Some(0),
            ..Default::default()
        };
        let err = path_params_raw(None, zero_path).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        for params in [
            AssocParams {
                max_retrans: Some(0),
                ..Default::default()
            },
            AssocParams {
                cookie_life_ms: Some(0),
                ..Default::default()
            },
        ] {
            let err = assoc_params_raw(params).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn linger_rounds_up() {
        use std::time::Duration;
        let value = |d| {
            let l = linger_value(d);
            (l.l_onoff, l.l_linger)
        };
        assert_eq!(value(None), (0, 0));
        assert_eq!(value(Some(Duration::ZERO)), (1, 0));
        // must not become 0, which would abort
        assert_eq!(value(Some(Duration::from_millis(1))), (1, 1));
        assert_eq!(value(Some(Duration::from_secs(2))), (1, 2));
        assert_eq!(value(Some(Duration::MAX)), (1, libc::c_int::MAX));
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
