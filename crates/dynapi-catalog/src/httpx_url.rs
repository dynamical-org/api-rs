//! Request URL semantics from httpx 0.28.1 `_urlparse.py` and `_urls.py`.
//! This parser is independent of WHATWG: only its normalized result is handed
//! to reqwest, and the resulting destination and raw target must compare equal.
use crate::CatalogError;

#[derive(Clone, Debug)]
pub(crate) struct HttpxUrl {
    scheme: String,
    userinfo: String,
    host: String,
    port: Option<String>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
}
fn invalid() -> CatalogError {
    CatalogError::typed("STAC URL cannot be requested as written", "InvalidURL")
}
#[derive(Clone, Copy)]
enum Component {
    Path,
    Query,
    Fragment,
    Userinfo,
    Host,
}
/// httpx preserves existing escapes, including their case. A bare '%' is safe
/// in paths/queries/fragments/hosts, but must be quoted in userinfo.
fn quote(text: &str, component: Component) -> String {
    let mut result = String::new();
    let mut bytes = text.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let mut rest = bytes.clone();
            if rest.next().is_some_and(|b| b.is_ascii_hexdigit())
                && rest.next().is_some_and(|b| b.is_ascii_hexdigit())
            {
                result.push('%');
                result.push(bytes.next().unwrap() as char);
                result.push(bytes.next().unwrap() as char);
                continue;
            }
        }
        let safe = match component {
            Component::Path => (0x21..0x7f).contains(&b) && !b"\"#<>?`{}".contains(&b),
            Component::Query => (0x21..0x7f).contains(&b) && !b"\"#<>".contains(&b),
            Component::Fragment => (0x21..0x7f).contains(&b) && !b"\"<>`".contains(&b),
            Component::Userinfo => {
                (0x21..0x7f).contains(&b) && !b"\"#<>?`{}/;=@[\\]^|%".contains(&b)
            }
            Component::Host => {
                b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=\"`{}%|\\".contains(&b)
            }
        };
        if safe {
            result.push(b as char);
        } else {
            use std::fmt::Write;
            write!(result, "%{b:02X}").unwrap();
        }
    }
    result
}
/// httpx's normalize_path removes literal dot components without appending a
/// slash for terminal dots. Encoded dots are never decoded here.
fn normalize_path(path: &str) -> String {
    let mut output = Vec::new();
    for component in path.split('/') {
        match component {
            "." => {}
            ".." => {
                if !output.is_empty() && output != [""] {
                    output.pop();
                }
            }
            component => output.push(component),
        }
    }
    output.join("/")
}
/// URL.fragment uses urllib.unquote (UTF-8 with replacement), rather than
/// returning the escaped component. copy_with then validates the decoded text.
#[cfg(any(feature = "native", test))]
fn unquote(text: &str) -> String {
    let mut bytes = Vec::new();
    let mut remaining = text.as_bytes();
    while let Some((&b, tail)) = remaining.split_first() {
        if b == b'%'
            && tail.len() >= 2
            && let (Some(a), Some(b)) = (
                (tail[0] as char).to_digit(16),
                (tail[1] as char).to_digit(16),
            )
        {
            bytes.push((a * 16 + b) as u8);
            remaining = &tail[2..];
        } else {
            bytes.push(b);
            remaining = tail;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
fn encode_host(host: &str) -> Result<String, CatalogError> {
    // Keep unsupported hosts until the final gate, outside the retryable
    // Location parse boundary. No IDNA conversion is performed.
    if !host.is_ascii() {
        return Ok(host.into());
    }
    if host.starts_with('[') && host.ends_with(']') {
        let ip = &host[1..host.len() - 1];
        // Python validates scoped IPv6 but retains its original spelling.
        let address = if let Some((address, scope)) = ip.split_once('%') {
            if scope.is_empty() || scope.contains('%') {
                return Err(invalid());
            }
            address
        } else {
            ip
        };
        address
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| invalid())?;
        return Ok(ip.into());
    }
    let labels: Vec<_> = host.split('.').collect();
    if labels.len() == 4
        && labels
            .iter()
            .all(|label| !label.is_empty() && label.bytes().all(|b| b.is_ascii_digit()))
    {
        host.parse::<std::net::Ipv4Addr>().map_err(|_| invalid())?;
        return Ok(host.into());
    }
    Ok(quote(&host.to_ascii_lowercase(), Component::Host))
}
fn parse_port(text: &str) -> Result<String, CatalogError> {
    let text = text.trim();
    let negative = text.starts_with('-');
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    let mut value = String::new();
    let mut previous_digit = false;
    for c in digits.chars() {
        if c == '_' && previous_digit {
            previous_digit = false;
            continue;
        }
        let cp = c as u32;
        let i = DECIMAL_ZEROS.partition_point(|&zero| zero <= cp);
        let digit = i
            .checked_sub(1)
            .map(|i| cp - DECIMAL_ZEROS[i])
            .filter(|&n| n < 10)
            .ok_or_else(invalid)? as u8;
        value.push(char::from(b'0' + digit));
        previous_digit = true;
    }
    if !previous_digit || value.len() > 4300 {
        return Err(invalid());
    }
    let digits = value.trim_start_matches('0');
    Ok(if digits.is_empty() {
        "0".into()
    } else if negative {
        format!("-{digits}")
    } else {
        digits.into()
    })
}
impl HttpxUrl {
    pub(crate) fn parse(text: &str) -> Result<Self, CatalogError> {
        if text.chars().count() > 65536 {
            return Err(CatalogError::typed("URL too long", "InvalidURL"));
        }
        if let Some((position, character)) =
            text.chars().enumerate().find(|(_, c)| c.is_ascii_control())
        {
            return Err(CatalogError::typed(
                format!(
                    "Invalid non-printable ASCII character in URL, {} at position {position}.",
                    dynapi_contract::output::python_repr(&character.to_string())
                ),
                "InvalidURL",
            ));
        }
        let (before_fragment, fragment) = text
            .split_once('#')
            .map_or((text, None), |(a, b)| (a, Some(b)));
        let (mut remaining, query) = before_fragment
            .split_once('?')
            .map_or((before_fragment, None), |(a, b)| (a, Some(b)));
        let mut scheme = "";
        if let Some((candidate, tail)) = remaining.split_once(':')
            && (candidate.is_empty()
                || (candidate.as_bytes()[0].is_ascii_alphabetic()
                    && candidate
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b))))
        {
            scheme = candidate;
            remaining = tail;
        }
        let mut authority = "";
        if let Some(tail) = remaining.strip_prefix("//") {
            let end = tail.find('/').unwrap_or(tail.len());
            authority = &tail[..end];
            remaining = &tail[end..];
        }
        let (userinfo, host_port) = authority.rsplit_once('@').unwrap_or(("", authority));
        let (host, port) = if host_port.starts_with('[') && host_port.contains(']') {
            let end = host_port.rfind(']').unwrap() + 1;
            (
                &host_port[..end],
                host_port[end..]
                    .strip_prefix(':')
                    .unwrap_or(&host_port[end..]),
            )
        } else {
            host_port.split_once(':').unwrap_or((host_port, ""))
        };
        let port = if port.is_empty() {
            None
        } else {
            // Keep Python's arbitrary signed integer here. Transport bounds
            // belong to the final gate, not the retryable Location parse step.
            Some(parse_port(port)?)
        };
        let host = encode_host(host)?;
        let userinfo = quote(userinfo, Component::Userinfo);
        let has_authority = !host.is_empty() || !userinfo.is_empty() || port.is_some();
        if has_authority && !remaining.is_empty() && !remaining.starts_with('/') {
            return Err(invalid());
        }
        if scheme.is_empty()
            && !has_authority
            && (remaining.starts_with("//") || remaining.starts_with(':'))
        {
            return Err(invalid());
        }
        let path = if !scheme.is_empty() || has_authority {
            normalize_path(remaining)
        } else {
            remaining.into()
        };
        let mut result = Self {
            scheme: scheme.to_ascii_lowercase(),
            userinfo,
            host,
            port,
            path: quote(&path, Component::Path),
            query: query.map(|s| quote(s, Component::Query)),
            fragment: fragment.map(|s| quote(s, Component::Fragment)),
        };
        // httpx normalizes the scheme separately, but its default-port lookup
        // uses the original scheme spelling. Keep that distinction for the
        // final comparison: it can also affect the Host header's port suffix.
        if scheme == result.scheme
            && result.port.as_deref() == result.default_port().map(|p| p.to_string()).as_deref()
        {
            result.port = None;
        }
        Ok(result)
    }
    fn default_port(&self) -> Option<u16> {
        match self.scheme.as_str() {
            "http" | "ws" => Some(80),
            "https" | "wss" => Some(443),
            "ftp" => Some(21),
            _ => None,
        }
    }
    fn as_string(&self) -> String {
        let mut text = String::new();
        if !self.scheme.is_empty() {
            text.push_str(&self.scheme);
            text.push(':');
        }
        if !self.host.is_empty() || !self.userinfo.is_empty() || self.port.is_some() {
            text.push_str("//");
            if !self.userinfo.is_empty() {
                text.push_str(&self.userinfo);
                text.push('@');
            }
            if self.host.contains(':') {
                text.push('[');
                text.push_str(&self.host);
                text.push(']');
            } else {
                text.push_str(&self.host);
            }
            if let Some(port) = &self.port {
                text.push(':');
                text.push_str(port);
            }
        }
        text.push_str(&self.path);
        if let Some(query) = &self.query {
            text.push('?');
            text.push_str(query);
        }
        if let Some(fragment) = &self.fragment {
            text.push('#');
            text.push_str(fragment);
        }
        text
    }
    fn check_host(&self) -> Result<(), CatalogError> {
        if !self.host.is_ascii() || self.host.split('.').any(|label| label.starts_with("xn--")) {
            return Err(invalid());
        }
        Ok(())
    }
    /// The single final gate before GET. Fragments never enter the wire target.
    pub(crate) fn request_url(&self) -> Result<url::Url, CatalogError> {
        self.check_host()?;
        let port = self
            .port
            .as_deref()
            .map(str::parse::<u16>)
            .transpose()
            .map_err(|_| invalid())?;
        let parsed = url::Url::parse(&self.as_string()).map_err(|_| invalid())?;
        let host = parsed.host_str().unwrap_or("");
        let host = host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(host);
        let path = if self.path.is_empty() {
            "/"
        } else {
            &self.path
        };
        if parsed.scheme() != self.scheme
            || host != self.host
            || parsed.port() != port
            || parsed.path() != path
            || parsed.query() != self.query.as_deref()
        {
            return Err(invalid());
        }
        Ok(parsed)
    }
    /// httpx Client._redirect_url: fill only the host for scheme-without-host,
    /// use absolute Locations directly, and urljoin only relative Locations.
    #[cfg(any(feature = "native", test))]
    pub(crate) fn redirect(&self, location: &str) -> Result<Self, CatalogError> {
        let mut next = Self::parse(location).map_err(|error| {
            CatalogError::typed(
                format!("Invalid URL in location header: {error}."),
                "RemoteProtocolError",
            )
        })?;
        next.check_host()?;
        if !next.scheme.is_empty() && next.host.is_empty() {
            if !next.path.is_empty() && !next.path.starts_with('/') {
                return Err(invalid());
            }
            next.host = encode_host(&self.host)?;
            // copy_with(host=...) reparses and normalizes the path.
            next = Self::parse(&next.as_string())?;
        }
        if next.scheme.is_empty() || next.host.is_empty() {
            next = Self::parse(&crate::pyurl::urljoin(
                &self.as_string(),
                &next.as_string(),
            )?)?;
        }
        if self.fragment.as_ref().is_some_and(|s| !s.is_empty())
            && next.fragment.as_ref().is_none_or(|s| s.is_empty())
        {
            let fragment = unquote(self.fragment.as_deref().unwrap());
            if fragment.chars().any(|c| c.is_ascii_control()) {
                return Err(invalid());
            }
            next.fragment = Some(quote(&fragment, Component::Fragment));
            next = Self::parse(&next.as_string())?;
        }
        Ok(next)
    }
}

// Unicode 15.0 decimal-digit block starts for the pinned numeric grammar.
const DECIMAL_ZEROS: &[u32] = &[
    0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
    0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
    0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10,
    0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0, 0x11650,
    0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60, 0x16ac0,
    0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e950,
    0x1fbf0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internationalized_hosts_fail_at_the_final_request_gate() {
        for host in [
            "é.test",
            "💧.test",
            "example.：test",
            "[fe80::1%é]",
            "xn--9ca.test",
            "XN--9CA.test",
            "sub.xN--9ca.test",
            "xn--.test",
        ] {
            let text = format!("http://{host}/root.json");
            assert_eq!(
                HttpxUrl::parse(&text)
                    .and_then(|next| next.request_url())
                    .unwrap_err()
                    .exception_type,
                "InvalidURL",
                "{text}"
            );
            let base = HttpxUrl::parse("http://example.test/root.json").unwrap();
            for location in [text, format!("//{host}/child.json")] {
                assert_eq!(
                    base.redirect(&location)
                        .and_then(|next| next.request_url())
                        .unwrap_err()
                        .exception_type,
                    "InvalidURL",
                    "{location}"
                );
            }
        }
    }

    #[test]
    fn normalized_port_state_must_match_even_when_the_effective_port_agrees() {
        // Policy: reject a target when explicit default-port state differs
        // between parsers, including uppercase-scheme spellings.
        for text in ["HTTP://example.test:80/a", "HTTPS://example.test:443/a"] {
            let parsed = HttpxUrl::parse(text).unwrap();
            assert!(parsed.port.is_some());
            assert_eq!(
                parsed.request_url().unwrap_err().exception_type,
                "InvalidURL"
            );
        }
        for text in ["http://example.test:80/a", "https://example.test:443/a"] {
            assert!(HttpxUrl::parse(text).unwrap().request_url().is_ok());
        }
    }

    #[test]
    fn unrepresentable_redirect_ports_fail_at_the_final_gate_not_location_parsing() {
        let base = HttpxUrl::parse("http://example.test:8123/root.json").unwrap();
        for location in ["http://example.test:65536/a", "http://example.test:-1/a"] {
            let next = base.redirect(location).unwrap();
            assert_eq!(next.request_url().unwrap_err().exception_type, "InvalidURL");
        }
    }
}
