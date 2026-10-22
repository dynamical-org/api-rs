//! CPython 3.12 `urllib.parse` splitting and joining, as the catalog uses them.
//! Components keep their original spelling; nothing is percent-decoded or
//! case-normalized except the scheme. Errors are Python's ValueError text.
use crate::CatalogError;
use dynapi_contract::output::python_repr;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ParseResult {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub params: String,
    pub query: String,
    pub fragment: String,
}
const USES_RELATIVE: &[&str] = &[
    "", "ftp", "http", "gopher", "nntp", "imap", "wais", "file", "https", "shttp", "mms",
    "prospero", "rtsp", "rtsps", "rtspu", "sftp", "svn", "svn+ssh", "ws", "wss",
];
const USES_NETLOC: &[&str] = &[
    "",
    "ftp",
    "http",
    "gopher",
    "nntp",
    "telnet",
    "imap",
    "wais",
    "file",
    "mms",
    "https",
    "shttp",
    "snews",
    "prospero",
    "rtsp",
    "rtsps",
    "rtspu",
    "rsync",
    "svn",
    "svn+ssh",
    "sftp",
    "nfs",
    "git",
    "git+ssh",
    "ws",
    "wss",
    "itms-services",
];
const USES_PARAMS: &[&str] = &[
    "", "ftp", "hdl", "prospero", "http", "imap", "https", "shttp", "rtsp", "rtsps", "rtspu",
    "sip", "sips", "mms", "sftp", "tel",
];
fn value_error(message: impl Into<String>) -> CatalogError {
    CatalogError::typed(message, "ValueError")
}
/// `ipaddress.ip_address` acceptance; std parsers plus Python's IPv6 scope IDs.
fn is_ip_address(text: &str) -> Option<bool> {
    if text.parse::<std::net::Ipv4Addr>().is_ok() {
        return Some(true);
    }
    let address = match text.split_once('%') {
        Some((_, scope)) if scope.is_empty() || scope.contains('%') => return None,
        Some((address, _)) => address,
        None => text,
    };
    address.parse::<std::net::Ipv6Addr>().ok().map(|_| false)
}
fn check_bracketed_netloc(netloc: &str) -> Result<(), CatalogError> {
    let host_and_port = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let hostname = match host_and_port.split_once('[') {
        Some((before, bracketed)) => {
            if !before.is_empty() {
                return Err(value_error("Invalid IPv6 URL"));
            }
            let (hostname, port) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            if !port.is_empty() && !port.starts_with(':') {
                return Err(value_error("Invalid IPv6 URL"));
            }
            hostname
        }
        None => host_and_port
            .split_once(':')
            .map_or(host_and_port, |(h, _)| h),
    };
    if let Some(future) = hostname.strip_prefix('v') {
        // \Av[a-fA-F0-9]+\..+\Z
        let hex = future
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(future.len());
        let rest = &future[hex..];
        if hex == 0 || !rest.starts_with('.') || rest.len() == 1 || rest[1..].contains('\n') {
            return Err(value_error("IPvFuture address is invalid"));
        }
        return Ok(());
    }
    match is_ip_address(hostname) {
        Some(true) => Err(value_error("An IPv4 address cannot be in brackets")),
        Some(false) => Ok(()),
        None => Err(value_error(format!(
            "{} does not appear to be an IPv4 or IPv6 address",
            python_repr(hostname)
        ))),
    }
}
fn check_netloc(netloc: &str) -> Result<(), CatalogError> {
    if netloc.is_ascii() {
        return Ok(());
    }
    let n = netloc.replace(['@', ':', '#', '?'], "");
    let normalized = icu_normalizer::ComposingNormalizerBorrowed::new_nfkc().normalize(&n);
    if normalized != n && normalized.contains(['/', '?', '#', '@', ':']) {
        return Err(value_error(format!(
            "netloc '{netloc}' contains invalid characters under NFKC normalization"
        )));
    }
    Ok(())
}
/// `urllib.parse.urlsplit`; `params` is always empty.
pub(crate) fn urlsplit(url: &str, default_scheme: &str) -> Result<ParseResult, CatalogError> {
    let mut url = url
        .trim_start_matches(|c: char| c <= ' ')
        .replace(['\t', '\r', '\n'], "");
    let mut scheme = default_scheme
        .trim_matches(|c: char| c <= ' ')
        .replace(['\t', '\r', '\n'], "");
    if let Some(i) = url.find(':')
        && i > 0
        && url.as_bytes()[0].is_ascii_alphabetic()
        && url[..i]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
    {
        scheme = url[..i].to_ascii_lowercase();
        url = url[i + 1..].to_string();
    }
    let mut netloc = String::new();
    if let Some(tail) = url.strip_prefix("//") {
        let end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
        netloc = tail[..end].to_string();
        url = tail[end..].to_string();
        if netloc.contains('[') != netloc.contains(']') {
            return Err(value_error("Invalid IPv6 URL"));
        }
        if netloc.contains('[') {
            check_bracketed_netloc(&netloc)?;
        }
    }
    let mut fragment = String::new();
    if let Some((before, after)) = url.split_once('#') {
        fragment = after.into();
        url = before.into();
    }
    let mut query = String::new();
    if let Some((before, after)) = url.split_once('?') {
        query = after.into();
        url = before.into();
    }
    check_netloc(&netloc)?;
    Ok(ParseResult {
        scheme,
        netloc,
        path: url,
        params: String::new(),
        query,
        fragment,
    })
}
/// `urllib.parse.urlparse`: parameters come only from the final path segment.
pub(crate) fn urlparse(url: &str, default_scheme: &str) -> Result<ParseResult, CatalogError> {
    let mut result = urlsplit(url, default_scheme)?;
    if USES_PARAMS.contains(&result.scheme.as_str()) && result.path.contains(';') {
        let from = result.path.rfind('/').unwrap_or(0);
        if let Some(i) = result.path[from..].find(';').map(|i| i + from) {
            result.params = result.path[i + 1..].into();
            result.path.truncate(i);
        }
    }
    Ok(result)
}
pub(crate) fn urlunparse(parts: &ParseResult) -> String {
    let mut url = parts.path.clone();
    if !parts.params.is_empty() {
        url = format!("{url};{}", parts.params);
    }
    if !parts.netloc.is_empty()
        || (!parts.scheme.is_empty()
            && USES_NETLOC.contains(&parts.scheme.as_str())
            && !url.starts_with("//"))
    {
        if !url.is_empty() && !url.starts_with('/') {
            url.insert(0, '/');
        }
        url = format!("//{}{url}", parts.netloc);
    }
    if !parts.scheme.is_empty() {
        url = format!("{}:{url}", parts.scheme);
    }
    if !parts.query.is_empty() {
        url = format!("{url}?{}", parts.query);
    }
    if !parts.fragment.is_empty() {
        url = format!("{url}#{}", parts.fragment);
    }
    url
}
/// `urllib.parse.urljoin(base, url)` for non-empty string arguments.
pub(crate) fn urljoin(base: &str, url: &str) -> Result<String, CatalogError> {
    if base.is_empty() {
        return Ok(url.into());
    }
    if url.is_empty() {
        return Ok(base.into());
    }
    let b = urlparse(base, "")?;
    let mut u = urlparse(url, &b.scheme)?;
    if u.scheme != b.scheme || !USES_RELATIVE.contains(&u.scheme.as_str()) {
        return Ok(url.into());
    }
    if USES_NETLOC.contains(&u.scheme.as_str()) {
        if !u.netloc.is_empty() {
            return Ok(urlunparse(&u));
        }
        u.netloc = b.netloc.clone();
    }
    if u.path.is_empty() && u.params.is_empty() {
        u.path = b.path;
        u.params = b.params;
        if u.query.is_empty() {
            u.query = b.query;
        }
        return Ok(urlunparse(&u));
    }
    let mut base_parts: Vec<&str> = b.path.split('/').collect();
    if base_parts.last() != Some(&"") {
        base_parts.pop();
    }
    let segments: Vec<&str> = if u.path.starts_with('/') {
        u.path.split('/').collect()
    } else {
        let mut segments = base_parts;
        segments.extend(u.path.split('/'));
        let last = segments.len() - 1;
        let mut filtered = vec![segments[0]];
        if last > 0 {
            filtered.extend(segments[1..last].iter().filter(|s| !s.is_empty()));
            filtered.push(segments[last]);
        }
        filtered
    };
    let mut resolved: Vec<&str> = vec![];
    for seg in &segments {
        match *seg {
            ".." => {
                resolved.pop();
            }
            "." => {}
            seg => resolved.push(seg),
        }
    }
    if matches!(segments.last(), Some(&".") | Some(&"..")) {
        resolved.push("");
    }
    let path = resolved.join("/");
    u.path = if path.is_empty() { "/".into() } else { path };
    Ok(urlunparse(&u))
}
