//! Serve two video hubs at `/main` and `/sub` over RTSP with interleaved RTP/RTCP.
//!
//! Supports HEVC over TCP and limits the server to four concurrent clients.

use base64::{engine::general_purpose::STANDARD, Engine};
use mi_camera_video::{hevc, Frame, Hub};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Writer = Arc<Mutex<TcpStream>>;
struct Request {
    method: String,
    uri: String,
    headers: HashMap<String, String>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_before(socket: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "RTSP message timed out",
            ));
        }
        socket.set_read_timeout(Some(remaining))?;
        match socket.read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "RTSP message ended",
                ))
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn request(socket: &mut TcpStream, timeout: Duration) -> io::Result<Request> {
    let mut header = Vec::new();
    let deadline = loop {
        socket.set_read_timeout(Some(timeout))?;
        let mut first = [0];
        socket.read_exact(&mut first)?;
        let deadline = Instant::now() + timeout;
        if first[0] == b'$' {
            let mut interleaved = [0; 3];
            read_before(socket, &mut interleaved, deadline)?;
            let mut length = u16::from_be_bytes([interleaved[1], interleaved[2]]) as usize;
            let mut payload = [0; 4096];
            while length > 0 {
                let count = length.min(payload.len());
                read_before(socket, &mut payload[..count], deadline)?;
                length -= count;
            }
            continue;
        }
        header.push(first[0]);
        break deadline;
    };
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= 16384 {
            return Err(invalid("RTSP headers too large"));
        }
        let mut byte = [0];
        read_before(socket, &mut byte, deadline)?;
        header.push(byte[0]);
    }
    let text = std::str::from_utf8(&header).map_err(|_| invalid("invalid RTSP text"))?;
    let mut lines = text.split("\r\n");
    let line: Vec<_> = lines.next().unwrap_or("").split_whitespace().collect();
    if line.len() != 3 || line[2] != "RTSP/1.0" {
        return Err(invalid("invalid RTSP request"));
    }
    let mut headers = HashMap::new();
    for line in lines.filter(|s| !s.is_empty()) {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("invalid header"))?;
        if headers
            .insert(key.to_ascii_lowercase(), value.trim().to_owned())
            .is_some()
        {
            return Err(invalid("duplicate header"));
        }
    }
    headers
        .get("cseq")
        .ok_or_else(|| invalid("missing CSeq"))?
        .parse::<u32>()
        .map_err(|_| invalid("invalid CSeq"))?;
    let length = headers
        .get("content-length")
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|_| invalid("invalid length"))?
        .unwrap_or(0);
    if length > 4096 {
        return Err(invalid("body too large"));
    }
    read_before(socket, &mut vec![0; length], deadline)?;
    Ok(Request {
        method: line[0].into(),
        uri: line[1].into(),
        headers,
    })
}

fn reply(writer: &Writer, req: &Request, status: &str, extra: &str, body: &str) -> io::Result<()> {
    let response = format!("RTSP/1.0 {status}\r\nCSeq: {}\r\nServer: mi-camera-server\r\n{extra}Content-Length: {}\r\n\r\n{body}", req.headers["cseq"], body.len());
    writer.lock().unwrap().write_all(response.as_bytes())
}

fn stream_index(uri: &str) -> Option<usize> {
    let path = if let Some(rest) = uri.strip_prefix("rtsp://") {
        &rest[rest.find('/')?..]
    } else {
        uri
    };
    match path.trim_end_matches('/') {
        "/main" | "/main/trackID=0" => Some(0),
        "/sub" | "/sub/trackID=0" => Some(1),
        _ => None,
    }
}

fn channels(transport: &str) -> Option<(u8, u8)> {
    let mut parts = transport.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("RTP/AVP/TCP") {
        return None;
    }
    if parts.clone().any(|p| p.eq_ignore_ascii_case("multicast")) {
        return None;
    }
    let pair = parts.find_map(|part| part.strip_prefix("interleaved="))?;
    let (a, b) = pair.split_once('-')?;
    let (a, b) = (a.parse::<u8>().ok()?, b.parse::<u8>().ok()?);
    (a != b).then_some((a, b))
}

fn interleave(writer: &Writer, channel: u8, packet: &[u8]) -> io::Result<()> {
    let length = u16::try_from(packet.len()).map_err(|_| invalid("RTP packet too large"))?;
    let mut socket = writer.lock().unwrap();
    socket.write_all(&[b'$', channel, (length >> 8) as u8, length as u8])?;
    socket.write_all(packet)
}

struct Rtp {
    sequence: u16,
    ssrc: u32,
    packets: u32,
    octets: u32,
    buffer: Vec<u8>,
}
impl Rtp {
    fn send(
        &mut self,
        writer: &Writer,
        channel: u8,
        timestamp: u32,
        payload: &[u8],
        marker: bool,
    ) -> io::Result<()> {
        let length =
            u16::try_from(12 + payload.len()).map_err(|_| invalid("RTP packet too large"))?;
        let packet = &mut self.buffer;
        packet.clear();
        packet.extend_from_slice(&[b'$', channel, (length >> 8) as u8, length as u8]);
        packet.extend_from_slice(&[0x80, 96 | if marker { 0x80 } else { 0 }]);
        packet.extend_from_slice(&self.sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        packet.extend_from_slice(payload);
        writer.lock().unwrap().write_all(packet)?;
        self.sequence = self.sequence.wrapping_add(1);
        self.packets = self.packets.wrapping_add(1);
        self.octets = self.octets.wrapping_add(payload.len() as u32);
        Ok(())
    }

    fn report(&self, writer: &Writer, channel: u8, timestamp: u32) -> io::Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut packet = vec![0x80, 200, 0, 6];
        for word in [
            self.ssrc,
            (now.as_secs() + 2_208_988_800) as u32,
            ((u64::from(now.subsec_nanos()) << 32) / 1_000_000_000) as u32,
            timestamp,
            self.packets,
            self.octets,
        ] {
            packet.extend_from_slice(&word.to_be_bytes());
        }
        // One SDES CNAME chunk, padded to a 32-bit boundary.
        packet.extend_from_slice(&[0x81, 202, 0, 4]);
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        packet.extend_from_slice(&[1, 9]);
        packet.extend_from_slice(b"mi-camera");
        packet.push(0);
        interleave(writer, channel, &packet)
    }
}

fn picture_nals(data: &[u8]) -> Vec<&[u8]> {
    // shortcut: SDP supplies parameter sets until go2rtc can unpack FFmpeg's HEVC AP packets.
    hevc::nals(data)
        .into_iter()
        .filter(|nal| !matches!(hevc::nal_type(nal), 32..=34))
        .collect()
}

fn send_video(
    writer: Writer,
    rx: mpsc::Receiver<Arc<Frame>>,
    playing: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    pair: (u8, u8),
    mut rtp: Rtp,
) {
    let mut started = false;
    let mut last_report = Instant::now() - Duration::from_secs(5);
    let result = (|| -> io::Result<()> {
        while !stopped.load(Ordering::Relaxed) {
            let frame = match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(frame) => frame,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "stream ended or client too slow",
                    ))
                }
            };
            if !playing.load(Ordering::Acquire) {
                started = false;
                continue;
            }
            let nals = picture_nals(&frame.data);
            if !started {
                if !nals.iter().any(|n| matches!(hevc::nal_type(n), 16..=23)) {
                    continue;
                }
                started = true;
            }
            // SigmaStar MI_VENC PTS uses microseconds; RTP H.265 uses a 90 kHz clock.
            let timestamp = ((frame.pts / 100) * 9 + ((frame.pts % 100) * 9) / 100) as u32;
            for (i, nal) in nals.iter().enumerate() {
                hevc::packetize(nal, i + 1 == nals.len(), |payload, marker| {
                    rtp.send(&writer, pair.0, timestamp, payload, marker)
                })?;
            }
            if last_report.elapsed() >= Duration::from_secs(5) {
                rtp.report(&writer, pair.1, timestamp)?;
                last_report = Instant::now();
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        eprintln!("RTSP sender: {e}");
    }
    stopped.store(true, Ordering::Relaxed);
    let _ = writer.lock().unwrap().shutdown(Shutdown::Both);
}

fn connection(mut socket: TcpStream, hubs: &[Arc<Hub>; 2]) -> io::Result<()> {
    socket.set_nodelay(true)?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let writer = Arc::new(Mutex::new(socket.try_clone()?));
    let playing = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let mut random = [0; 12];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let session = format!(
        "{:016x}",
        u64::from_ne_bytes(random[..8].try_into().unwrap())
    );
    let ssrc = u32::from_ne_bytes(random[8..].try_into().unwrap());
    let mut setup = None;
    let mut sender = None;
    let result = (|| -> io::Result<()> {
        loop {
            let req = request(&mut socket, Duration::from_secs(60))?;
            if req.method == "OPTIONS" {
                reply(
                    &writer,
                    &req,
                    "200 OK",
                    "Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER\r\n",
                    "",
                )?;
                continue;
            }
            let Some(index) = stream_index(&req.uri) else {
                reply(&writer, &req, "404 Not Found", "", "")?;
                continue;
            };
            match req.method.as_str() {
                "DESCRIBE" => match hubs[index].parameters() {
                    Ok(params) => {
                        let body = format!("v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=Mi Camera\r\nt=0 0\r\na=control:*\r\na=range:npt=0-\r\nm=video 0 RTP/AVP 96\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:96 H265/90000\r\na=fmtp:96 sprop-vps={};sprop-sps={};sprop-pps={}\r\na=control:trackID=0\r\n", STANDARD.encode(&params[0]), STANDARD.encode(&params[1]), STANDARD.encode(&params[2]));
                        let extra = format!(
                            "Content-Type: application/sdp\r\nContent-Base: {}/\r\n",
                            req.uri.trim_end_matches('/')
                        );
                        reply(&writer, &req, "200 OK", &extra, &body)?;
                    }
                    Err(_) => reply(
                        &writer,
                        &req,
                        "503 Service Unavailable",
                        "Retry-After: 1\r\n",
                        "",
                    )?,
                },
                "SETUP" => {
                    if setup.is_some() {
                        reply(&writer, &req, "455 Method Not Valid in This State", "", "")?;
                        continue;
                    }
                    let Some(pair) = req.headers.get("transport").and_then(|s| channels(s)) else {
                        reply(&writer, &req, "461 Unsupported Transport", "", "")?;
                        continue;
                    };
                    setup = Some((index, pair));
                    reply(&writer, &req, "200 OK", &format!("Transport: RTP/AVP/TCP;unicast;interleaved={}-{};ssrc={ssrc:08X}\r\nSession: {session};timeout=60\r\n", pair.0, pair.1), "")?;
                }
                "PLAY" | "TEARDOWN" | "GET_PARAMETER" => {
                    if setup.is_none()
                        || req
                            .headers
                            .get("session")
                            .map(|s| s.split(';').next().unwrap_or("").trim())
                            != Some(session.as_str())
                    {
                        reply(&writer, &req, "454 Session Not Found", "", "")?;
                        continue;
                    }
                    if setup.unwrap().0 != index {
                        reply(&writer, &req, "455 Method Not Valid in This State", "", "")?;
                        continue;
                    }
                    if req.method == "TEARDOWN" {
                        playing.store(false, Ordering::Release);
                    }
                    reply(
                        &writer,
                        &req,
                        "200 OK",
                        &format!("Session: {session}\r\n"),
                        "",
                    )?;
                    if req.method == "TEARDOWN" {
                        break;
                    }
                    if req.method == "PLAY" {
                        playing.store(true, Ordering::Release);
                        if sender.is_none() {
                            let w = writer.clone();
                            let p = playing.clone();
                            let s = stopped.clone();
                            let rx = hubs[index].subscribe();
                            let pair = setup.unwrap().1;
                            let rtp = Rtp {
                                sequence: u16::from_ne_bytes(random[..2].try_into().unwrap()),
                                ssrc,
                                packets: 0,
                                octets: 0,
                                buffer: Vec::new(),
                            };
                            sender = Some(
                                thread::Builder::new()
                                    .name("rtp".into())
                                    .stack_size(128 * 1024)
                                    .spawn(move || send_video(w, rx, p, s, pair, rtp))?,
                            );
                        }
                    }
                }
                _ => reply(&writer, &req, "405 Method Not Allowed", "", "")?,
            }
        }
        Ok(())
    })();
    stopped.store(true, Ordering::Relaxed);
    let _ = socket.shutdown(Shutdown::Both);
    if let Some(sender) = sender {
        let _ = sender.join();
    }
    result
}

/// Serve the main and substream hubs, in that order, until the listener fails.
pub fn serve(address: &str, hubs: [Arc<Hub>; 2]) -> io::Result<()> {
    let listener = TcpListener::bind(address)?;
    eprintln!(
        "RTSP listening on {} (/main, /sub; TCP transport)",
        listener.local_addr()?
    );
    let clients = Arc::new(AtomicUsize::new(0));
    for socket in listener.incoming() {
        let socket = socket?;
        if clients.fetch_add(1, Ordering::Relaxed) >= 4 {
            clients.fetch_sub(1, Ordering::Relaxed);
            let _ = socket.shutdown(Shutdown::Both);
            continue;
        }
        let hubs = hubs.clone();
        let count = clients.clone();
        if let Err(error) = thread::Builder::new()
            .name("rtsp".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                if let Err(error) = connection(socket, &hubs) {
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                    ) {
                        eprintln!("RTSP client: {error}");
                    }
                }
                count.fetch_sub(1, Ordering::Relaxed);
            })
        {
            clients.fetch_sub(1, Ordering::Relaxed);
            eprintln!("RTSP client thread: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sockets() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        (listener.accept().unwrap().0, client)
    }

    #[test]
    fn slow_headers_bodies_and_interleaved_packets_have_a_total_deadline() {
        for prefix in [
            &b"OPTIONS /main RTSP/1.0\r\nCSeq: "[..],
            &b"GET_PARAMETER /main RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 8\r\n\r\n"[..],
            &b"$\x01\x00\x08"[..],
        ] {
            let (mut server, mut client) = sockets();
            client.write_all(prefix).unwrap();
            let sender = thread::spawn(move || {
                for byte in b"12345678" {
                    thread::sleep(Duration::from_millis(40));
                    if client.write_all(&[*byte]).is_err() {
                        break;
                    }
                }
            });
            let error = request(&mut server, Duration::from_millis(120))
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "prefix {prefix:?}");
            drop(server);
            sender.join().unwrap();
        }
    }

    #[test]
    fn complete_interleaved_packets_reset_the_deadline_before_the_next_message() {
        let (mut server, mut client) = sockets();
        let sender = thread::spawn(move || {
            for _ in 0..4 {
                client.write_all(b"$\x01\x00\x02ab").unwrap();
                thread::sleep(Duration::from_millis(80));
            }
            client
                .write_all(b"GET_PARAMETER /main RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\no")
                .unwrap();
            thread::sleep(Duration::from_millis(20));
            client.write_all(b"ff").unwrap();
        });
        let req = request(&mut server, Duration::from_millis(200)).unwrap();
        assert_eq!(req.method, "GET_PARAMETER");
        assert_eq!(req.headers["cseq"], "1");
        sender.join().unwrap();
    }

    #[test]
    fn rejects_truncated_interleaved_packets() {
        let (mut server, mut client) = sockets();
        client.write_all(b"$\x01\x00\x04ab").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let error = request(&mut server, Duration::from_millis(200))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn rtp_reuses_its_buffer_and_preserves_framing_and_success_counters() {
        let (socket, mut client) = sockets();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let writer = Arc::new(Mutex::new(socket));
        let mut rtp = Rtp {
            sequence: u16::MAX,
            ssrc: 0x01020304,
            packets: 0,
            octets: 0,
            buffer: Vec::new(),
        };
        let payload = [0x55; 1200];
        rtp.send(&writer, 2, 0x05060708, &payload, true).unwrap();
        let allocation = rtp.buffer.as_ptr();
        let mut bytes = vec![0; 1216];
        client.read_exact(&mut bytes).unwrap();
        assert_eq!(
            bytes[..16],
            [b'$', 2, 4, 188, 0x80, 0xe0, 0xff, 0xff, 5, 6, 7, 8, 1, 2, 3, 4]
        );
        assert_eq!(bytes[16..], payload);
        rtp.send(&writer, 2, 9, b"xy", false).unwrap();
        assert_eq!(rtp.buffer.as_ptr(), allocation);
        let mut bytes = [0; 18];
        client.read_exact(&mut bytes).unwrap();
        assert_eq!(
            bytes,
            [b'$', 2, 0, 14, 0x80, 96, 0, 0, 0, 0, 0, 9, 1, 2, 3, 4, b'x', b'y']
        );
        assert_eq!((rtp.sequence, rtp.packets, rtp.octets), (1, 2, 1202));
        writer.lock().unwrap().shutdown(Shutdown::Write).unwrap();
        assert!(rtp.send(&writer, 2, 10, b"xy", false).is_err());
        assert_eq!((rtp.sequence, rtp.packets, rtp.octets), (1, 2, 1202));
    }

    #[test]
    fn sends_picture_data_without_repeating_sdp_parameter_sets() {
        let mut frame = Vec::new();
        for kind in [32, 33, 34, 19, 1] {
            frame.extend_from_slice(&[0, 0, 0, 1, kind << 1, 1, 0xaa, 0]);
        }
        assert_eq!(
            picture_nals(&frame),
            vec![&[38, 1, 0xaa, 0][..], &[2, 1, 0xaa, 0][..]]
        );
        assert_eq!(hevc::nals(&frame).len(), 5);
    }

    #[test]
    fn routes_and_transport() {
        assert_eq!(
            stream_index("rtsp://10.1.0.214:8554/main/trackID=0"),
            Some(0)
        );
        assert_eq!(stream_index("/sub/"), Some(1));
        assert_eq!(stream_index("/main/unknown"), None);
        assert_eq!(
            channels("RTP/AVP/TCP;unicast;interleaved=2-3"),
            Some((2, 3))
        );
        assert_eq!(channels("RTP/AVP;unicast;client_port=5000-5001"), None);
        assert_eq!(channels("RTP/AVP/TCP;interleaved=0-0"), None);
        assert_eq!(channels("RTP/AVP/TCP;interleaved=256-257"), None);
    }
}
