use std::{
    io::Cursor,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    ops::ControlFlow,
    sync::Arc,
};

use aes_siv::{aead::KeyInit, Aes128SivAead, Key};

use ntp_proto::{KeyExchange, KeyExchangeError, NtpPacket, NtsRecord, PollInterval};
use ntp_udp::UdpSocket;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;

fn key_exchange_client() -> Result<tokio_rustls::TlsConnector, rustls::Error> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().expect("could not load platform certs") {
        roots.add(&rustls::Certificate(cert.0)).unwrap();
    }

    let mut config = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols.push(b"ntske/1".to_vec());

    let rc_config = Arc::new(config);

    Ok(tokio_rustls::TlsConnector::from(rc_config))
}

// unstable in std; check on https://github.com/rust-lang/rust/issues/88581 some time in the future
pub const fn next_multiple_of(lhs: usize, rhs: usize) -> usize {
    match lhs % rhs {
        0 => lhs,
        r => lhs + (rhs - r),
    }
}

// unstable in std; check on https://github.com/rust-lang/rust/issues/88581 some time in the future
pub const fn div_ceil(lhs: usize, rhs: usize) -> usize {
    let d = lhs / rhs;
    let r = lhs % rhs;
    if r > 0 && rhs > 0 {
        d + 1
    } else {
        d
    }
}

async fn key_exchange(
    domain: &str,
    stream: &mut tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
) -> std::io::Result<Result<KeyExchange, KeyExchangeError>> {
    let mut state = KeyExchange {
        // use the domain of the KE server, unless the KE server gives a different remote
        remote: domain.to_string(),
        // use port 123, the default port for NTS, unless the KE server gives a different port.
        port: 123,
        // servers SHOULD send 8 cookies, but MUST send at least one
        cookies: Vec::with_capacity(8),
    };

    let mut buffer = [0; 1024];
    let mut decoder = ntp_proto::NtsRecord::decoder();

    'outer: loop {
        let n = stream.read(&mut buffer).await?;
        decoder.extend(buffer[..n].iter().copied());

        while let Some(record) = decoder.next()? {
            match state.step_with_record(record) {
                ControlFlow::Continue(new_state) => {
                    state = new_state;
                    continue;
                }
                ControlFlow::Break(Ok(new_state)) => {
                    state = new_state;
                    break 'outer;
                }
                ControlFlow::Break(Err(e)) => return Ok(Err(e)),
            }
        }
    }

    if state.cookies.is_empty() {
        Ok(Err(KeyExchangeError::NoCookies))
    } else {
        Ok(Ok(state))
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // let domain = "time.cloudflare.com";
    let domain = "nts.time.nl";
    let config = key_exchange_client().unwrap();

    let addr = (domain, 4460)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut stream = config
        .connect(domain.try_into().unwrap(), stream)
        .await
        .unwrap();

    let mut buffer = Vec::with_capacity(1024);
    for record in NtsRecord::client_key_exchange_records() {
        record.write(&mut buffer)?;
    }

    // it is important for `nts.time.nl` that we only make one write to the rustls stream
    stream.write_all(&buffer).await?;

    let ke = match key_exchange(domain, &mut stream).await? {
        Ok(state) => state,
        Err(e) => panic!("key exchange failed: {:?}", e),
    };

    println!("cookie: {:?}", &ke.cookies[0]);

    let mut c2s = [0; 32];
    let mut s2c = [0; 32];
    let label = b"EXPORTER-network-time-security";

    stream
        .get_ref()
        .1
        .export_keying_material(&mut c2s, label, Some(&[0, 0, 0, 15, 0]))
        .unwrap();
    stream
        .get_ref()
        .1
        .export_keying_material(&mut s2c, label, Some(&[0, 0, 0, 15, 1]))
        .unwrap();

    let addr = (ke.remote, ke.port)
        .to_socket_addrs()
        .unwrap()
        .next()
        .unwrap();

    let mut socket = match addr {
        SocketAddr::V4(_) => UdpSocket::client((Ipv4Addr::UNSPECIFIED, 0).into(), addr).await?,
        SocketAddr::V6(_) => UdpSocket::client((Ipv6Addr::UNSPECIFIED, 0).into(), addr).await?,
    };

    let identifier: Vec<u8> = (0..).take(32).collect();
    let cipher = Aes128SivAead::new(Key::<Aes128SivAead>::from_slice(c2s.as_slice()));

    let (packet, _) = NtpPacket::nts_poll_message_request_extra_cookies(
        &identifier,
        &ke.cookies[0],
        1,
        PollInterval::default(),
    );

    let mut raw = [0u8; 1024];
    let mut w = Cursor::new(raw.as_mut_slice());
    packet.serialize(&mut w, Some(&cipher))?;
    socket.send(&w.get_ref()[..w.position() as usize]).await?;

    let mut buf = [0; 1024];
    let (n, _remote, _timestamp) = socket.recv(&mut buf).await?;
    println!("response ({n} bytes): {:?}", &buf[0..n]);

    let cipher = Aes128SivAead::new(Key::<Aes128SivAead>::from_slice(s2c.as_slice()));
    let _ = dbg!(NtpPacket::deserialize(&buf[..n], Some(&cipher)).unwrap());

    Ok(())
}
