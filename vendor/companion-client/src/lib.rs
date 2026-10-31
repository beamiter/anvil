//! Unix IPC only: no network, model, audio, or autonomous fallback body.
use companion_protocol::*;
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
};
fn connect_checked(path: &Path) -> io::Result<UnixStream> {
    use std::os::unix::{
        fs::{FileTypeExt, MetadataExt},
        io::AsRawFd,
    };
    let parent = path.parent().ok_or_else(|| invalid("socket_parent"))?;
    let m = std::fs::symlink_metadata(parent)?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
    {
        return Err(invalid("unsafe_socket_directory"));
    }
    let m = std::fs::symlink_metadata(path)?;
    if !m.file_type().is_socket() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0
    {
        return Err(invalid("unsafe_socket"));
    }
    let stream = UnixStream::connect(path)?;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut _,
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if cred.uid != unsafe { libc::geteuid() } {
        return Err(invalid("wrong_server_uid"));
    }
    Ok(stream)
}
const QUEUE_LIMIT: usize = 128;
fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
pub struct Framed {
    stream: UnixStream,
    input: Vec<u8>,
    output: VecDeque<Vec<u8>>,
    offset: usize,
}
impl Framed {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            input: Vec::new(),
            output: VecDeque::new(),
            offset: 0,
        })
    }
    pub fn queue<T: serde::Serialize>(&mut self, value: &T) -> io::Result<()> {
        if self.output.len() >= QUEUE_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "control backpressure; retry without changing event ID",
            ));
        }
        let b = serde_json::to_vec(value).map_err(|_| invalid("encode"))?;
        if b.len() > MAX_FRAME {
            return Err(invalid("frame_limit"));
        }
        let mut f = (b.len() as u32).to_be_bytes().to_vec();
        f.extend(b);
        self.output.push_back(f);
        Ok(())
    }
    pub fn flush(&mut self) -> io::Result<()> {
        while let Some(f) = self.output.front() {
            match self.stream.write(&f[self.offset..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")),
                Ok(n) => {
                    self.offset += n;
                    if self.offset == f.len() {
                        self.output.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    pub fn read(&mut self) -> io::Result<Vec<Vec<u8>>> {
        let mut frames = Vec::new();
        let mut b = [0; 8192];
        for _ in 0..16 {
            match self.stream.read(&mut b) {
                Ok(0) => {
                    // Deliver complete frames before reporting the following EOF.
                    // A final exact retirement proof must not disappear merely
                    // because the peer closed its write side in the same read.
                    if !frames.is_empty() {
                        return Ok(frames);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "daemon/host disconnected; hide body",
                    ));
                }
                Ok(n) => self.input.extend_from_slice(&b[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
            loop {
                if self.input.len() < 4 {
                    break;
                }
                let n = u32::from_be_bytes(self.input[..4].try_into().unwrap()) as usize;
                if n == 0 || n > MAX_FRAME {
                    return Err(invalid("frame_limit"));
                }
                if self.input.len() < n + 4 {
                    break;
                }
                frames.push(self.input[4..n + 4].to_vec());
                self.input.drain(..n + 4);
                if frames.len() > 200 {
                    return Err(invalid("rate_limit"));
                }
            }
        }
        Ok(frames)
    }
    pub fn pending(&self) -> usize {
        self.output.len()
    }
}
pub struct Client {
    pending_control: VecDeque<Request>,
    pending_geometry: Option<Request>,
    pending_focus: Option<Request>,
    io: Framed,
    host: Host,
    session: String,
    boot: String,
    seq: u64,
    connected: bool,
}
impl Client {
    pub fn connect(
        path: impl AsRef<Path>,
        session: impl Into<String>,
        host: Host,
    ) -> io::Result<Self> {
        let mut c = Self {
            pending_control: VecDeque::new(),
            pending_geometry: None,
            pending_focus: None,
            io: Framed::new(connect_checked(path.as_ref())?)?,
            host: host.clone(),
            session: session.into(),
            boot: String::new(),
            seq: 0,
            connected: true,
        };
        c.send(Request::Hello { host })?;
        Ok(c)
    }
    pub fn connect_compositor(
        path: impl AsRef<Path>,
        session: impl Into<String>,
        host: Host,
    ) -> io::Result<Self> {
        let mut c = Self {
            pending_control: VecDeque::new(),
            pending_geometry: None,
            pending_focus: None,
            io: Framed::new(connect_checked(path.as_ref())?)?,
            host: host.clone(),
            session: session.into(),
            boot: String::new(),
            seq: 0,
            connected: true,
        };
        c.send(Request::CompositorHello { host })?;
        Ok(c)
    }
    /// Explicit feature negotiation; legacy callers continue to use `connect`.
    pub fn connect_with_features(
        path: impl AsRef<Path>,
        session: impl Into<String>,
        host: Host,
        features: Vec<HostFeature>,
    ) -> io::Result<Self> {
        let mut client = Self {
            pending_control: VecDeque::new(),
            pending_geometry: None,
            pending_focus: None,
            io: Framed::new(connect_checked(path.as_ref())?)?,
            host: host.clone(),
            session: session.into(),
            boot: String::new(),
            seq: 0,
            connected: true,
        };
        client.send(Request::HelloWithFeatures { host, features })?;
        Ok(client)
    }
    pub fn connect_desktop(
        path: impl AsRef<Path>,
        session: impl Into<String>,
        source_id: String,
    ) -> io::Result<Self> {
        let mut c = Self {
            pending_control: VecDeque::new(),
            pending_geometry: None,
            pending_focus: None,
            io: Framed::new(connect_checked(path.as_ref())?)?,
            host: Host {
                id: source_id,
                incarnation: 1,
                surface_id: "desktop-observer".into(),
                retirement: RetirementCapability::None,
            },
            session: session.into(),
            boot: String::new(),
            seq: 0,
            connected: true,
        };
        c.send(Request::DesktopHello)?;
        Ok(c)
    }
    pub fn connect_eva(
        path: impl AsRef<Path>,
        session: impl Into<String>,
        source_id: String,
    ) -> io::Result<Self> {
        let mut c = Self {
            pending_control: VecDeque::new(),
            pending_geometry: None,
            pending_focus: None,
            io: Framed::new(connect_checked(path.as_ref())?)?,
            host: Host {
                id: source_id,
                incarnation: 1,
                surface_id: "audio".into(),
                retirement: RetirementCapability::None,
            },
            session: session.into(),
            boot: String::new(),
            seq: 0,
            connected: true,
        };
        c.send(Request::EvaHello)?;
        Ok(c)
    }
    pub fn send(&mut self, body: Request) -> io::Result<()> {
        if self.boot.is_empty()
            && !matches!(
                body,
                Request::Hello { .. }
                    | Request::HelloWithFeatures { .. }
                    | Request::DesktopHello
                    | Request::EvaHello
                    | Request::CompositorHello { .. }
                    | Request::Geometry { .. }
                    | Request::Focus { .. }
            )
        {
            if self.pending_control.len() >= QUEUE_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "handshake backpressure",
                ));
            }
            self.pending_control.push_back(body);
            return Ok(());
        }
        match body {
            Request::Geometry { .. } => {
                self.pending_geometry = Some(body);
                Ok(())
            }
            Request::Focus { .. } => {
                self.pending_focus = Some(body);
                Ok(())
            }
            _ => {
                self.flush_passive()?;
                self.send_now(body)
            }
        }
    }
    fn flush_passive(&mut self) -> io::Result<()> {
        if self.boot.is_empty() {
            return Ok(());
        }
        if let Some(r) = self.pending_geometry.clone() {
            self.send_now(r)?;
            self.pending_geometry = None;
        }
        if let Some(r) = self.pending_focus.clone() {
            self.send_now(r)?;
            self.pending_focus = None;
        }
        Ok(())
    }
    fn send_now(&mut self, body: Request) -> io::Result<()> {
        if !self.connected {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "disconnected"));
        }
        let seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| invalid("sequence_exhausted"))?;
        let e = Envelope {
            protocol_version: VERSION,
            event_id: format!("{}-{}-{}", self.host.id, self.host.incarnation, seq),
            source_id: self.host.id.clone(),
            source_seq: seq,
            boot_id: self.boot.clone(),
            session_id: self.session.clone(),
            body,
        };
        self.io.queue(&e)?;
        self.seq = seq;
        self.io.flush()
    }
    pub fn poll(&mut self) -> io::Result<Vec<Reply>> {
        let result = (|| {
            self.io.flush()?;
            match self.flush_passive() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                other => other?,
            };
            let mut out = vec![];
            for b in self.io.read()? {
                let r: Reply = serde_json::from_slice(&b).map_err(|_| invalid("invalid_reply"))?;
                if let Reply::Accepted {
                    boot_id,
                    session_id,
                    ..
                }
                | Reply::HostAccepted {
                    boot_id,
                    session_id,
                    ..
                } = &r
                {
                    if *session_id != self.session {
                        return Err(invalid("wrong_session"));
                    }
                    self.boot = boot_id.clone();
                }
                out.push(r)
            }
            if !self.boot.is_empty() {
                self.flush_passive()?;
                while let Some(r) = self.pending_control.front().cloned() {
                    self.send_now(r)?;
                    self.pending_control.pop_front();
                }
            }
            Ok(out)
        })();
        if result.is_err() {
            self.connected = false
        }
        result
    }
    pub fn boot_id(&self) -> &str {
        &self.boot
    }
    pub fn connected(&self) -> bool {
        self.connected
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backpressure_preserves_control() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut f = Framed::new(a).unwrap();
        for _ in 0..QUEUE_LIMIT {
            f.queue(&Reply::Ack).unwrap()
        }
        assert_eq!(
            f.queue(&Reply::Ack).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(f.pending(), QUEUE_LIMIT);
    }
    #[test]
    fn framed_roundtrip() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut a = Framed::new(a).unwrap();
        let mut b = Framed::new(b).unwrap();
        a.queue(&Reply::Ack).unwrap();
        a.flush().unwrap();
        assert_eq!(b.read().unwrap().len(), 1);
    }
}
#[cfg(test)]
mod handshake_tests {
    use super::*;
    #[test]
    fn early_controls_wait_for_accepted_boot() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut c = Client {
            pending_control: VecDeque::new(),
            pending_geometry: None,
            pending_focus: None,
            io: Framed::new(a).unwrap(),
            host: Host {
                id: "h".into(),
                incarnation: 1,
                surface_id: "s".into(),
                retirement: RetirementCapability::NativePresented,
            },
            session: "session".into(),
            boot: String::new(),
            seq: 0,
            connected: true,
        };
        let mut server = Framed::new(b).unwrap();
        c.send(Request::Hello {
            host: c.host.clone(),
        })
        .unwrap();
        c.send(Request::Input {
            active: true,
            selecting: false,
        })
        .unwrap();
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        c.send(Request::Geometry {
            revision: 1,
            viewport: rect,
            candidates: vec![rect],
            exclusions: vec![],
        })
        .unwrap();
        assert_eq!(server.read().unwrap().len(), 1);
        server
            .queue(&Reply::Accepted {
                boot_id: "boot".into(),
                companion_id: "eva".into(),
                session_id: "session".into(),
            })
            .unwrap();
        server.flush().unwrap();
        c.poll().unwrap();
        let frames = server.read().unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| decode(f).unwrap().boot_id == "boot"));
    }
}
#[cfg(test)]
mod eof_tests {
    use super::*;
    #[test]
    fn complete_final_frame_survives_eof() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut a = Framed::new(a).unwrap();
        let mut b = Framed::new(b).unwrap();
        a.queue(&Reply::Ack).unwrap();
        a.flush().unwrap();
        a.stream.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(b.read().unwrap().len(), 1);
        assert_eq!(b.read().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}
