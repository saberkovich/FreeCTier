//! Wire protocol between the FreeC Tier UI process (Steam engine, packet
//! routing) and the elevated Wintun service (adapter ownership). The protocol
//! is transport-agnostic: the caller owns the named pipe and exchanges frames
//! through [`read_frame`]/[`write_frame`].
//!
//! Frame layout: `[u32 payload length LE][payload]`, where payload is
//! `[u8 kind][body]`. The service trusts its single connected client (pipe
//! DACL restricts who that can be) and performs no traffic validation —
//! anti-spoof checks need the Steam sender identity and stay in the UI.

use std::io;
use std::net::Ipv4Addr;
use uuid::Uuid;

/// Protocol version; a mismatch disconnects the client.
pub const PROTOCOL: u16 = 1;
/// Named pipe both sides agree on.
pub const PIPE_PATH: &str = "\\\\.\\pipe\\FreeCTierService";
/// Packet payload is bounded by MTU (1280); the cap covers any frame the
/// service accepts, including the largest adapter error text.
pub const MAX_FRAME: usize = 64 * 1024;

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// UI process → service. Opens/closes adapters and feeds them outbound IPv4
/// packets that the UI already validated with `packet.outgoing`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    Hello,
    OpenAdapter { network: Uuid, local_ip: Ipv4Addr },
    CloseAdapter { network: Uuid },
    Packet { network: Uuid, packet: Vec<u8> },
}

/// Service → UI process. Delivers inbound IPv4 packets and acknowledges
/// adapter lifecycle changes; `AdapterError` also covers service-wide
/// failures (`network: None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceMessage {
    Welcome,
    AdapterOpened {
        network: Uuid,
    },
    AdapterError {
        network: Option<Uuid>,
        message: String,
    },
    Packet {
        network: Uuid,
        packet: Vec<u8>,
    },
}

fn put_uuid(out: &mut Vec<u8>, network: Uuid) {
    out.extend_from_slice(network.as_bytes());
}

fn put_string(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn get_uuid(src: &[u8], at: usize) -> io::Result<Uuid> {
    let bytes: [u8; 16] = src
        .get(at..at + 16)
        .ok_or_else(|| invalid("truncated uuid"))?
        .try_into()
        .expect("16-byte slice");
    Ok(Uuid::from_bytes(bytes))
}

fn get_string(src: &[u8], at: usize) -> io::Result<(String, usize)> {
    let len = u16::from_le_bytes(
        src.get(at..at + 2)
            .ok_or_else(|| invalid("truncated string length"))?
            .try_into()
            .expect("2-byte slice"),
    ) as usize;
    let end = at + 2 + len;
    let bytes = src
        .get(at + 2..end)
        .ok_or_else(|| invalid("truncated string"))?;
    Ok((String::from_utf8_lossy(bytes).into_owned(), end))
}

fn get_packet(src: &[u8], at: usize) -> io::Result<Vec<u8>> {
    src.get(at..)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| invalid("truncated packet"))
}

fn encode_client(message: &ClientMessage) -> io::Result<(u8, Vec<u8>)> {
    let mut body = Vec::new();
    let kind = match message {
        ClientMessage::Hello => {
            body.extend_from_slice(&PROTOCOL.to_le_bytes());
            1
        }
        ClientMessage::OpenAdapter { network, local_ip } => {
            put_uuid(&mut body, *network);
            body.extend_from_slice(&local_ip.octets());
            2
        }
        ClientMessage::CloseAdapter { network } => {
            put_uuid(&mut body, *network);
            3
        }
        ClientMessage::Packet { network, packet } => {
            put_uuid(&mut body, *network);
            body.extend_from_slice(packet);
            4
        }
    };
    Ok((kind, body))
}

fn encode_service(message: &ServiceMessage) -> io::Result<(u8, Vec<u8>)> {
    let mut body = Vec::new();
    let kind = match message {
        ServiceMessage::Welcome => {
            body.extend_from_slice(&PROTOCOL.to_le_bytes());
            1
        }
        ServiceMessage::AdapterOpened { network } => {
            put_uuid(&mut body, *network);
            2
        }
        ServiceMessage::AdapterError { network, message } => {
            body.push(match network {
                Some(_) => 1,
                None => 0,
            });
            if let Some(network) = network {
                put_uuid(&mut body, *network);
            }
            put_string(&mut body, message);
            3
        }
        ServiceMessage::Packet { network, packet } => {
            put_uuid(&mut body, *network);
            body.extend_from_slice(packet);
            4
        }
    };
    Ok((kind, body))
}

/// Frame body ([kind][payload]) without the length prefix; [`write_frame`]
/// adds it on the wire.
pub fn encode_frame(message: &ClientMessage) -> io::Result<Vec<u8>> {
    let (kind, body) = encode_client(message)?;
    if body.len() + 1 > MAX_FRAME {
        return Err(invalid("frame exceeds size cap"));
    }
    let mut frame = Vec::with_capacity(body.len() + 1);
    frame.push(kind);
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn decode_client(frame: &[u8]) -> io::Result<ClientMessage> {
    let (kind, body) = frame.split_first().ok_or_else(|| invalid("empty frame"))?;
    Ok(match *kind {
        1 => {
            let protocol = u16::from_le_bytes(
                body.first_chunk::<2>()
                    .ok_or_else(|| invalid("truncated hello"))?
                    .to_owned(),
            );
            if protocol != PROTOCOL {
                return Err(invalid("protocol mismatch"));
            }
            ClientMessage::Hello
        }
        2 => ClientMessage::OpenAdapter {
            network: get_uuid(body, 0)?,
            local_ip: Ipv4Addr::new(
                *body.get(16).ok_or_else(|| invalid("truncated local ip"))?,
                *body.get(17).ok_or_else(|| invalid("truncated local ip"))?,
                *body.get(18).ok_or_else(|| invalid("truncated local ip"))?,
                *body.get(19).ok_or_else(|| invalid("truncated local ip"))?,
            ),
        },
        3 => ClientMessage::CloseAdapter {
            network: get_uuid(body, 0)?,
        },
        4 => ClientMessage::Packet {
            network: get_uuid(body, 0)?,
            packet: get_packet(body, 16)?,
        },
        _ => return Err(invalid("unknown client frame kind")),
    })
}

fn decode_service(frame: &[u8]) -> io::Result<ServiceMessage> {
    let (kind, body) = frame.split_first().ok_or_else(|| invalid("empty frame"))?;
    Ok(match *kind {
        1 => {
            let protocol = u16::from_le_bytes(
                body.first_chunk::<2>()
                    .ok_or_else(|| invalid("truncated welcome"))?
                    .to_owned(),
            );
            if protocol != PROTOCOL {
                return Err(invalid("protocol mismatch"));
            }
            ServiceMessage::Welcome
        }
        2 => ServiceMessage::AdapterOpened {
            network: get_uuid(body, 0)?,
        },
        3 => {
            let has_network = *body.first().ok_or_else(|| invalid("truncated error"))? == 1;
            let (network, at) = if has_network {
                (Some(get_uuid(body, 1)?), 17)
            } else {
                (None, 1)
            };
            let (message, _) = get_string(body, at)?;
            ServiceMessage::AdapterError { network, message }
        }
        4 => ServiceMessage::Packet {
            network: get_uuid(body, 0)?,
            packet: get_packet(body, 16)?,
        },
        _ => return Err(invalid("unknown service frame kind")),
    })
}

/// Encode a service-side message into a frame body ([kind][payload]).
pub fn encode_service_frame(message: &ServiceMessage) -> io::Result<Vec<u8>> {
    let (kind, body) = encode_service(message)?;
    if body.len() + 1 > MAX_FRAME {
        return Err(invalid("frame exceeds size cap"));
    }
    let mut frame = Vec::with_capacity(body.len() + 1);
    frame.push(kind);
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Decode one UI-side frame (kind + body, without the length prefix).
pub fn decode_client_frame(frame: &[u8]) -> io::Result<ClientMessage> {
    decode_client(frame)
}

/// Decode one service-side frame (kind + body, without the length prefix).
pub fn decode_service_frame(frame: &[u8]) -> io::Result<ServiceMessage> {
    decode_service(frame)
}

/// Blocking framed write; the caller treats a short write as a broken pipe.
pub fn write_frame<W: io::Write>(stream: &mut W, frame: &[u8]) -> io::Result<()> {
    stream.write_all(&(frame.len() as u32).to_le_bytes())?;
    stream.write_all(frame)
}

/// Blocking framed read; returns the frame without the length prefix.
pub fn read_frame<R: io::Read>(stream: &mut R) -> io::Result<Vec<u8>> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(invalid("frame size out of range"));
    }
    let mut frame = vec![0u8; length];
    stream.read_exact(&mut frame)?;
    Ok(frame)
}

#[cfg(test)]
mod codec {
    use super::*;

    const NETWORK: Uuid = Uuid::from_bytes([7; 16]);
    const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(10, 77, 3, 42);

    fn roundtrip_client(message: ClientMessage) {
        let frame = encode_frame(&message).unwrap();
        assert_eq!(decode_client_frame(&frame).unwrap(), message);
    }

    fn roundtrip_service(message: ServiceMessage) {
        let frame = encode_service_frame(&message).unwrap();
        assert_eq!(decode_service_frame(&frame).unwrap(), message);
    }

    #[test]
    fn client_messages_roundtrip() {
        roundtrip_client(ClientMessage::Hello);
        roundtrip_client(ClientMessage::OpenAdapter {
            network: NETWORK,
            local_ip: LOCAL_IP,
        });
        roundtrip_client(ClientMessage::CloseAdapter { network: NETWORK });
        roundtrip_client(ClientMessage::Packet {
            network: NETWORK,
            packet: vec![69, 0, 0, 20],
        });
    }

    #[test]
    fn service_messages_roundtrip() {
        roundtrip_service(ServiceMessage::Welcome);
        roundtrip_service(ServiceMessage::AdapterOpened { network: NETWORK });
        roundtrip_service(ServiceMessage::AdapterError {
            network: Some(NETWORK),
            message: "адаптер не открылся".into(),
        });
        roundtrip_service(ServiceMessage::AdapterError {
            network: None,
            message: String::new(),
        });
        roundtrip_service(ServiceMessage::Packet {
            network: NETWORK,
            packet: vec![69, 0, 0, 20],
        });
    }

    #[test]
    fn frames_stream_through_read_write() {
        let message = ClientMessage::Packet {
            network: NETWORK,
            packet: vec![1; 1280],
        };
        let frame = encode_frame(&message).unwrap();
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &frame).unwrap();
        // read_exact inside read_frame tolerates partial pipe reads, so a
        // single slice reader is sufficient to exercise the framing.
        let decoded = read_frame(&mut buffer.as_slice()).unwrap();
        assert_eq!(decode_client_frame(&decoded).unwrap(), message);
    }

    #[test]
    fn malformed_frames_are_rejected() {
        use std::io::Read as _;
        assert!(decode_client_frame(&[]).is_err());
        assert!(decode_client_frame(&[9]).is_err());
        assert!(decode_client_frame(&[9, 1, 0]).is_err()); // unknown kind
                                                           // Wrong protocol version is rejected on the handshake frame.
        assert!(decode_client_frame(&[1, 0xFF, 0xFF]).is_err());
        // Oversized frames never encode.
        assert!(encode_frame(&ClientMessage::Packet {
            network: NETWORK,
            packet: vec![0; MAX_FRAME],
        })
        .is_err());
        // Length prefix beyond the cap is rejected before reading the body.
        let mut reader = [0x01, 0x00, 0x01, 0x00].chain(io::repeat(0).take(0x1000));
        let error = read_frame(&mut reader).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let mut empty = [0u8; 4].chain(io::repeat(0).take(16));
        assert_eq!(
            read_frame(&mut empty).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
