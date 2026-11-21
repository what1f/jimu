use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use url::{Host, Url};

use super::web2md::Web2MdError;

/// Optional host-provided DNS resolver. An empty result rejects the hostname.
/// When unset, the tool uses the operating system resolver.
pub type HostResolver = Arc<dyn Fn(&str) -> std::io::Result<Vec<IpAddr>> + Send + Sync>;

/// Limits and network policy shared by the tool and the extraction engine.
#[derive(Clone)]
pub struct WebFetchConfig {
    pub max_output_bytes: usize,
    pub max_download_bytes: usize,
    pub allow_private_hosts: bool,
    pub host_resolver: Option<HostResolver>,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            max_output_bytes: 128 * 1024,
            max_download_bytes: 10 * 1024 * 1024,
            allow_private_hosts: false,
            host_resolver: None,
        }
    }
}

pub(super) fn parse_url(input: &str) -> Result<Url, Web2MdError> {
    let url = Url::parse(input.trim()).map_err(|_| Web2MdError::InvalidUrl)?;
    check_url(&url)?;
    Ok(url)
}

pub(super) fn check_url(url: &Url) -> Result<(), Web2MdError> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Web2MdError::InvalidUrl);
    }
    Ok(())
}

pub(super) async fn check_destination(
    url: &Url,
    config: &WebFetchConfig,
) -> Result<Vec<SocketAddr>, Web2MdError> {
    check_url(url)?;
    if config.allow_private_hosts {
        return Ok(Vec::new());
    }
    match url.host().ok_or(Web2MdError::InvalidUrl)? {
        Host::Ipv4(ip) => {
            if !public_ip(ip.into()) {
                return Err(Web2MdError::PrivateAddress);
            }
            Ok(Vec::new())
        }
        Host::Ipv6(ip) => {
            if !public_ip(ip.into()) {
                return Err(Web2MdError::PrivateAddress);
            }
            Ok(Vec::new())
        }
        Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.');
            if !domain.contains('.') || domain.ends_with(".localhost") || domain.ends_with(".local")
            {
                return Err(Web2MdError::PrivateAddress);
            }
            let port = url.port_or_known_default().ok_or(Web2MdError::InvalidUrl)?;
            let addresses: Vec<SocketAddr> = if let Some(resolve) = &config.host_resolver {
                resolve(domain)
                    .map_err(|_| Web2MdError::Dns)?
                    .into_iter()
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect()
            } else {
                tokio::net::lookup_host((domain, port))
                    .await
                    .map_err(|_| Web2MdError::Dns)?
                    .collect()
            };
            if addresses.is_empty() || !addresses.iter().all(|addr| public_ip(addr.ip())) {
                return Err(Web2MdError::PrivateAddress);
            }
            Ok(addresses)
        }
    }
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return public_v4(mapped);
            }
            // Global unicast only; also exclude transition, special-use and documentation ranges.
            let [a, b, ..] = ip.segments();
            (a & 0xe000) == 0x2000 && a != 0x2002 && !(a == 0x2001 && (b < 0x0200 || b == 0x0db8))
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_documentation()
        || matches!(a, 0 | 224..)
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && matches!(b, 18 | 19))
        || (a == 192 && b == 0 && matches!(c, 0 | 2)))
}
