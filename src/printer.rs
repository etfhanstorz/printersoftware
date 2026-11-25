//! LAN-mode connection to a Bambu A1 over MQTT (TLS, port 8883).
//!
//! Credentials: user `bblp`, password = the access code shown on the printer.
//! Topics: `device/<serial>/request` (commands) and `device/<serial>/report` (status).
//!
//! The printer uses a self-signed certificate that stricter TLS stacks refuse to
//! parse, so this uses the platform TLS (Schannel on Windows) with certificate
//! checks turned off. Only use this on your own LAN.
//!
//! MQTT 3.1.1 is encoded by hand here; only the few packet types needed are used.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use native_tls::{TlsConnector, TlsStream};

const CONNACK: u8 = 2;
const PUBLISH: u8 = 3;

/// Latest values pulled from the printer's status report.
#[derive(Debug, Default, Clone)]
pub struct Status {
    pub gcode_state: Option<String>,
    pub nozzle_temp: Option<f64>,
    pub bed_temp: Option<f64>,
    /// Fan name and raw speed value as the printer reports it (0-15 scale).
    pub fans: Vec<(String, String)>,
}

pub struct Printer {
    tls: TlsStream<TcpStream>,
    buf: Vec<u8>,
    serial: String,
    seq: u64,
}

/// Listen on the LAN broker for any printer report and return its serial number.
pub fn discover_serial(host: &str, access_code: &str) -> Result<String> {
    let mut p = open(host, access_code)?;
    eprintln!("Logged in to printer. Subscribing to reports...");
    p.send(&subscribe_packet(1, "device/+/report"))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while let Some((kind, body)) = p.next_packet(deadline)? {
        if kind == PUBLISH {
            let (topic, _) = split_publish(&body)?;
            let parts: Vec<&str> = topic.split('/').collect();
            if parts.len() == 3 && parts[0] == "device" && parts[2] == "report" {
                return Ok(parts[1].to_string());
            }
        }
    }
    bail!("no status report received within 20 seconds; make sure the printer is on and the IP is right")
}

impl Printer {
    /// Connect and subscribe to this printer's report topic.
    pub fn connect(host: &str, serial: &str, access_code: &str) -> Result<Self> {
        let mut p = Printer {
            tls: open_stream(host, access_code)?,
            buf: Vec::new(),
            serial: serial.to_string(),
            seq: 1,
        };
        p.send(&subscribe_packet(1, &format!("device/{serial}/report")))?;
        Ok(p)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.tls.write_all(bytes).context("send to printer")?;
        self.tls.flush().context("send to printer")
    }

    fn next_packet(&mut self, deadline: Instant) -> Result<Option<(u8, Vec<u8>)>> {
        next_packet(&mut self.tls, &mut self.buf, deadline)
    }

    fn publish_print(&mut self, command: &str, param: Option<&str>) -> Result<()> {
        self.seq += 1;
        let mut body = serde_json::json!({
            "print": { "sequence_id": self.seq.to_string(), "command": command }
        });
        if let Some(p) = param {
            body["print"]["param"] = serde_json::Value::String(p.to_string());
        }
        let topic = format!("device/{}/request", self.serial);
        self.send(&publish_packet(&topic, body.to_string().as_bytes()))
    }

    /// Send G-code lines one at a time as `gcode_line` commands.
    pub fn send_gcode(&mut self, lines: &[String]) -> Result<()> {
        for line in lines {
            self.publish_print("gcode_line", Some(&format!("{line}\n")))?;
        }
        Ok(())
    }

    /// Wait briefly for the printer's reply to the last command. Returns the
    /// printer's result and reason text, if it answered in time.
    pub fn wait_result(&mut self, timeout: Duration) -> Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        while let Some((kind, body)) = self.next_packet(deadline)? {
            if kind != PUBLISH {
                continue;
            }
            let (_, payload) = split_publish(&body)?;
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else { continue };
            if let Some(print) = v.get("print") {
                if let Some(result) = print.get("result").and_then(|x| x.as_str()) {
                    let reason = print.get("reason").and_then(|x| x.as_str()).unwrap_or("");
                    return Ok(Some(format!("{result} {reason}").trim().to_string()));
                }
            }
        }
        Ok(None)
    }

    /// Emergency stop: halts the current job.
    pub fn stop(&mut self) -> Result<()> {
        self.publish_print("stop", None)
    }

    /// Ask for a full report, then read reports until temperatures arrive or time runs out.
    pub fn read_status(&mut self, timeout: Duration) -> Result<Status> {
        self.seq += 1;
        let body = serde_json::json!({ "pushing": { "sequence_id": self.seq.to_string(), "command": "pushall" } });
        let topic = format!("device/{}/request", self.serial);
        self.send(&publish_packet(&topic, body.to_string().as_bytes()))?;

        let mut status = Status::default();
        let deadline = Instant::now() + timeout;
        while let Some((kind, body)) = self.next_packet(deadline)? {
            if kind != PUBLISH {
                continue;
            }
            let (_, payload) = split_publish(&body)?;
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else { continue };
            if let Some(print) = v.get("print") {
                if let Some(s) = print.get("gcode_state").and_then(|x| x.as_str()) {
                    status.gcode_state = Some(s.to_string());
                }
                if let Some(t) = print.get("nozzle_temper").and_then(|x| x.as_f64()) {
                    status.nozzle_temp = Some(t);
                }
                if let Some(t) = print.get("bed_temper").and_then(|x| x.as_f64()) {
                    status.bed_temp = Some(t);
                }
                for key in ["cooling_fan_speed", "heatbreak_fan_speed", "big_fan1_speed", "big_fan2_speed"] {
                    if let Some(v) = print.get(key) {
                        let text = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                        status.fans.retain(|(k, _)| k != key);
                        status.fans.push((key.to_string(), text));
                    }
                }
            }
            if status.nozzle_temp.is_some() {
                break;
            }
        }
        Ok(status)
    }
}

fn open(host: &str, access_code: &str) -> Result<Printer> {
    Ok(Printer {
        tls: open_stream(host, access_code)?,
        buf: Vec::new(),
        serial: String::new(),
        seq: 0,
    })
}

/// Open TLS to the printer and complete the MQTT CONNECT handshake.
fn open_stream(host: &str, access_code: &str) -> Result<TlsStream<TcpStream>> {
    let addr = format!("{host}:8883")
        .to_socket_addrs()
        .context("bad printer IP address")?
        .next()
        .ok_or_else(|| anyhow!("could not resolve {host}"))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .context("could not reach printer on port 8883 (check IP and that LAN mode is on)")?;

    let connector = TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .context("set up TLS")?;
    let mut tls = connector
        .connect(host, tcp)
        .map_err(|e| anyhow!("TLS handshake with printer failed: {e}"))?;

    tls.get_ref().set_read_timeout(Some(Duration::from_millis(500)))?;
    tls.write_all(&connect_packet("a1-control", "bblp", access_code))?;

    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let Some((kind, body)) = next_packet(&mut tls, &mut buf, deadline)
            .map_err(|e| anyhow!("{e}"))?
        else {
            bail!("printer closed the connection before accepting the login");
        };
        if kind == CONNACK {
            // body = [session_present, return_code]; 0 means accepted.
            if body.get(1) != Some(&0) {
                bail!("printer rejected the login: check the access code (it can change, so re-check on the printer)");
            }
            return Ok(tls);
        }
    }
}

/// Read one complete MQTT packet, waiting until `deadline`.
fn next_packet<S: Read>(stream: &mut S, buf: &mut Vec<u8>, deadline: Instant) -> Result<Option<(u8, Vec<u8>)>> {
    loop {
        if let Some((total, kind)) = frame_len(buf) {
            if buf.len() >= total {
                let header = remaining_len_bytes(buf);
                let body = buf[header..total].to_vec();
                buf.drain(..total);
                return Ok(Some((kind, body)));
            }
        }
        if Instant::now() > deadline {
            return Ok(None);
        }
        let mut tmp = [0u8; 2048];
        match stream.read(&mut tmp) {
            Ok(0) => bail!("printer closed the connection"),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e).context("read from printer"),
        }
    }
}

/// Returns (total packet length, packet type) if the header is fully buffered.
fn frame_len(buf: &[u8]) -> Option<(usize, u8)> {
    let kind = buf.first()? >> 4;
    let mut value = 0usize;
    let mut mult = 1usize;
    for (i, b) in buf.iter().skip(1).enumerate() {
        value += (*b as usize & 0x7f) * mult;
        if b & 0x80 == 0 {
            return Some((1 + i + 1 + value, kind));
        }
        mult *= 128;
        if i >= 3 {
            return None;
        }
    }
    None
}

/// Number of bytes in the remaining-length field of the buffered packet.
fn remaining_len_bytes(buf: &[u8]) -> usize {
    buf.iter().skip(1).take_while(|b| *b & 0x80 != 0).count() + 2
}

/// Split a PUBLISH body into topic and payload (QoS 0 only, which is all we read).
fn split_publish(body: &[u8]) -> Result<(String, &[u8])> {
    if body.len() < 2 {
        bail!("short PUBLISH packet");
    }
    let tlen = u16::from_be_bytes([body[0], body[1]]) as usize;
    let topic_end = 2 + tlen;
    if body.len() < topic_end {
        bail!("bad PUBLISH topic length");
    }
    let topic = String::from_utf8_lossy(&body[2..topic_end]).into_owned();
    Ok((topic, &body[topic_end..]))
}

fn encode_remaining(mut len: usize, out: &mut Vec<u8>) {
    loop {
        let mut b = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            b |= 0x80;
        }
        out.push(b);
        if len == 0 {
            break;
        }
    }
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
}

fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![first];
    encode_remaining(body.len(), &mut v);
    v.extend_from_slice(body);
    v
}

fn connect_packet(client_id: &str, user: &str, pass: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, b"MQTT");
    b.push(4); // protocol level 4 = MQTT 3.1.1
    b.push(0b1100_0010); // username + password + clean session
    b.extend_from_slice(&30u16.to_be_bytes()); // keep-alive seconds
    put_str(&mut b, client_id.as_bytes());
    put_str(&mut b, user.as_bytes());
    put_str(&mut b, pass.as_bytes());
    packet(0x10, &b)
}

fn subscribe_packet(pid: u16, topic: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&pid.to_be_bytes());
    put_str(&mut b, topic.as_bytes());
    b.push(0); // requested QoS 0
    packet(0x82, &b)
}

fn publish_packet(topic: &str, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, topic.as_bytes());
    b.extend_from_slice(payload);
    packet(0x30, &b) // PUBLISH, QoS 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_round_trip() {
        let pkt = publish_packet("device/X/report", b"{\"a\":1}");
        let mut cursor = std::io::Cursor::new(pkt);
        let mut buf = Vec::new();
        let (kind, body) = next_packet(&mut cursor, &mut buf, Instant::now() + Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(kind, PUBLISH);
        let (topic, payload) = split_publish(&body).unwrap();
        assert_eq!(topic, "device/X/report");
        assert_eq!(payload, b"{\"a\":1}");
    }

    #[test]
    fn long_packet_uses_two_length_bytes() {
        let payload = vec![b'x'; 300];
        let pkt = publish_packet("t", &payload);
        assert_eq!(frame_len(&pkt).unwrap().0, pkt.len());
    }
}
