use std::{
    io,
    io::{ErrorKind, IoSliceMut},
    os::unix::prelude::AsRawFd,
};

use ntp_proto::NtpTimestamp;
use tokio::io::unix::AsyncFd;
use tracing::{debug, instrument, trace, warn};

// Unix uses an epoch located at 1/1/1970-00:00h (UTC) and NTP uses 1/1/1900-00:00h.
// This leads to an offset equivalent to 70 years in seconds
// there are 17 leap years between the two dates so the offset is
const EPOCH_OFFSET: u32 = (70 * 365 + 17) * 86400;

pub struct UdpSocket {
    io: AsyncFd<std::net::UdpSocket>,
}

impl UdpSocket {
    #[instrument(level = "debug", skip(peer_addr))]
    pub async fn new<A, B>(listen_addr: A, peer_addr: B) -> io::Result<UdpSocket>
    where
        A: tokio::net::ToSocketAddrs + std::net::ToSocketAddrs + std::fmt::Debug,
        B: tokio::net::ToSocketAddrs + std::net::ToSocketAddrs + std::fmt::Debug,
    {
        let socket = tokio::net::UdpSocket::bind(&listen_addr).await?;
        debug!(
            local_addr = debug(socket.local_addr().unwrap()),
            "socket bound"
        );
        socket.connect(peer_addr).await?;
        debug!(
            local_addr = debug(socket.local_addr().unwrap()),
            peer_addr = debug(socket.peer_addr().unwrap()),
            "socket connected"
        );
        let socket = socket.into_std()?;

        set_timestamping_options(&socket)?;

        Ok(UdpSocket {
            io: AsyncFd::new(socket)?,
        })
    }

    #[instrument(level = "trace", skip(self, buf), fields(
        local_addr = debug(self.as_ref().local_addr().unwrap()),
        peer_addr = debug(self.as_ref().peer_addr()),
        buf_size = buf.len(),
    ))]
    pub async fn send(&self, buf: &[u8]) -> io::Result<(usize, Option<NtpTimestamp>)> {
        trace!(size = buf.len(), "sending bytes");
        loop {
            let mut guard = self.io.writable().await?;
            match guard.try_io(|inner| inner.get_ref().send(buf)) {
                Ok(result) => match result {
                    Err(e) => {
                        debug!(error = debug(&e), "error sending data");
                        return Err(e);
                    }
                    Ok(size) => {
                        trace!(sent = size, "sent bytes");
                        let send_timestamp = fetch_send_timestamp(self.io.get_ref())?;
                        return Ok((size, send_timestamp));
                    }
                },
                Err(_would_block) => {
                    trace!("blocked after becoming writable, retrying");
                    continue;
                }
            }
        }
    }

    #[instrument(level = "trace", skip(self, buf), fields(
        local_addr = debug(self.as_ref().local_addr().unwrap()),
        peer_addr = debug(self.as_ref().peer_addr()),
        buf_size = buf.len(),
    ))]
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<(usize, Option<NtpTimestamp>)> {
        loop {
            trace!("waiting for socket to become readable");
            let mut guard = self.io.readable().await?;
            let result = match guard.try_io(|inner| recv(inner.get_ref(), buf)) {
                Err(_would_block) => {
                    trace!("blocked after becoming readable, retrying");
                    continue;
                }
                Ok(result) => result,
            };
            match &result {
                Ok((size, ts)) => trace!(size, ts = debug(ts), "received message"),
                Err(e) => debug!(error = debug(e), "error receiving data"),
            }
            return result;
        }
    }
}

impl AsRef<std::net::UdpSocket> for UdpSocket {
    fn as_ref(&self) -> &std::net::UdpSocket {
        self.io.get_ref()
    }
}

/// # Safety
///
/// The given pointer must point to a libc::timespec
unsafe fn read_ntp_timestamp(ptr: *const u8) -> NtpTimestamp {
    let ts: libc::timespec = std::ptr::read_unaligned(ptr as *const _);

    // truncates the higher bits of the i64
    let seconds = (ts.tv_sec as u32).wrapping_add(EPOCH_OFFSET);

    // tv_nsec is always within [0, 1e10)
    let nanos = ts.tv_nsec as u32;

    NtpTimestamp::from_seconds_nanos_since_ntp_era(seconds, nanos)
}

fn set_timestamping_options(udp_socket: &std::net::UdpSocket) -> io::Result<()> {
    let fd = udp_socket.as_raw_fd();

    // our options:
    //  - we want software timestamps to be reported,
    //  - we want both send and receive software timestamps
    let bits = libc::SOF_TIMESTAMPING_SOFTWARE
        | libc::SOF_TIMESTAMPING_RX_SOFTWARE
        | libc::SOF_TIMESTAMPING_TX_SOFTWARE;

    unsafe {
        cvt(libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TIMESTAMPING,
            &bits as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ))?
    };

    Ok(())
}

fn cvt(t: libc::c_int) -> crate::io::Result<libc::c_int> {
    match t {
        -1 => Err(std::io::Error::last_os_error()),
        _ => Ok(t),
    }
}

fn recv(socket: &std::net::UdpSocket, buf: &mut [u8]) -> io::Result<(usize, Option<NtpTimestamp>)> {
    let mut buf_slice = IoSliceMut::new(buf);

    // could be on the stack if const extern fn is stable
    let control_size =
        unsafe { libc::CMSG_SPACE((3 * std::mem::size_of::<libc::timespec>()) as _) } as usize;
    let mut control_buf = vec![0; control_size];
    let mut mhdr = libc::msghdr {
        msg_control: control_buf.as_mut_ptr().cast::<libc::c_void>(),
        msg_controllen: control_buf.len(),
        msg_iov: (&mut buf_slice as *mut IoSliceMut).cast::<libc::iovec>(),
        msg_iovlen: 1,
        msg_flags: 0,
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
    };

    // loops for when we receive an interrupt during the recv
    let bytes_read = loop {
        match cvt(unsafe { libc::recvmsg(socket.as_raw_fd(), &mut mhdr, 0) } as _) {
            Err(e) => {
                if let ErrorKind::Interrupted = e.kind() {
                    // retry when the recv was interrupted
                    trace!("recv was interrupted, retrying");
                    continue;
                } else {
                    return Err(e);
                }
            }

            Ok(bytes_read) => break bytes_read,
        }
    };

    if mhdr.msg_flags & libc::MSG_TRUNC > 0 {
        warn!(
            max_len = buf.len(),
            "truncated packet because it was larger than expected",
        );
    }

    if mhdr.msg_flags & libc::MSG_CTRUNC > 0 {
        warn!("truncated control messages");
    }

    // Loops through the control messages, but we should only get a single message
    let mut recv_ts = None;
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&mhdr).as_ref() };
    while let Some(msg) = cmsg {
        if let (libc::SOL_SOCKET, libc::SO_TIMESTAMPING) = (msg.cmsg_level, msg.cmsg_type) {
            // Safety: SCM_TIMESTAMPING always has a timespec in the data, so this operation should be safe
            recv_ts = Some(unsafe { read_ntp_timestamp(libc::CMSG_DATA(msg)) });

            break;
        }

        // grab the next control message
        cmsg = unsafe { libc::CMSG_NXTHDR(&mhdr, msg).as_ref() };
    }

    Ok((bytes_read as usize, recv_ts))
}

fn fetch_send_timestamp(socket: &std::net::UdpSocket) -> io::Result<Option<NtpTimestamp>> {
    // TODO: I don't understand why 90 is the right number, but that is what `recvmsg` reports
    let mut buf = [0u8; 90];
    let mut buf_slice = IoSliceMut::new(&mut buf);

    // could be on the stack if const extern fn is stable
    let timestamp_control_size =
        unsafe { libc::CMSG_SPACE((3 * std::mem::size_of::<libc::timespec>()) as _) } as usize;

    let control_size = std::mem::size_of::<ntp_proto::NtpHeader>() + timestamp_control_size;

    let mut control_buf = vec![0; control_size];
    let mut mhdr = libc::msghdr {
        msg_control: control_buf.as_mut_ptr().cast::<libc::c_void>(),
        msg_controllen: control_buf.len(),
        msg_iov: (&mut buf_slice as *mut IoSliceMut).cast::<libc::iovec>(),
        msg_iovlen: 1,
        msg_flags: 0,
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
    };

    let mut try_recvmsg = || unsafe {
        // NOTE: we're receiving on MSG_ERRQUEUE, that is different from the receive timestamps
        cvt(libc::recvmsg(socket.as_raw_fd(), &mut mhdr, libc::MSG_ERRQUEUE) as _)
    };

    // loops for when we receive an interrupt during the recv
    while let Err(e) = try_recvmsg() {
        if let ErrorKind::Interrupted = e.kind() {
            // retry when the recv was interrupted
            trace!("recv was interrupted, retrying");
            continue;
        } else {
            return Err(e);
        }
    }

    if mhdr.msg_flags & libc::MSG_TRUNC > 0 {
        warn!("truncated packet because it was larger than expected",);
    }

    if mhdr.msg_flags & libc::MSG_CTRUNC > 0 {
        warn!("truncated control messages");
    }

    // Loops through the control messages, but we should only get a single message
    let mut send_ts = None;
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&mhdr).as_ref() };
    while let Some(msg) = cmsg {
        if let (libc::SOL_SOCKET, libc::SO_TIMESTAMPING) = (msg.cmsg_level, msg.cmsg_type) {
            // Safety: SCM_TIMESTAMP always has a timespec in the data, so this operation should be safe
            send_ts = Some(unsafe { read_ntp_timestamp(libc::CMSG_DATA(msg)) });

            break;
        }

        // grab the next control message
        cmsg = unsafe { libc::CMSG_NXTHDR(&mhdr, msg).as_ref() };
    }

    Ok(send_ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamping_reasonable() {
        tokio_test::block_on(async {
            let a = UdpSocket::new("127.0.0.1:8000", "127.0.0.1:8001")
                .await
                .unwrap();
            let b = UdpSocket::new("127.0.0.1:8001", "127.0.0.1:8000")
                .await
                .unwrap();

            tokio::spawn(async move {
                a.send(&[1; 48]).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                a.send(&[2; 48]).await.unwrap();
            });

            let mut buf = [0; 48];
            let (s1, t1) = b.recv(&mut buf).await.unwrap();
            let (s2, t2) = b.recv(&mut buf).await.unwrap();
            assert_eq!(s1, 48);
            assert_eq!(s2, 48);

            let t1 = t1.unwrap();
            let t2 = t2.unwrap();
            let delta = t2 - t1;

            assert!(delta.to_seconds() > 0.15 && delta.to_seconds() < 0.25);
        });
    }
}
