use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use url::{Host, Origin, Url};

use super::web2md::Web2MdError;

/// Optional host-provided DNS resolver. An empty result rejects the hostname.
/// When unset, the tool uses the operating system resolver.
pub type HostResolver = Arc<dyn Fn(&str) -> std::io::Result<Vec<IpAddr>> + Send + Sync>;

/// Limits and network policy shared by the tool and the extraction engine.
#[derive(Clone)]
pub struct WebFetchConfig {
    /// Image validation and resizing options for downloaded images.
    pub image: crate::ImageProcessConfig,
    pub max_output_bytes: usize,
    pub max_download_bytes: usize,
    /// Permit local destinations. IPv4 and IPv6 link-local addresses are always blocked.
    pub allow_private_hosts: bool,
    pub host_resolver: Option<HostResolver>,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            image: Default::default(),
            max_output_bytes: crate::DEFAULT_MAX_OUTPUT_BYTES,
            max_download_bytes: 10 * 1024 * 1024,
            allow_private_hosts: false,
            host_resolver: None,
        }
    }
}

impl WebFetchConfig {
    pub fn with_image(mut self, image: crate::ImageProcessConfig) -> Self {
        self.image = image;
        self
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
    private_origin: Option<&Origin>,
) -> Result<Vec<SocketAddr>, Web2MdError> {
    let allow_private =
        config.allow_private_hosts && private_origin.is_none_or(|origin| *origin == url.origin());
    let (private, addresses) = resolve_destination(url, config, allow_private).await?;
    if private && !allow_private {
        return Err(Web2MdError::PrivateAddress);
    }
    Ok(addresses)
}

pub(super) async fn resolve_destination(
    url: &Url,
    config: &WebFetchConfig,
    allow_local_names: bool,
) -> Result<(bool, Vec<SocketAddr>), Web2MdError> {
    check_url(url)?;
    let ip = match url.host().ok_or(Web2MdError::InvalidUrl)? {
        Host::Ipv4(ip) => IpAddr::V4(ip),
        Host::Ipv6(ip) => IpAddr::V6(ip),
        Host::Domain(host) => {
            let domain = host.trim_end_matches('.');
            let local = !domain.contains('.')
                || domain.ends_with(".localhost")
                || domain.ends_with(".local");
            if local && !allow_local_names {
                return Err(Web2MdError::PrivateAddress);
            }
            let port = url.port_or_known_default().ok_or(Web2MdError::InvalidUrl)?;
            let mut addresses: Vec<SocketAddr> = if let Some(resolve) = &config.host_resolver {
                resolve(domain)
                    .map_err(|_| Web2MdError::Dns)?
                    .into_iter()
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect()
            } else {
                tokio::net::lookup_host((host, port))
                    .await
                    .map_err(|_| Web2MdError::Dns)?
                    .collect()
            };
            if local {
                // Local DNS can return scoped IPv6 addresses alongside usable addresses.
                addresses.retain(
                    |addr| !matches!(addr.ip(), IpAddr::V6(ip) if ip.is_unicast_link_local()),
                );
            }
            if addresses.is_empty() || addresses.iter().any(|addr| link_local(addr.ip())) {
                return Err(Web2MdError::PrivateAddress);
            }
            let private = local || addresses.iter().any(|addr| !public_ip(addr.ip(), true));
            return Ok((private, addresses));
        }
    };
    if link_local(ip) {
        return Err(Web2MdError::PrivateAddress);
    }
    Ok((!public_ip(ip, false), Vec::new()))
}

fn link_local(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_unicast_link_local(),
    }
}

fn public_ip(ip: IpAddr, from_dns: bool) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => public_v4(ip, from_dns),
        IpAddr::V6(ip) => {
            // Global unicast only; also exclude transition, special-use and documentation ranges.
            let [a, b, ..] = ip.segments();
            (a & 0xe000) == 0x2000
                && a != 0x2002
                && !(a == 0x2001 && (b < 0x0200 || b == 0x0db8))
                && !(a == 0x3fff && b < 0x1000)
        }
    }
}

fn public_v4(ip: Ipv4Addr, from_dns: bool) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_documentation()
        || matches!(a, 0 | 224..)
        || (a == 100 && (64..128).contains(&b))
        || (!from_dns && a == 198 && matches!(b, 18 | 19))
        || (a == 192 && b == 0 && matches!(c, 0 | 2)))
}
