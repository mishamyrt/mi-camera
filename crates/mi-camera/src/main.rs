use mi_camera_video::Hub;
use std::{io, thread};

fn main() -> io::Result<()> {
    let mut rtsp_address = "0.0.0.0:8554".to_owned();
    let mut http_address = "0.0.0.0:8080".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--rtsp-bind" => {
                rtsp_address = args
                    .next()
                    .ok_or_else(|| io::Error::other("missing RTSP address"))?
            }
            "--http-bind" => {
                http_address = args
                    .next()
                    .ok_or_else(|| io::Error::other("missing HTTP address"))?
            }
            "--help" | "-h" => {
                println!("mi-camera-server [--rtsp-bind 0.0.0.0:8554] [--http-bind 0.0.0.0:8080]");
                return Ok(());
            }
            _ => return Err(io::Error::other(format!("unknown argument: {arg}"))),
        }
    }
    mi_camera_control::restore_tracking()?;
    let hubs = [
        Hub::start("/run/video_mainstream/control")?,
        Hub::start("/run/video_substream/control")?,
    ];
    thread::Builder::new()
        .name("http-ptz".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            if let Err(error) = mi_camera_control::serve(&http_address) {
                eprintln!("HTTP server: {error}");
                std::process::exit(1);
            }
        })?;
    mi_camera_rtsp::serve(&rtsp_address, hubs)
}
