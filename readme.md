
# async sctp for rust
This is wrapper to use linux kernel sctp module in rust with focus on async/tokio compatibility.
Wrapper does not use lksctp library, but directly libc calls. This simplifies setup of cross compilation.

features:
- bindx - bind multiple IP addresses (for multihoming use)
- connectx - connect with multiple ip addresses (for multihoming use)
- set_ppid - configure default protocol identifier to be used in outgoing requests
- sendmsg with data and sctp_sndinfo
- sendmsg with data and default values
- recvmsg returning data
- implements tokio AsyncWrite and AsyncRead
   - this sounds little strange, but it is usefull for protocols defined for both tcp and sctp to have as unified implementation as possible.
   - this is used for example for diameter stack which could then use AsyncWrite/AsyncRead to send/receive from both tcp and sctp
   - supports stream split for AsyncWrite/AsyncRead use. For sendmsg/recvmsg split is not required(they do not require mutable access)
   - Write of complete message must be obviusly done in single write. It is ok to use write_all.
   - Read with large enough buffer will always read one complete message.
   - Read with smaller buffer than message will return portion of message, next read will return next portion.