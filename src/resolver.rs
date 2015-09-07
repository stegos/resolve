//! High-level resolver operations

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;
use std::vec::IntoIter;

use address::address_name;
use config::DnsConfig;
use message::{Message, Qr, Question};
use record::{A, AAAA, Class, Ptr, RecordType};
use socket::{DnsSocket, Error};

/// Performs resolution operations
pub struct DnsResolver {
    sock: DnsSocket,
    config: DnsConfig,
    /// Index of `config.name_servers` to use in next DNS request;
    /// ignored if `config.rotate` is `false`.
    next_ns: usize,
}

impl DnsResolver {
    /// Constructs a `DnsResolver` using the given configuration.
    pub fn new(config: DnsConfig) -> io::Result<DnsResolver> {
        let sock = try!(DnsSocket::new());
        DnsResolver::new_with_sock(sock, config)
    }

    /// Constructs a `DnsResolver` using the given configuration and bound
    /// to the given address.
    pub fn bind<A: ToSocketAddrs>(addr: A, config: DnsConfig) -> io::Result<DnsResolver> {
        let sock = try!(DnsSocket::bind(addr));
        DnsResolver::new_with_sock(sock, config)
    }

    fn new_with_sock(sock: DnsSocket, config: DnsConfig) -> io::Result<DnsResolver> {
        Ok(DnsResolver{
            sock: sock,
            config: config,
            next_ns: 0,
        })
    }

    /// Resolves an IPv4 or IPv6 address to a hostname.
    pub fn resolve_addr(&mut self, addr: &IpAddr) -> io::Result<String> {
        convert_error("failed to resolve address", || {
            let mut out_msg = self.basic_message();

            out_msg.question.push(Question::new(
                address_name(addr), RecordType::Ptr, Class::Internet));

            let msg = try!(self.get_response(&out_msg));

            for rr in msg.into_records() {
                if rr.r_type == RecordType::Ptr {
                    let ptr = try!(rr.read_rdata::<Ptr>());
                    let mut name = ptr.name;
                    if name.ends_with('.') {
                        name.pop();
                    }
                    return Ok(name);
                }
            }

            Err(Error::IoError(io::Error::new(io::ErrorKind::Other,
                "failed to resolve address: name not found")))
        })
    }

    /// Resolves a hostname to a series of IPv4 or IPv6 addresses.
    pub fn resolve_host(&mut self, host: &str) -> io::Result<ResolveHost> {
        convert_error("failed to resolve host", || {
            let mut err = None;
            let mut res = Vec::new();

            let use_search = !host.ends_with('.') &&
                host.chars().filter(|&c| c == '.')
                    .count() as u32 >= self.config.n_dots;

            let names = if use_search {
                with_suffixes(host, &self.config.search)
            } else {
                vec![host.to_owned()]
            };

            for name in names {
                info!("attempting lookup of name \"{}\"", name);

                if self.config.use_inet6 {
                    err = self.resolve_host_v6(&name,
                        |ip| res.push(IpAddr::V6(ip))).err();

                    if res.is_empty() {
                        err = err.or(self.resolve_host_v4(&name,
                            |ip| res.push(IpAddr::V6(ip.to_ipv6_mapped()))).err());
                    }
                } else {
                    err = self.resolve_host_v4(&name, |ip| res.push(IpAddr::V4(ip))).err();
                    err = err.or(self.resolve_host_v6(&name,
                        |ip| res.push(IpAddr::V6(ip))).err());
                }

                if !res.is_empty() {
                    return Ok(ResolveHost(res.into_iter()));
                }
            }

            if let Some(e) = err {
                Err(e)
            } else {
                Err(Error::IoError(io::Error::new(io::ErrorKind::Other,
                    "failed to resolve host: name not found")))
            }
        })
    }

    fn resolve_host_v4<F>(&mut self, host: &str, mut f: F) -> Result<(), Error>
            where F: FnMut(Ipv4Addr) {
        let mut out_msg = self.basic_message();

        out_msg.question.push(Question::new(
            host.to_owned(), RecordType::A, Class::Internet));

        let msg = try!(self.get_response(&out_msg));

        for rr in msg.into_records() {
            if rr.r_type == RecordType::A {
                let a = try!(rr.read_rdata::<A>());
                f(a.address);
            }
        }

        Ok(())
    }

    fn resolve_host_v6<F>(&mut self, host: &str, mut f: F) -> Result<(), Error>
            where F: FnMut(Ipv6Addr) {
        let mut out_msg = self.basic_message();

        out_msg.question.push(Question::new(
            host.to_owned(), RecordType::AAAA, Class::Internet));

        let msg = try!(self.get_response(&out_msg));

        for rr in msg.into_records() {
            if rr.r_type == RecordType::AAAA {
                let aaaa = try!(rr.read_rdata::<AAAA>());
                f(aaaa.address);
            }
        }

        Ok(())
    }

    fn basic_message(&self) -> Message {
        let mut msg = Message::new();

        msg.header.recursion_desired = true;
        msg
    }

    fn get_response(&mut self, out_msg: &Message) -> Result<Message, Error> {
        let mut last_err = None;

        'retry: for retries in 0..self.config.attempts {
            let ns_addr = if self.config.rotate {
                self.next_nameserver()
            } else {
                let n = self.config.name_servers.len();
                self.config.name_servers[retries as usize % n]
            };

            let mut timeout = self.config.timeout;

            info!("resolver sending message to {}", ns_addr);

            try!(self.sock.send_message(out_msg, &ns_addr));

            loop {
                try!(self.sock.get().set_read_timeout(Some(timeout)));

                let (passed, r) = span(|| self.sock.recv_message(&ns_addr));

                match r {
                    Ok(None) => (),
                    Ok(Some(msg)) => {
                        // Ignore irrelevant messages
                        if msg.header.id == out_msg.header.id &&
                                msg.header.qr == Qr::Response {
                            try!(msg.get_error());
                            return Ok(msg);
                        }
                    }
                    Err(e) => {
                        // Retry on timeout
                        if e.is_timeout() {
                            last_err = Some(e);
                            continue 'retry;
                        }
                        // Immediately bail for other errors
                        return Err(e);
                    }
                }

                // Maintain the right total timeout if we're interrupted by
                // irrelevant messages.
                if timeout < passed {
                    timeout = Duration::from_secs(0);
                } else {
                    timeout = timeout - passed;
                }
            }
        }

        Err(last_err.unwrap())
    }

    fn next_nameserver(&mut self) -> SocketAddr {
        let n = self.next_ns;
        self.next_ns = (n + 1) % self.config.name_servers.len();
        self.config.name_servers[n]
    }
}

fn convert_error<T, F>(desc: &str, f: F) -> io::Result<T>
        where F: FnOnce() -> Result<T, Error> {
    match f() {
        Ok(t) => Ok(t),
        Err(Error::IoError(e)) => Err(e),
        Err(e) => Err(io::Error::new(
            io::ErrorKind::Other, format!("{}: {}", desc, e)))
    }
}

fn with_suffixes(host: &str, suffixes: &[String]) -> Vec<String> {
    let mut v = suffixes.iter()
        .map(|s| format!("{}.{}", host, s)).collect::<Vec<_>>();
    v.push(host.to_owned());
    v
}

fn span<F, R>(f: F) -> (Duration, R) where F: FnOnce() -> R {
    let mut r = None;
    let dur = Duration::span(|| { r = Some(f()); });
    (dur, r.unwrap())
}

#[cfg(unix)]
pub fn default_config() -> io::Result<DnsConfig> {
    use resolv_conf::load;
    load()
}

/// Resolves an IPv4 or IPv6 address to a hostname.
pub fn resolve_addr(addr: &IpAddr) -> io::Result<String> {
    let mut r = try!(DnsResolver::new(try!(default_config())));
    r.resolve_addr(addr)
}

/// Resolves a hostname to one or more IPv4 or IPv6 addresses.
///
/// # Example
///
/// ```no_run
/// use resolve::resolve_host;
/// # use std::io;
///
/// # fn _foo() -> io::Result<()> {
/// for addr in try!(resolve_host("rust-lang.org")) {
///     println!("found address: {}", addr);
/// }
/// # Ok(())
/// # }
/// ```
pub fn resolve_host(host: &str) -> io::Result<ResolveHost> {
    let mut r = try!(DnsResolver::new(try!(default_config())));
    r.resolve_host(host)
}

/// Yields a series of `IpAddr` values from `resolve_host`.
pub struct ResolveHost(IntoIter<IpAddr>);

impl Iterator for ResolveHost {
    type Item = IpAddr;

    fn next(&mut self) -> Option<IpAddr> {
        self.0.next()
    }
}
