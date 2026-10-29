//! Times a 16-byte request and its echo, one every 250 to 400 ms, over TCP,
//! QUIC (quinn), or the Atur Terminal UDP stream. It is what a keystroke and
//! its echo ask of a transport, with nothing else in the way.
//!
//! ```text
//! pingpong tcp|quic|udp PORT COUNT       run inside the lab's network namespace
//! ```
//!
//! It starts the server and the client in one process, talking over loopback,
//! so the shaping that netem applies to the loopback interface is the whole
//! network. It prints one JSON object with the round trip percentiles.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use muxy_protocol::transport::ByteStream;
use muxy_protocol::transport::udp::{Options, Secret, UdpServer};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn pause(index: usize) -> Duration {
    Duration::from_millis(250 + ((index * 7919) % 150) as u64)
}

fn main() -> Result {
    let mut arguments = std::env::args().skip(1);
    let kind = arguments.next().ok_or("tcp|quic|udp")?;
    let port: u16 = arguments.next().ok_or("PORT")?.parse()?;
    let count: usize = arguments.next().ok_or("COUNT")?.parse()?;
    let mut rtts = match kind.as_str() {
        "tcp" => tcp(port, count)?,
        "udp" => udp(port, count)?,
        "quic" => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(quic(port, count))?,
        other => return Err(format!("unknown transport {other}").into()),
    };
    rtts.sort_by(f64::total_cmp);
    let at = |fraction: f64| rtts[((rtts.len() as f64 * fraction) as usize).min(rtts.len() - 1)];
    writeln!(
        std::io::stdout(),
        "{{\"transport\":\"{kind}\",\"n\":{},\"p50\":{:.0},\"p90\":{:.0},\"p99\":{:.0},\"max\":{:.0}}}",
        rtts.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        rtts.last().copied().unwrap_or(f64::NAN)
    )?;
    Ok(())
}

fn tcp(port: u16, count: usize) -> Result<Vec<f64>> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_nodelay(true);
            let mut buffer = [0u8; 16];
            while stream.read_exact(&mut buffer).is_ok() {
                if stream.write_all(&buffer).is_err() {
                    break;
                }
            }
        }
    });
    let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
    stream.set_nodelay(true)?;
    exchange(&mut stream, count)
}

fn exchange(stream: &mut (impl Read + Write), count: usize) -> Result<Vec<f64>> {
    let mut rtts = Vec::with_capacity(count);
    let mut buffer = [0u8; 16];
    for index in 0..count {
        let sent = Instant::now();
        stream.write_all(&[index as u8; 16])?;
        stream.read_exact(&mut buffer)?;
        rtts.push(sent.elapsed().as_secs_f64() * 1000.0);
        std::thread::sleep(pause(index));
    }
    Ok(rtts)
}

fn udp(port: u16, count: usize) -> Result<Vec<f64>> {
    let secret = Secret::from_bytes([42; 32]);
    let server = UdpServer::bind(port..=port, &secret, Options::SERVER)?;
    let (mut reader, mut writer) = Box::new(server.into_stream()).split()?;
    std::thread::spawn(move || {
        let mut buffer = [0u8; 16];
        while reader.read_exact(&mut buffer).is_ok() {
            if writer.write_all(&buffer).is_err() {
                break;
            }
        }
    });
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let stream = muxy_protocol::transport::udp::connect(
        &[address],
        &secret,
        Duration::from_secs(30),
        Options::CLIENT,
    )?;
    let (mut reader, mut writer) = Box::new(stream).split()?;
    let mut rtts = Vec::with_capacity(count);
    let mut buffer = [0u8; 16];
    for index in 0..count {
        let sent = Instant::now();
        writer.write_all(&[index as u8; 16])?;
        reader.read_exact(&mut buffer)?;
        rtts.push(sent.elapsed().as_secs_f64() * 1000.0);
        std::thread::sleep(pause(index));
    }
    Ok(rtts)
}

async fn quic(port: u16, count: usize) -> Result<Vec<f64>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["lab".to_owned()])?;
    let certificate = certified.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let mut server_config = quinn::ServerConfig::with_single_cert(vec![certificate.clone()], key.into())?;
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(120).try_into()?));
    server_config.transport_config(Arc::new(transport));
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let endpoint = quinn::Endpoint::server(server_config, address)?;
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            tokio::spawn(async move {
                let Ok(connection) = incoming.await else { return };
                let Ok((mut send, mut receive)) = connection.accept_bi().await else {
                    return;
                };
                let mut buffer = [0u8; 16];
                while receive.read_exact(&mut buffer).await.is_ok() {
                    if send.write_all(&buffer).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate)?;
    let mut client = quinn::Endpoint::client((Ipv4Addr::LOCALHOST, 0).into())?;
    let mut config = quinn::ClientConfig::with_root_certificates(Arc::new(roots))?;
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(120).try_into()?));
    config.transport_config(Arc::new(transport));
    client.set_default_client_config(config);
    let connection = client.connect(address, "lab")?.await?;
    let (mut send, mut receive) = connection.open_bi().await?;
    let mut rtts = Vec::with_capacity(count);
    let mut buffer = [0u8; 16];
    for index in 0..count {
        let sent = Instant::now();
        send.write_all(&[index as u8; 16]).await?;
        receive.read_exact(&mut buffer).await?;
        rtts.push(sent.elapsed().as_secs_f64() * 1000.0);
        tokio::time::sleep(pause(index)).await;
    }
    Ok(rtts)
}
