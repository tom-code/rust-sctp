use std::io::{Error, Result};
use std::net::ToSocketAddrs;
use std::os::fd::{AsRawFd, RawFd};

use os_socketaddr::OsSocketAddr;


#[derive(Debug)]
pub struct SctpSocket {
    fd: RawFd,
    pub(crate) ppid: u32,
    notification_queue: Option<tokio::sync::mpsc::Sender<Notification>>
}

const SOL_SCTP: libc::c_int = 132;

// setsockopt
const SCTP_SOCKOPT_BINDX_ADD: libc::c_int = 100;
const SCTP_SOCKOPT_CONNECTX: libc::c_int = 110;
const SCTP_EVENTS: libc::c_int = 11;
const SCTP_RTOINFO: i32 = 0;
const SCTP_INIT_MSG: i32 = 2;

// aux info
//const SCTP_INIT: i32 = 0;
const SCTP_SNDRCV: i32 = 1;
const SCTP_SNDINFO: i32 = 2;
//const SCTP_RCVINFO: i32 = 3;
//const SCTP_NXTINFO: i32 = 4;
//const SCTP_PRINFO: i32 = 5;
const SCTP_DSTADDRV4: i32 = 7;

const SCTP_NODELAY: i32 = 3;

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
    error: u32
}
#[derive(Debug)]
pub enum Notification {
    Address(NotificationAddress),
    Shutdown()
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
        Ok(Self { fd, ppid: 0, notification_queue: None })
    }
    pub fn new6() -> Result<Self> {
        let fd = unsafe { libc::socket(libc::PF_INET6, libc::SOCK_STREAM, libc::IPPROTO_SCTP) };
        if fd == -1 {
            return Err(Error::last_os_error());
        }
        Ok(Self { fd, ppid: 0, notification_queue: None })
    }
    pub fn set_ppid(&mut self, ppid: u32) {
        self.ppid = ppid;
    }
    fn setsockopt<T>(&self, level: libc::c_int, option: libc::c_int, value: T) -> Result<()> {
        handle_libc_error(
        unsafe {
            libc::setsockopt(
                self.fd,
                level,
                option,
                &value as *const T as *const libc::c_void,
                std::mem::size_of_val(&value) as libc::socklen_t
                //std::mem::size_of::<T>() as libc::socklen_t
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
                &mut n
                //std::mem::size_of::<T>() as libc::socklen_t
            )
        })
    }
    fn setsockopt_slice(&self, level: libc::c_int, option: libc::c_int, value: &[u8]) -> Result<()> {
        handle_libc_error(
        unsafe {
            libc::setsockopt(
                self.fd,
                level,
                option,
                value as *const _ as *const libc::c_void,
                value.len() as libc::socklen_t
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
    pub fn shutdown(&self, how: std::net::Shutdown) -> Result<()> {
        let flag = match how {
            std::net::Shutdown::Write => libc::SHUT_WR,
            std::net::Shutdown::Read => libc::SHUT_RD,
            std::net::Shutdown::Both => libc::SHUT_RDWR,
        };
        if let Some(q) = &self.notification_queue {
            q.try_send(Notification::Shutdown()).unwrap();
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
                std::mem::size_of::<libc::sockaddr_in>() as u32,
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
                std::mem::size_of::<libc::sockaddr_in>() as u32,
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
        Ok(SctpSocket { fd: r, ppid: 0, notification_queue: None })
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
        //just take enough room on stack to avoid extra allocation
        const SPACE: usize = 512;
        let hbuffer = [0; SPACE];
        //let space = unsafe { (libc::CMSG_SPACE(std::mem::size_of::<SndRcvInfo> as u32) ) as _ };
        //let mut hbuffer = vec![0 as u8; space];

        // zeroed must be used to support linux-musl which has some private padding crap
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = &mut iovec_item;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = &hbuffer as *const _ as *mut libc::c_void;
        msghdr.msg_controllen = SPACE as _;
        unsafe {
            let hlen = libc::CMSG_LEN(std::mem::size_of::<SndInfo>() as u32) as _;
            let cmsg_hdr = libc::CMSG_FIRSTHDR(&msghdr);
            if !cmsg_hdr.is_null() {
                (*cmsg_hdr).cmsg_level = libc::IPPROTO_SCTP;
                (*cmsg_hdr).cmsg_type = SCTP_SNDINFO;
                (*cmsg_hdr).cmsg_len = hlen;

                std::ptr::copy(
                    std::ptr::addr_of!(*info) as *const _,
                    libc::CMSG_DATA(cmsg_hdr),
                    std::mem::size_of::<SndInfo>(),
                );
                msghdr.msg_controllen = hlen;
            }
        }
        let r = unsafe { libc::sendmsg(self.fd, &msghdr as *const _ as *mut libc::msghdr, libc::MSG_DONTWAIT) };
        if r < 0 {
            Err(Error::last_os_error())
        } else {
            Ok(data.len())
        }
    }

    pub fn recvmsg(&self, data: &mut [u8]) -> Result<usize> {
        let mut iovec_item = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };

        //just take enough room on stack to avoid extra allocation
        const SPACE: usize = 512;
        let hbuffer = [0; SPACE];
        //let space = unsafe { (libc::CMSG_SPACE(std::mem::size_of::<SndRcvInfo> as u32) ) as _ };
        //let mut hbuffer = vec![0 as u8; space];

        // zeroed must be used to support linux-musl which has some private padding crap
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = &mut iovec_item;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = &hbuffer as *const _ as *mut libc::c_void;
        msghdr.msg_controllen = SPACE as _;

        let r = unsafe { libc::recvmsg(self.fd, &msghdr as *const _ as *mut libc::msghdr, 0) };
        if r < 0 {
            Err(Error::last_os_error())
        } else {
            unsafe {
                let mut cmsghdr = libc::CMSG_FIRSTHDR(&msghdr as *const libc::msghdr);
                while !cmsghdr.is_null() {
                    if (*cmsghdr).cmsg_level == libc::IPPROTO_SCTP {
                        if (*cmsghdr).cmsg_type == SCTP_SNDRCV {
                            //sndrcv info
                            let data = libc::CMSG_DATA(cmsghdr);
                            let _info = data as *mut SndRcvInfo;
                            //println!("rcv info! {} {}", u32::from_be((*_info).sinfo_ppid), (*_info).sinfo_stream);
                        }
                        if (*cmsghdr).cmsg_type == SCTP_DSTADDRV4 {
                            println!("rcv info! dst");
                        }
                    }
                    cmsghdr = libc::CMSG_NXTHDR(&msghdr as *const libc::msghdr, cmsghdr)
                }
            }
            Ok(r as usize)
        }
    }

    pub fn recvmsg_detailed(&self, data: &mut [u8], info: &mut SndRcvInfo) -> Result<usize> {
        loop {
            let r = self.recvmsg_detailed_i(data, info);
            match r {
                Ok(s) => {
                    if s != 0x99999999 {
                        return r
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
        let osa = unsafe {OsSocketAddr::copy_from_raw(&storage as *const _ as *const libc::sockaddr, len)};
        match osa.into_addr() {
            Some(addr) => Ok(addr),
            None => Err(Error::new(std::io::ErrorKind::Other, "Invalid socket address")),
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
        let osa = unsafe {OsSocketAddr::copy_from_raw(&storage as *const _ as *const libc::sockaddr, len)};
        match osa.into_addr() {
            Some(addr) => Ok(addr),
            None => Err(Error::new(std::io::ErrorKind::Other, "Invalid socket address")),
        }
    }

    fn handle_notification(&self, data: &[u8]) {
        let header = unsafe {
            let header_ptr = data as *const _ as *const NotificationHeader;
            &*header_ptr.cast::<NotificationHeader>()
        };
        println!("not {:?}", header);
        if header.sn_type == 0x8002 {
            let paddr = unsafe {
                let paddr_ptr = data as *const _ as *const IPaddrChange;
                &*paddr_ptr.cast::<IPaddrChange>()
            };
            println!("not {:?}", paddr);
            unsafe {
                let Some(dst) = OsSocketAddr::copy_from_raw(&paddr.spc_addr as *const _ as *const libc::sockaddr, 128).into_addr() else {
                    // not an IPv4/IPv6 address
                    return;
                };
                println!("dst {:?}", dst);
                let n = NotificationAddress {
                    addr: dst.to_socket_addrs().unwrap().next().unwrap(),
                    state: paddr.spc_state,
                    error: paddr.spc_error
                };
                let not = Notification::Address(n);
                if let Some(q) = &self.notification_queue {
                    q.try_send(not).unwrap();
                }
            }
        }
    }

    pub fn recvmsg_detailed_i(&self, data: &mut [u8], info: &mut SndRcvInfo) -> Result<usize> {
        let mut iovec_item = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };

        //just take enough room on stack to avoid extra allocation
        const SPACE: usize = 512;
        let hbuffer = [0; SPACE];
        //let space = unsafe { (libc::CMSG_SPACE(std::mem::size_of::<SndRcvInfo> as u32) ) as _ };
        //let mut hbuffer = vec![0 as u8; space];

        // zeroed must be used to support linux-musl which has some private padding crap
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = &mut iovec_item;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = &hbuffer as *const _ as *mut libc::c_void;
        msghdr.msg_controllen = SPACE as _;

        let r = unsafe { libc::recvmsg(self.fd, &msghdr as *const _ as *mut libc::msghdr, 0) };
        if r < 0 {
            Err(Error::last_os_error())
        } else {
            if msghdr.msg_flags & 0x8000 == 0x8000 {
                self.handle_notification(data);
                return Ok(0x99999999)
            }
            unsafe {
                let mut cmsghdr = libc::CMSG_FIRSTHDR(&msghdr as *const libc::msghdr);
                while !cmsghdr.is_null() {
                    if (*cmsghdr).cmsg_level == libc::IPPROTO_SCTP && (*cmsghdr).cmsg_type == SCTP_SNDRCV {
                        //sndrcv info
                        let data = libc::CMSG_DATA(cmsghdr);
                        let _info = data as *mut SndRcvInfo;
                        core::ptr::write(info, *_info);
                        //println!("rcv info! {} {}", u32::from_be((*_info).sinfo_ppid), (*_info).sinfo_stream);
                    }
                    cmsghdr = libc::CMSG_NXTHDR(&msghdr as *const libc::msghdr, cmsghdr)
                }
            }
            Ok(r as usize)
        }
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

fn read_fd(fd: RawFd, buf: &mut [u8]) -> Result<usize> {
    let rv = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if rv < 0 {
        return Err(Error::last_os_error());
    }
    Ok(rv as usize)
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
        read_fd(self.fd, buf)
    }
}
impl<'a> std::io::Read for &'a SctpSocket {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        read_fd(self.fd, buf)
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
impl<'a> std::io::Write for &'a SctpSocket {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        //write_fd(self.fd, buf)
        self.sendmsg_default(buf)
        //self.write_fd(buf)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
