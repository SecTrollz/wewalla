//! RFC 6455 server side: text frames out, masked control/text frames in. Fragmentation,
//! binary frames and extensions are refused rather than half-supported.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MAX_CLIENT_PAYLOAD: usize = 4096;
const OP_TEXT: u8 = 0x1;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

pub fn encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 10);
    f.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => f.push(n as u8),
        n if n <= 0xFFFF => {
            f.push(126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            f.push(127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    f.extend_from_slice(payload);
    f
}

#[derive(Debug, PartialEq, Eq)]
pub enum WsErr {
    Protocol(u16),
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub opcode: u8,
    pub payload: Vec<u8>,
}

pub fn read_frame<R: Read>(r: &mut R) -> Result<Frame, WsErr> {
    let mut h = [0u8; 2];
    r.read_exact(&mut h).map_err(|_| WsErr::Closed)?;
    let (fin, rsv, opcode) = (h[0] & 0x80 != 0, h[0] & 0x70, h[0] & 0x0F);
    let masked = h[1] & 0x80 != 0;
    let mut len = u64::from(h[1] & 0x7F);
    if rsv != 0 || !masked || !fin {
        return Err(WsErr::Protocol(1002)); // RSV bits, unmasked client frame, or fragmentation
    }
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b).map_err(|_| WsErr::Closed)?;
        len = u64::from(u16::from_be_bytes(b));
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b).map_err(|_| WsErr::Closed)?;
        len = u64::from_be_bytes(b);
    }
    if len > MAX_CLIENT_PAYLOAD as u64 {
        return Err(WsErr::Protocol(1009));
    }
    if opcode >= 0x8 && len > 125 {
        return Err(WsErr::Protocol(1002));
    }
    if !matches!(opcode, OP_TEXT | OP_CLOSE | OP_PING | OP_PONG) {
        return Err(WsErr::Protocol(1003)); // binary, continuation, reserved
    }
    let mut mask = [0u8; 4];
    r.read_exact(&mut mask).map_err(|_| WsErr::Closed)?;
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).map_err(|_| WsErr::Closed)?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Ok(Frame { opcode, payload })
}

fn close_frame(code: u16) -> Vec<u8> {
    encode_frame(OP_CLOSE, &code.to_be_bytes())
}

/// Run a WebSocket session until either side ends it. `on_text` may return a reply.
pub fn serve<F>(
    stream: TcpStream,
    rx: &Receiver<Arc<str>>,
    first: Option<Arc<str>>,
    stop: &AtomicBool,
    on_text: F,
) where
    F: Fn(&str) -> Option<String> + Send + 'static,
{
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let writer = Arc::new(Mutex::new(stream));
    let done = Arc::new(AtomicBool::new(false));

    let (w2, d2) = (Arc::clone(&writer), Arc::clone(&done));
    let reader = std::thread::spawn(move || {
        let mut rs = read_half;
        let _ = rs.set_read_timeout(Some(Duration::from_secs(90)));
        loop {
            match read_frame(&mut rs) {
                Ok(Frame {
                    opcode: OP_TEXT,
                    payload,
                }) => {
                    if let Some(reply) = std::str::from_utf8(&payload).ok().and_then(&on_text) {
                        if w2
                            .lock()
                            .map(|mut w| w.write_all(&encode_frame(OP_TEXT, reply.as_bytes())))
                            .map_or(true, |r| r.is_err())
                        {
                            break;
                        }
                    }
                }
                Ok(Frame {
                    opcode: OP_PING,
                    payload,
                }) => {
                    if w2
                        .lock()
                        .map(|mut w| w.write_all(&encode_frame(OP_PONG, &payload)))
                        .map_or(true, |r| r.is_err())
                    {
                        break;
                    }
                }
                Ok(Frame {
                    opcode: OP_CLOSE, ..
                }) => {
                    if let Ok(mut w) = w2.lock() {
                        let _ = w.write_all(&close_frame(1000));
                    }
                    break;
                }
                Ok(_) => {}
                Err(WsErr::Protocol(code)) => {
                    if let Ok(mut w) = w2.lock() {
                        let _ = w.write_all(&close_frame(code));
                    }
                    break;
                }
                Err(WsErr::Closed) => break,
            }
        }
        d2.store(true, Ordering::Relaxed);
    });

    let send = |payload: &[u8], op: u8| {
        writer
            .lock()
            .map(|mut w| w.write_all(&encode_frame(op, payload)))
            .is_ok_and(|r| r.is_ok())
    };
    let mut alive = first.is_none_or(|f| send(f.as_bytes(), OP_TEXT));
    let mut idle_ticks = 0u32;
    while alive && !done.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(m) if m.is_empty() => {}
            Ok(m) => {
                idle_ticks = 0;
                alive = send(m.as_bytes(), OP_TEXT);
            }
            Err(RecvTimeoutError::Timeout) => {
                idle_ticks += 1;
                if idle_ticks >= 25 {
                    idle_ticks = 0;
                    alive = send(b"", OP_PING);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Ok(mut w) = writer.lock() {
        if stop.load(Ordering::Relaxed) {
            let _ = w.write_all(&close_frame(1001));
        }
        let _ = w.shutdown(Shutdown::Both);
    }
    let _ = reader.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn masked(opcode: u8, payload: &[u8], fin: bool) -> Vec<u8> {
        let mask = [0x12, 0x34, 0x56, 0x78];
        let mut f = vec![if fin { 0x80 } else { 0 } | opcode];
        match payload.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
        }
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        f
    }

    #[test]
    fn server_frame_length_encodings() {
        assert_eq!(encode_frame(OP_TEXT, b"hi"), [0x81, 2, b'h', b'i']);
        let mid = encode_frame(OP_TEXT, &[0u8; 300]);
        assert_eq!(&mid[..4], &[0x81, 126, 1, 44]);
        let big = encode_frame(OP_TEXT, &vec![0u8; 70_000]);
        assert_eq!(&big[..10], &[0x81, 127, 0, 0, 0, 0, 0, 1, 0x11, 0x70]);
    }

    #[test]
    fn decodes_masked_text_and_extended_lengths() {
        let f = read_frame(&mut Cursor::new(masked(
            OP_TEXT,
            b"{\"type\":\"ping\"}",
            true,
        )))
        .unwrap();
        assert_eq!(
            f,
            Frame {
                opcode: OP_TEXT,
                payload: b"{\"type\":\"ping\"}".to_vec()
            }
        );
        let long = vec![b'x'; 300];
        assert_eq!(
            read_frame(&mut Cursor::new(masked(OP_TEXT, &long, true)))
                .unwrap()
                .payload,
            long
        );
    }

    #[test]
    fn refuses_unmasked_fragmented_binary_rsv_and_oversize() {
        assert_eq!(
            read_frame(&mut Cursor::new(vec![0x81, 0x02, b'h', b'i'])),
            Err(WsErr::Protocol(1002))
        );
        assert_eq!(
            read_frame(&mut Cursor::new(masked(OP_TEXT, b"x", false))),
            Err(WsErr::Protocol(1002))
        );
        assert_eq!(
            read_frame(&mut Cursor::new(masked(0x2, b"x", true))),
            Err(WsErr::Protocol(1003))
        );
        let mut rsv = masked(OP_TEXT, b"x", true);
        rsv[0] |= 0x40;
        assert_eq!(
            read_frame(&mut Cursor::new(rsv)),
            Err(WsErr::Protocol(1002))
        );
        let huge = masked(OP_TEXT, &vec![b'x'; MAX_CLIENT_PAYLOAD + 1], true);
        assert_eq!(
            read_frame(&mut Cursor::new(huge)),
            Err(WsErr::Protocol(1009))
        );
        assert_eq!(read_frame(&mut Cursor::new(vec![0x81])), Err(WsErr::Closed));
    }

    #[test]
    fn oversize_control_frames_are_rejected() {
        let mut ping = vec![0x80 | OP_PING, 0x80 | 126, 0, 200, 0, 0, 0, 0];
        ping.extend(vec![0u8; 200]);
        assert_eq!(
            read_frame(&mut Cursor::new(ping)),
            Err(WsErr::Protocol(1002))
        );
    }
}
