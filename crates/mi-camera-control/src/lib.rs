//! PTZ and auto tracking HTTP API backed by the camera's stock `mortoxc` controller.
//!
//! Tracking state is saved on the SD card and can be restored before serving requests.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicU32, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static CONTROLLER: Mutex<()> = Mutex::new(());
const TIMEOUT: Duration = Duration::from_secs(2);
const TRACKING_STATE: &str = "/mnt/sdcard/mi-camera-server/tracking";

#[derive(Debug, PartialEq)]
struct Position {
    id: i32,
    result: i32,
    horizontal: i32,
    vertical: i32,
    event: i32,
}

impl Position {
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim().strip_prefix('[')?.strip_suffix(']')?;
        let numbers: Vec<i32> = value
            .split(',')
            .map(|part| part.trim().parse())
            .collect::<Result<_, _>>()
            .ok()?;
        let [id, result, horizontal, vertical, event] = numbers.as_slice() else {
            return None;
        };
        if *id < 0
            || !(1..=101).contains(horizontal)
            || !(1..=101).contains(vertical)
            || !(0..=99).contains(event)
        {
            return None;
        }
        Some(Self {
            id: *id,
            result: *result,
            horizontal: *horizontal,
            vertical: *vertical,
            event: *event,
        })
    }

    fn json(&self) -> String {
        format!(
            "{{\"id\":{},\"result\":{},\"horizontal\":{},\"vertical\":{},\"event\":{}}}\n",
            self.id, self.result, self.horizontal, self.vertical, self.event
        )
    }
}

fn operation(path: &str) -> Option<i32> {
    match path {
        "/ptz/left" => Some(1),
        "/ptz/right" => Some(2),
        "/ptz/up" => Some(3),
        "/ptz/down" => Some(4),
        "/ptz/status" => Some(6),
        _ => None,
    }
}

fn mortox(args: &[&str], deadline: Instant) -> io::Result<String> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "motor controller timed out",
        ));
    }
    let mut child = Command::new("/mnt/data/bin/mortoxc")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(io::Error::other("mortoxc failed"));
            }
            let mut output = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .take(1025)
                .read_to_string(&mut output)?;
            if output.len() > 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized motor reply",
                ));
            }
            return Ok(output);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "motor controller timed out",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn control(op: i32) -> io::Result<Position> {
    let _guard = CONTROLLER.lock().unwrap();
    let id = (std::process::id()
        .wrapping_mul(10000)
        .wrapping_add(NEXT_ID.fetch_add(1, Ordering::Relaxed))
        & 0x7fff_ffff) as i32;
    let deadline = Instant::now() + TIMEOUT;
    let value = format!("[{id},{op},1]");
    mortox(&["set", "mem", "default", "motor_cmd", &value], deadline)?;
    // The controller acknowledges before the bounded motor pulse finishes.
    if op != 6 {
        thread::sleep(Duration::from_millis(600));
    }
    loop {
        let reply = mortox(&["get", "mem", "default", "motor_position"], deadline)?;
        if !reply.trim().is_empty() {
            let position = Position::parse(&reply)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid motor reply"))?;
            if position.id == id {
                return if position.result == 0 {
                    Ok(position)
                } else {
                    Err(io::Error::other("motor rejected command"))
                };
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "motor acknowledgement timed out",
            ));
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn switch_state(value: &str) -> Option<bool> {
    match value.trim() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn read_tracking(path: &Path) -> io::Result<bool> {
    let mut value = String::new();
    File::open(path)?.take(17).read_to_string(&mut value)?;
    if value.len() <= 16 {
        if let Some(enabled) = switch_state(&value) {
            return Ok(enabled);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid saved tracking state",
    ))
}

fn save_tracking(path: &Path, enabled: bool) -> io::Result<()> {
    if read_tracking(path).ok() == Some(enabled) {
        return Ok(());
    }
    let temporary = path.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(if enabled { b"on\n" } else { b"off\n" })?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    File::open(
        path.parent()
            .ok_or_else(|| io::Error::other("missing state directory"))?,
    )?
    .sync_all()
}

/// Restore saved auto tracking state, if present, before starting the HTTP server.
pub fn restore_tracking() -> io::Result<()> {
    match read_tracking(Path::new(TRACKING_STATE)) {
        Ok(enabled) => {
            tracking(Some(enabled))?;
            eprintln!("Restored tracking: {}", if enabled { "on" } else { "off" });
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn tracking(set: Option<bool>) -> io::Result<bool> {
    let _guard = CONTROLLER.lock().unwrap();
    let deadline = Instant::now() + TIMEOUT;
    if let Some(enabled) = set {
        mortox(
            &[
                "set",
                "mem",
                "default",
                "track_switch",
                if enabled { "on" } else { "off" },
            ],
            deadline,
        )?;
    }
    loop {
        let value = mortox(&["get", "mem", "default", "track_switch"], deadline)?;
        let enabled = switch_state(&value)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid tracking state"))?;
        if set.is_none_or(|wanted| wanted == enabled) {
            if set.is_some() {
                save_tracking(Path::new(TRACKING_STATE), enabled)?;
            }
            return Ok(enabled);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "tracking state did not change",
            ));
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>), u16> {
    let mut header = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    let header_end = loop {
        if header.len() == 8192 {
            return Err(431);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(400);
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| 400u16)?;
        let mut bytes = [0; 1024];
        let capacity = bytes.len().min(8192 - header.len());
        let count = stream.read(&mut bytes[..capacity]).map_err(|_| 400u16)?;
        if count == 0 {
            return Err(400);
        }
        header.extend_from_slice(&bytes[..count]);
        if let Some(end) = header.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let mut body = header.split_off(header_end);
    let text = std::str::from_utf8(&header).map_err(|_| 400u16)?;
    let mut lines = text.split("\r\n");
    let mut words = lines.next().ok_or(400u16)?.split_whitespace();
    let method = words.next().ok_or(400u16)?;
    let path = words.next().ok_or(400u16)?;
    let version = words.next().ok_or(400u16)?;
    if words.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(400);
    }
    let mut length = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(400);
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(400);
            }
            let size = value.trim().parse::<usize>().map_err(|_| 400u16)?;
            if size > 1024 {
                return Err(413);
            }
            length = Some(size);
        }
    }
    let length = length.unwrap_or(0);
    if body.len() > length {
        return Err(400);
    }
    while body.len() < length {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(400);
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| 400u16)?;
        let mut bytes = [0; 1024];
        let capacity = length - body.len();
        let count = stream.read(&mut bytes[..capacity]).map_err(|_| 400u16)?;
        if count == 0 {
            return Err(400);
        }
        body.extend_from_slice(&bytes[..count]);
    }
    Ok((method.to_owned(), path.to_owned(), body))
}

fn respond(stream: &mut TcpStream, status: u16, body: &str, allow: Option<&str>) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len())?;
    if let Some(method) = allow {
        write!(stream, "Allow: {method}\r\n")?;
    }
    write!(stream, "\r\n{body}")
}

fn handle(mut stream: TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let (method, path, body) = match request(&mut stream) {
        Ok(request) => request,
        Err(status) => {
            return respond(
                &mut stream,
                status,
                "{\"error\":\"invalid request\"}\n",
                None,
            )
        }
    };
    if path == "/health" {
        return if method == "GET" {
            respond(&mut stream, 200, "{\"ok\":true}\n", None)
        } else {
            respond(&mut stream, 405, "{\"error\":\"use GET\"}\n", Some("GET"))
        };
    }
    if path == "/tracking" {
        let set = match method.as_str() {
            "GET" if body.is_empty() => None,
            "POST" => match std::str::from_utf8(&body).ok().and_then(switch_state) {
                Some(enabled) => Some(enabled),
                None => {
                    return respond(
                        &mut stream,
                        400,
                        "{\"error\":\"expected on or off\"}\n",
                        None,
                    )
                }
            },
            "GET" => return respond(&mut stream, 400, "{\"error\":\"unexpected body\"}\n", None),
            _ => {
                return respond(
                    &mut stream,
                    405,
                    "{\"error\":\"use GET or POST\"}\n",
                    Some("GET, POST"),
                )
            }
        };
        return match tracking(set) {
            Ok(enabled) => respond(
                &mut stream,
                200,
                &format!("{{\"enabled\":{enabled}}}\n"),
                None,
            ),
            Err(error) => controller_error(&mut stream, error),
        };
    }
    let Some(op) = operation(&path) else {
        return respond(&mut stream, 404, "{\"error\":\"unknown endpoint\"}\n", None);
    };
    let required = if op == 6 { "GET" } else { "POST" };
    if method != required {
        return respond(
            &mut stream,
            405,
            "{\"error\":\"method not allowed\"}\n",
            Some(required),
        );
    }
    match control(op) {
        Ok(position) => respond(&mut stream, 200, &position.json(), None),
        Err(error) => controller_error(&mut stream, error),
    }
}

fn controller_error(stream: &mut TcpStream, error: io::Error) -> io::Result<()> {
    eprintln!("Camera control: {error}");
    let status = if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::TimedOut
    ) {
        503
    } else {
        502
    };
    respond(
        stream,
        status,
        "{\"error\":\"camera controller unavailable or rejected command\"}\n",
        None,
    )
}

/// Serve HTTP with up to four concurrent clients and serialized controller operations.
pub fn serve(address: &str) -> io::Result<()> {
    let listener = TcpListener::bind(address)?;
    serve_connections(listener.incoming())
}

fn serve_connections(connections: impl Iterator<Item = io::Result<TcpStream>>) -> io::Result<()> {
    let clients = Arc::new(AtomicUsize::new(0));
    for stream in connections {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("PTZ accept: {error}");
                continue;
            }
        };
        if clients.fetch_add(1, Ordering::Relaxed) >= 4 {
            clients.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let count = clients.clone();
        if let Err(error) = thread::Builder::new()
            .name("http-client".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                if let Err(error) = handle(stream) {
                    eprintln!("PTZ HTTP: {error}");
                }
                count.fetch_sub(1, Ordering::Relaxed);
            })
        {
            clients.fetch_sub(1, Ordering::Relaxed);
            eprintln!("PTZ client thread: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_is_not_blocked_by_an_idle_client_or_the_controller() {
        let _controller = CONTROLLER.lock().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_connections(listener.incoming().take(2)).unwrap());
        let mut idle = TcpStream::connect(address).unwrap();
        let mut health = TcpStream::connect(address).unwrap();
        health
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        health.write_all(b"GET /health HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        health.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        idle.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        idle.write_all(b"GET /health HTTP/1.1\r\n\r\n").unwrap();
        response.clear();
        idle.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        server.join().unwrap();
    }

    #[test]
    fn saves_tracking_atomically_and_rejects_corrupt_state() {
        let directory =
            std::env::temp_dir().join(format!("mi-camera-tracking-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("tracking");
        save_tracking(&path, false).unwrap();
        assert!(!read_tracking(&path).unwrap());
        fs::create_dir(path.with_extension("tmp")).unwrap();
        assert!(save_tracking(&path, true).is_err());
        assert!(!read_tracking(&path).unwrap());
        fs::remove_dir(path.with_extension("tmp")).unwrap();
        save_tracking(&path, true).unwrap();
        assert!(read_tracking(&path).unwrap());
        for corrupt in ["invalid", "on\noff", "off                 "] {
            fs::write(&path, corrupt).unwrap();
            assert_eq!(
                read_tracking(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tracking_accepts_only_native_switch_values() {
        assert_eq!(switch_state("on\n"), Some(true));
        assert_eq!(switch_state("off"), Some(false));
        for value in ["", "true", "false", "1", "off; reboot", "on\noff"] {
            assert_eq!(switch_state(value), None);
        }
    }

    #[test]
    fn http_body_survives_fragmentation_and_rejects_invalid_framing() {
        fn parse(parts: &[&[u8]]) -> Result<(String, String, Vec<u8>), u16> {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut server, _) = listener.accept().unwrap();
            let parts: Vec<Vec<u8>> = parts.iter().map(|p| p.to_vec()).collect();
            let writer = thread::spawn(move || {
                for part in parts {
                    let _ = client.write_all(&part);
                    thread::sleep(Duration::from_millis(5));
                }
                let _ = client.shutdown(std::net::Shutdown::Write);
            });
            let result = request(&mut server);
            writer.join().unwrap();
            result
        }
        assert_eq!(
            parse(&[
                b"POST /tracking HTTP/1.1\r\nContent-Length: 3\r\n\r\no",
                b"ff"
            ])
            .unwrap(),
            ("POST".into(), "/tracking".into(), b"off".to_vec())
        );
        assert_eq!(
            parse(&[b"POST /tracking HTTP/1.1\r\nContent-Length: 3\r\n\r\nof"]),
            Err(400)
        );
        assert_eq!(
            parse(&[b"POST /tracking HTTP/1.1\r\nContent-Length: 1025\r\n\r\n"]),
            Err(413)
        );
        assert_eq!(
            parse(&[b"POST /tracking HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n"]),
            Err(400)
        );
        assert_eq!(
            parse(&[b"POST /tracking HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"]),
            Err(400)
        );
    }

    #[test]
    fn parses_observed_position_and_failure() {
        let position = Position::parse("[101,0,41,8,72]\n").unwrap();
        assert_eq!(
            (position.id, position.horizontal, position.vertical),
            (101, 41, 8)
        );
        assert_eq!(
            position.json(),
            "{\"id\":101,\"result\":0,\"horizontal\":41,\"vertical\":8,\"event\":72}\n"
        );
        assert_eq!(Position::parse("[1,-6,1,101,0]").unwrap().result, -6);
    }

    #[test]
    fn rejects_malformed_and_out_of_range_positions() {
        for value in [
            "",
            "[1,0,41,8]",
            "[1,0,41,8,1,2]",
            "[1,0,x,8,1]",
            "[1,0,0,8,1]",
            "[1,0,41,102,1]",
            "[1,0,41,8,100]",
            "[-1,0,41,8,1]",
            "[2147483648,0,41,8,1]",
            "[1,0,41,8,1]\nextra",
        ] {
            assert!(Position::parse(value).is_none(), "accepted {value}");
        }
    }

    #[test]
    fn operations_are_allowlisted() {
        for (path, op) in [
            ("left", 1),
            ("right", 2),
            ("up", 3),
            ("down", 4),
            ("status", 6),
        ] {
            assert_eq!(operation(&format!("/ptz/{path}")), Some(op));
        }
        for path in [
            "/ptz/stop",
            "/ptz/reset",
            "/ptz/5",
            "/ptz/left?op=5",
            "/ptz/left/../reset",
        ] {
            assert_eq!(operation(path), None);
        }
    }
}
