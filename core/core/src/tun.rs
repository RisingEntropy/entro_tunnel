//! Cross-platform TUN (layer-3 virtual NIC) abstraction.
//!
//! * **Linux** — `/dev/net/tun` with `IFF_TUN | IFF_NO_PI`.
//! * **macOS** — `utun` via a `PF_SYSTEM` / `SYSPROTO_CONTROL` socket. Note: utun
//!   prepends a 4-byte address-family header to every packet; we add/strip it so
//!   the public API deals in raw IP packets, matching the Linux behaviour.
//! * **Windows** — Wintun (`wintun.dll`): delivers/accepts raw IP packets with
//!   no address-family prefix (same contract as Linux).
//!
//! `recv`/`send` take `&self` so the device can be shared (via `Arc`) between a
//! reader task and a writer task. IP address / MTU / routes are configured by
//! the client's `netcfg`, not here.

use std::net::Ipv4Addr;

/// Parameters for bringing up a TUN device.
#[derive(Debug, Clone)]
pub struct TunConfig {
    /// Requested device name (e.g. `et0` on Linux); ignored where the OS picks
    /// the name (macOS `utunN`).
    pub name: String,
    pub ip: Ipv4Addr,
    pub prefix_len: u8,
    pub mtu: u16,
}

// ----------------------------------------------------------------------------
// Linux
// ----------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    use super::TunConfig;
    use crate::{Error, Result};
    use std::os::unix::io::{AsRawFd, RawFd};
    use tokio::io::unix::AsyncFd;

    const IFF_TUN: i16 = 0x0001;
    const IFF_NO_PI: i16 = 0x1000;
    const TUNSETIFF: libc::c_ulong = 0x400454ca;

    struct Fd(RawFd);
    impl AsRawFd for Fd {
        fn as_raw_fd(&self) -> RawFd {
            self.0
        }
    }
    impl Drop for Fd {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }

    pub struct TunDevice {
        afd: AsyncFd<Fd>,
        name: String,
    }

    impl TunDevice {
        pub async fn create(cfg: &TunConfig) -> Result<Self> {
            let fd = unsafe {
                libc::open(
                    b"/dev/net/tun\0".as_ptr() as *const libc::c_char,
                    libc::O_RDWR | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(Error::Io(std::io::Error::last_os_error()));
            }

            let mut ifr = [0u8; 40]; // struct ifreq: 16-byte name + 24-byte union
            let nb = cfg.name.as_bytes();
            let n = nb.len().min(15);
            ifr[..n].copy_from_slice(&nb[..n]);
            ifr[16..18].copy_from_slice(&(IFF_TUN | IFF_NO_PI).to_ne_bytes());

            if unsafe { libc::ioctl(fd, TUNSETIFF, ifr.as_mut_ptr()) } < 0 {
                let e = std::io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(Error::Io(e));
            }

            let end = ifr[..16].iter().position(|&b| b == 0).unwrap_or(16);
            let name = String::from_utf8_lossy(&ifr[..end]).into_owned();

            Ok(Self {
                afd: AsyncFd::new(Fd(fd))?,
                name,
            })
        }

        pub fn name(&self) -> &str {
            &self.name
        }

        pub async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
            loop {
                let mut guard = self.afd.readable().await?;
                match guard.try_io(|inner| {
                    let n = unsafe {
                        libc::read(
                            inner.get_ref().as_raw_fd(),
                            buf.as_mut_ptr() as *mut libc::c_void,
                            buf.len(),
                        )
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(res) => return res.map_err(Error::Io),
                    Err(_would_block) => continue,
                }
            }
        }

        pub async fn send(&self, pkt: &[u8]) -> Result<usize> {
            loop {
                let mut guard = self.afd.writable().await?;
                match guard.try_io(|inner| {
                    let n = unsafe {
                        libc::write(
                            inner.get_ref().as_raw_fd(),
                            pkt.as_ptr() as *const libc::c_void,
                            pkt.len(),
                        )
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(res) => return res.map_err(Error::Io),
                    Err(_would_block) => continue,
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------
// macOS (utun)
// ----------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod imp {
    use super::TunConfig;
    use crate::{Error, Result};
    use std::os::unix::io::{AsRawFd, RawFd};
    use tokio::io::unix::AsyncFd;

    const UTUN_CONTROL_NAME: &[u8] = b"com.apple.net.utun_control";

    struct Fd(RawFd);
    impl AsRawFd for Fd {
        fn as_raw_fd(&self) -> RawFd {
            self.0
        }
    }
    impl Drop for Fd {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }

    pub struct TunDevice {
        afd: AsyncFd<Fd>,
        name: String,
    }

    impl TunDevice {
        pub async fn create(_cfg: &TunConfig) -> Result<Self> {
            unsafe {
                let fd = libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL);
                if fd < 0 {
                    return Err(Error::Io(std::io::Error::last_os_error()));
                }

                let mut info: libc::ctl_info = std::mem::zeroed();
                std::ptr::copy_nonoverlapping(
                    UTUN_CONTROL_NAME.as_ptr(),
                    info.ctl_name.as_mut_ptr() as *mut u8,
                    UTUN_CONTROL_NAME.len(),
                );
                if libc::ioctl(fd, libc::CTLIOCGINFO, &mut info) < 0 {
                    let e = std::io::Error::last_os_error();
                    libc::close(fd);
                    return Err(Error::Io(e));
                }

                let mut addr: libc::sockaddr_ctl = std::mem::zeroed();
                addr.sc_len = std::mem::size_of::<libc::sockaddr_ctl>() as u8;
                addr.sc_family = libc::AF_SYSTEM as u8;
                addr.ss_sysaddr = libc::AF_SYS_CONTROL as u16;
                addr.sc_id = info.ctl_id;
                addr.sc_unit = 0; // 0 → kernel assigns the next free utunN

                if libc::connect(
                    fd,
                    &addr as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_ctl>() as libc::socklen_t,
                ) < 0
                {
                    let e = std::io::Error::last_os_error();
                    libc::close(fd);
                    return Err(Error::Io(e));
                }

                // Read back the assigned interface name (utunN).
                let mut ifname = [0u8; 32];
                let mut len = ifname.len() as libc::socklen_t;
                let name = if libc::getsockopt(
                    fd,
                    libc::SYSPROTO_CONTROL,
                    libc::UTUN_OPT_IFNAME,
                    ifname.as_mut_ptr() as *mut libc::c_void,
                    &mut len,
                ) == 0
                {
                    let end = ifname.iter().position(|&b| b == 0).unwrap_or(0);
                    String::from_utf8_lossy(&ifname[..end]).into_owned()
                } else {
                    "utun".to_string()
                };

                let flags = libc::fcntl(fd, libc::F_GETFL, 0);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);

                Ok(Self {
                    afd: AsyncFd::new(Fd(fd))?,
                    name,
                })
            }
        }

        pub fn name(&self) -> &str {
            &self.name
        }

        /// Reads one packet, stripping the 4-byte utun address-family header.
        pub async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
            let mut tmp = vec![0u8; buf.len() + 4];
            loop {
                let mut guard = self.afd.readable().await?;
                let n = match guard.try_io(|inner| {
                    let n = unsafe {
                        libc::read(
                            inner.get_ref().as_raw_fd(),
                            tmp.as_mut_ptr() as *mut libc::c_void,
                            tmp.len(),
                        )
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(res) => res.map_err(Error::Io)?,
                    Err(_would_block) => continue,
                };
                if n < 4 {
                    return Ok(0);
                }
                let payload = n - 4;
                buf[..payload].copy_from_slice(&tmp[4..n]);
                return Ok(payload);
            }
        }

        /// Writes one packet, prepending the 4-byte utun address-family header.
        pub async fn send(&self, pkt: &[u8]) -> Result<usize> {
            if pkt.is_empty() {
                return Ok(0);
            }
            let af: u32 = if pkt[0] >> 4 == 6 {
                libc::AF_INET6 as u32
            } else {
                libc::AF_INET as u32
            };
            let mut framed = Vec::with_capacity(pkt.len() + 4);
            framed.extend_from_slice(&af.to_be_bytes());
            framed.extend_from_slice(pkt);
            loop {
                let mut guard = self.afd.writable().await?;
                match guard.try_io(|inner| {
                    let n = unsafe {
                        libc::write(
                            inner.get_ref().as_raw_fd(),
                            framed.as_ptr() as *const libc::c_void,
                            framed.len(),
                        )
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(res) => return Ok(res.map_err(Error::Io)?.saturating_sub(4)),
                    Err(_would_block) => continue,
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Windows (Wintun)
// ----------------------------------------------------------------------------
#[cfg(target_os = "windows")]
mod imp {
    use super::TunConfig;
    use crate::{Error, Result};
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    /// Wintun ring-buffer capacity (must be a power of two in [128 KiB, 64 MiB]).
    const RING_CAPACITY: u32 = 4 * 1024 * 1024;

    /// A live Wintun adapter session. Wintun's receive is blocking, so a
    /// dedicated OS thread drains packets into a channel; `send` writes directly
    /// to the ring (non-blocking).
    pub struct TunDevice {
        session: Arc<wintun::Session>,
        rx: Mutex<mpsc::Receiver<Vec<u8>>>,
        name: String,
        reader: Option<std::thread::JoinHandle<()>>,
    }

    impl TunDevice {
        pub async fn create(cfg: &TunConfig) -> Result<Self> {
            let name = if cfg.name.is_empty() {
                "et0".to_string()
            } else {
                cfg.name.clone()
            };
            // load()/Adapter::create()/start_session() are blocking FFI (DLL
            // load, kernel adapter install/PnP, 4 MiB ring alloc) — keep them
            // off the async worker.
            tokio::task::spawn_blocking(move || {
                let wintun = unsafe { wintun::load() }
                    .map_err(|e| Error::Transport(format!("load wintun.dll: {e}")))?;

                // Reuse an existing adapter of the same name, else create one.
                let adapter = match wintun::Adapter::open(&wintun, &name) {
                    Ok(a) => a,
                    Err(_) => wintun::Adapter::create(&wintun, &name, "EntroTunnel", None)
                        .map_err(|e| Error::Transport(format!("create wintun adapter: {e}")))?,
                };

                let session = Arc::new(
                    adapter
                        .start_session(RING_CAPACITY)
                        .map_err(|e| Error::Transport(format!("start wintun session: {e}")))?,
                );

                let (tx, rx) = mpsc::channel::<Vec<u8>>(1024);
                let reader = session.clone();
                let handle = std::thread::spawn(move || loop {
                    match reader.receive_blocking() {
                        Ok(packet) => {
                            if tx.blocking_send(packet.bytes().to_vec()).is_err() {
                                break; // receiver dropped
                            }
                        }
                        Err(_) => break, // session shut down
                    }
                });

                Ok(Self {
                    session,
                    rx: Mutex::new(rx),
                    name,
                    reader: Some(handle),
                })
            })
            .await
            .map_err(|e| Error::Transport(format!("wintun setup join: {e}")))?
        }

        pub fn name(&self) -> &str {
            &self.name
        }

        pub async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
            let pkt = self
                .rx
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| Error::Transport("wintun reader stopped".into()))?;
            if pkt.len() > buf.len() {
                return Err(Error::Transport(format!(
                    "tun recv: packet {} bytes exceeds buffer {}",
                    pkt.len(),
                    buf.len()
                )));
            }
            buf[..pkt.len()].copy_from_slice(&pkt);
            Ok(pkt.len())
        }

        pub async fn send(&self, pkt: &[u8]) -> Result<usize> {
            // Wintun's send-packet size is a u16; reject oversized packets
            // rather than wrapping the cast (which would panic in copy_from_slice).
            let len = u16::try_from(pkt.len()).map_err(|_| {
                Error::Transport(format!("wintun send: packet {} bytes exceeds 65535", pkt.len()))
            })?;
            let mut packet = self
                .session
                .allocate_send_packet(len)
                .map_err(|e| Error::Transport(format!("wintun alloc: {e}")))?;
            packet.bytes_mut().copy_from_slice(pkt);
            self.session.send_packet(packet);
            Ok(pkt.len())
        }
    }

    impl Drop for TunDevice {
        fn drop(&mut self) {
            // Unblock receive_blocking, then join so the thread (and its
            // Arc<Session> clone) is gone before we return — deterministic
            // teardown across reconnects.
            let _ = self.session.shutdown();
            if let Some(handle) = self.reader.take() {
                let _ = handle.join();
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Other platforms — scaffold
// ----------------------------------------------------------------------------
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod imp {
    use super::TunConfig;
    use crate::{Error, Result};

    pub struct TunDevice {
        name: String,
    }

    impl TunDevice {
        pub async fn create(_cfg: &TunConfig) -> Result<Self> {
            Err(Error::NotImplemented("TUN device not implemented for this OS"))
        }
        pub fn name(&self) -> &str {
            &self.name
        }
        pub async fn recv(&self, _buf: &mut [u8]) -> Result<usize> {
            Err(Error::NotImplemented("tun recv on this OS"))
        }
        pub async fn send(&self, _pkt: &[u8]) -> Result<usize> {
            Err(Error::NotImplemented("tun send on this OS"))
        }
    }
}

pub use imp::TunDevice;
