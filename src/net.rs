//! IP addresses as the gateway sees and names them, for IPv4 and IPv6 alike
//! (D-026): listeners bound dual-stack, client addresses in one canonical
//! form, the prefix an abuse limit counts, and hosts written into URLs.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

use url::{Host, Url};

/// Listens on `address`. An unspecified IPv6 address (`[::]:port`) accepts
/// IPv4 clients too, whatever the host's `bindv6only` default; where the
/// host has no IPv6 at all, it falls back to `0.0.0.0:port`, so a dual-stack
/// default still starts on an IPv4-only node.
pub fn bind_listener(address: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    match bind(address) {
        Err(error)
            if address.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) && ipv6_unavailable(&error) =>
        {
            let fallback = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), address.port());
            tracing::warn!(%address, %fallback, %error, "IPv6 is unavailable; listening on IPv4 only");
            bind(fallback)
        }
        result => result,
    }
}

fn bind(address: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    if address.is_ipv6() && address.ip().is_unspecified() {
        socket.set_only_v6(false)?;
    }
    // As std and tokio do: a restarted process can rebind at once.
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    tokio::net::TcpListener::from_std(socket.into())
}

/// A UDP socket for a QUIC listener at `address`, bound as
/// [`bind_listener`] binds TCP: `[::]` dual-stack, falling back to
/// `0.0.0.0` where the host has no IPv6.
pub fn bind_udp(address: SocketAddr) -> io::Result<std::net::UdpSocket> {
    match bind_datagram(address) {
        Err(error)
            if address.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) && ipv6_unavailable(&error) =>
        {
            let fallback = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), address.port());
            tracing::warn!(%address, %fallback, %error, "IPv6 is unavailable; listening on IPv4 only");
            bind_datagram(fallback)
        }
        result => result,
    }
}

fn bind_datagram(address: SocketAddr) -> io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(
        Domain::for_address(address),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    if address.is_ipv6() && address.ip().is_unspecified() {
        socket.set_only_v6(false)?;
    }
    socket.bind(&address.into())?;
    Ok(socket.into())
}

/// The host cannot make IPv6 sockets (IPv6 disabled or absent):
/// `EAFNOSUPPORT` or `EADDRNOTAVAIL`.
pub fn ipv6_unavailable(error: &io::Error) -> bool {
    #[cfg(target_os = "linux")]
    const CODES: [i32; 2] = [97, 99];
    #[cfg(not(target_os = "linux"))]
    const CODES: [i32; 2] = [47, 49];
    error
        .raw_os_error()
        .is_some_and(|code| CODES.contains(&code))
        || error.kind() == io::ErrorKind::AddrNotAvailable
}

/// Whether two listeners would claim the same port: equal addresses, or the
/// same port where either is unspecified (a dual-stack `[::]` covers
/// `0.0.0.0` too).
pub fn listeners_overlap(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

/// One address in one form: an IPv4 client of a dual-stack listener
/// (`::ffff:192.0.2.7`) is `192.0.2.7`.
pub fn canonical(address: IpAddr) -> IpAddr {
    address.to_canonical()
}

/// What an abuse limit counts for `address`: an IPv4 address itself, and an
/// IPv6 address's /64, which one host or subscriber is routinely given whole,
/// so that walking its addresses does not reset the limit.
pub fn abuse_key(address: IpAddr) -> IpAddr {
    match canonical(address) {
        IpAddr::V4(address) => IpAddr::V4(address),
        IpAddr::V6(address) => {
            let bits = u128::from(address) & !((1u128 << 64) - 1);
            IpAddr::V6(Ipv6Addr::from(bits))
        }
    }
}

/// One `X-Forwarded-For` entry as proxies write it: `192.0.2.7`,
/// `192.0.2.7:4711`, `2001:db8::7`, `[2001:db8::7]` or `[2001:db8::7]:4711`,
/// optionally quoted, an IPv6 zone (`fe80::1%eth0`) dropped. Canonical.
pub fn parse_forwarded_entry(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim().trim_matches('"');
    let address = if let Some(rest) = entry.strip_prefix('[') {
        let (inside, after) = rest.split_once(']')?;
        if !(after.is_empty() || after.strip_prefix(':').is_some_and(is_port)) {
            return None;
        }
        inside
            .parse::<Ipv6Addr>()
            .ok()
            .map(IpAddr::V6)
            .or_else(|| {
                strip_zone(inside).and_then(|value| value.parse::<Ipv6Addr>().ok().map(IpAddr::V6))
            })?
    } else if let Ok(address) = entry.parse::<IpAddr>() {
        address
    } else if let Some(address) = strip_zone(entry).and_then(|value| value.parse::<Ipv6Addr>().ok())
    {
        IpAddr::V6(address)
    } else {
        // IPv4 with a port: exactly one colon.
        let (host, port) = entry.split_once(':')?;
        if !is_port(port) {
            return None;
        }
        IpAddr::V4(host.parse::<Ipv4Addr>().ok()?)
    };
    Some(canonical(address))
}

fn strip_zone(value: &str) -> Option<&str> {
    value.split_once('%').map(|(address, _)| address)
}

fn is_port(value: &str) -> bool {
    !value.is_empty() && value.len() <= 5 && value.parse::<u16>().is_ok()
}

/// Whether `url` names this machine: `localhost` (or a subdomain of it), or
/// a loopback address of either family (`127.0.0.0/8`, `::1`, and IPv4
/// loopback mapped into IPv6).
pub fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.');
            domain.eq_ignore_ascii_case("localhost")
                || domain.to_ascii_lowercase().ends_with(".localhost")
        }
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => canonical(IpAddr::V6(address)).is_loopback(),
        None => false,
    }
}

/// `host` as written in a URL's authority: an IPv6 literal in brackets
/// (`fd20:ce::254` → `[fd20:ce::254]`), anything else as given.
pub fn url_host(host: &str) -> String {
    match host.parse::<Ipv6Addr>() {
        Ok(_) => format!("[{host}]"),
        Err(_) => host.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn forwarded_entries_parse_in_every_form_proxies_write() {
        for (entry, expected) in [
            ("192.0.2.7", "192.0.2.7"),
            (" 192.0.2.7:4711 ", "192.0.2.7"),
            ("2001:db8::7", "2001:db8::7"),
            ("[2001:db8::7]", "2001:db8::7"),
            ("[2001:db8::7]:4711", "2001:db8::7"),
            ("\"[2001:db8::7]:4711\"", "2001:db8::7"),
            ("fe80::1%eth0", "fe80::1"),
            ("[fe80::1%25eth0]:80", "fe80::1"),
            ("::ffff:192.0.2.7", "192.0.2.7"),
            ("[::ffff:192.0.2.7]:80", "192.0.2.7"),
        ] {
            assert_eq!(parse_forwarded_entry(entry), Some(ip(expected)), "{entry}");
        }
        for entry in [
            "",
            "unknown",
            "_hidden",
            "192.0.2.7:",
            "192.0.2.7:99999",
            "[2001:db8::7",
            "[2001:db8::7]x",
            "2001:db8::7:4711:1:2:3:4",
            "192.0.2.7:80:80",
        ] {
            assert_eq!(parse_forwarded_entry(entry), None, "{entry}");
        }
    }

    #[test]
    fn addresses_are_canonical_and_ipv6_counts_by_its_64() {
        assert_eq!(canonical(ip("::ffff:10.1.2.3")), ip("10.1.2.3"));
        assert_eq!(canonical(ip("2001:db8::1")), ip("2001:db8::1"));
        assert_eq!(abuse_key(ip("192.0.2.7")), ip("192.0.2.7"));
        assert_eq!(abuse_key(ip("::ffff:192.0.2.7")), ip("192.0.2.7"));
        assert_eq!(
            abuse_key(ip("2001:db8:1:2:aaaa:bbbb:cccc:dddd")),
            ip("2001:db8:1:2::")
        );
        assert_eq!(
            abuse_key(ip("2001:db8:1:2::1")),
            abuse_key(ip("2001:db8:1:2::ffff"))
        );
        assert_ne!(
            abuse_key(ip("2001:db8:1:2::1")),
            abuse_key(ip("2001:db8:1:3::1"))
        );
    }

    #[test]
    fn loopback_urls_of_both_families_are_recognized() {
        for url in [
            "http://localhost:8080/",
            "http://LOCALHOST/",
            "http://api.localhost/",
            "http://127.0.0.1/",
            "http://127.0.0.53:9/",
            "http://[::1]:8000/v1",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(is_loopback_url(&Url::parse(url).unwrap()), "{url}");
        }
        for url in [
            "http://example.com/",
            "http://localhost.example.com/",
            "http://10.0.0.1/",
            "http://[2001:db8::1]/",
            "http://[::]/",
        ] {
            assert!(!is_loopback_url(&Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn ipv6_hosts_are_bracketed_in_urls() {
        assert_eq!(url_host("fd20:ce::254"), "[fd20:ce::254]");
        assert_eq!(url_host("[fd20:ce::254]"), "[fd20:ce::254]");
        assert_eq!(url_host("169.254.169.254"), "169.254.169.254");
        assert_eq!(
            url_host("metadata.google.internal"),
            "metadata.google.internal"
        );
        assert!(Url::parse(&format!("http://{}/", url_host("fd20:ce::254"))).is_ok());
    }

    #[test]
    fn listeners_on_one_port_overlap_when_either_is_unspecified() {
        let at = |value: &str| value.parse::<SocketAddr>().unwrap();
        assert!(listeners_overlap(at("[::]:3000"), at("0.0.0.0:3000")));
        assert!(listeners_overlap(at("127.0.0.1:3000"), at("[::]:3000")));
        assert!(listeners_overlap(at("[::1]:3000"), at("[::1]:3000")));
        assert!(!listeners_overlap(at("[::]:3000"), at("[::]:9464")));
        assert!(!listeners_overlap(at("127.0.0.1:3000"), at("[::1]:3000")));
    }

    #[tokio::test]
    async fn a_dual_stack_listener_accepts_both_families() {
        let listener = match bind_listener("[::]:0".parse().unwrap()) {
            Ok(listener) => listener,
            Err(error) => panic!("bind: {error}"),
        };
        let port = listener.local_addr().unwrap().port();
        if listener.local_addr().unwrap().is_ipv4() {
            eprintln!("no IPv6 on this host; fell back to IPv4");
        } else if tokio::net::TcpStream::connect(("::1", port)).await.is_err() {
            eprintln!("no IPv6 loopback on this host; skipping the IPv6 connect");
        } else {
            let (_, peer) = listener.accept().await.unwrap();
            assert!(peer.ip().is_loopback());
        }
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let (_, peer) = listener.accept().await.unwrap();
        assert_eq!(canonical(peer.ip()), ip("127.0.0.1"));
    }

    #[test]
    fn a_dual_stack_udp_socket_receives_both_families() {
        let socket = bind_udp("[::]:0".parse().unwrap()).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let mut buffer = [0u8; 8];
        let send = |from: &str, to: SocketAddr| {
            std::net::UdpSocket::bind(from)
                .and_then(|sender| sender.send_to(b"ping", to))
                .is_ok()
        };
        if socket.local_addr().unwrap().is_ipv4() {
            eprintln!("no IPv6 on this host; fell back to IPv4");
        } else if send("[::1]:0", SocketAddr::new(ip("::1"), port)) {
            let (_, peer) = socket.recv_from(&mut buffer).unwrap();
            assert!(peer.ip().is_loopback());
        } else {
            eprintln!("no IPv6 loopback on this host; skipping the IPv6 datagram");
        }
        assert!(send("127.0.0.1:0", SocketAddr::new(ip("127.0.0.1"), port)));
        let (_, peer) = socket.recv_from(&mut buffer).unwrap();
        assert_eq!(canonical(peer.ip()), ip("127.0.0.1"));
    }
}
