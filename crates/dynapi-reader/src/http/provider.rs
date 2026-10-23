//! Opt-in provider policy. This module grants no source eligibility or authority.
use super::transport::{BoundedRangeTransport, FetchContext, Policy};
use icechunk::{format::manifest::Checksum, virtual_chunks::HttpVirtualChunkResponse};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use std::{ops::Range, time::Duration};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    S3,
    Gcs,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProviderFailure {
    #[error("Provider range fetch failed")]
    Generic,
    #[error("Provider rate limited the terminal attempt")]
    SlowDown,
    #[error("Provider operation deadline expired")]
    OperationTimeout,
    #[error("Provider object changed or required validator is missing")]
    ObjectModified,
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderTransport {
    pub(super) inner: BoundedRangeTransport,
    provider: Provider,
}
impl ProviderTransport {
    pub(crate) fn new(provider: Provider) -> Result<Self, reqwest::Error> {
        Ok(Self {
            inner: BoundedRangeTransport::new(true)?,
            provider,
        })
    }
    pub(crate) fn for_provider(&self, provider: Provider) -> Self {
        Self {
            inner: self.inner.clone(),
            provider,
        }
    }
    pub(crate) async fn fetch(
        &self,
        url: &str,
        range: &Range<u64>,
        checksum: Option<&Checksum>,
    ) -> Result<HttpVirtualChunkResponse, ProviderFailure> {
        self.fetch_until(
            url,
            range,
            checksum,
            Instant::now() + Duration::from_secs(30),
        )
        .await
    }
    async fn fetch_until(
        &self,
        url: &str,
        range: &Range<u64>,
        checksum: Option<&Checksum>,
        deadline: Instant,
    ) -> Result<HttpVirtualChunkResponse, ProviderFailure> {
        let context = FetchContext::new(Policy::Provider(self.provider));
        let operation = async {
            let length = range
                .end
                .checked_sub(range.start)
                .filter(|n| *n > 0 && *n <= 8 * 1024 * 1024)
                .ok_or(ProviderFailure::Generic)?;
            let url = reqwest::Url::parse(url).map_err(|_| ProviderFailure::Generic)?;
            let response = self
                .inner
                .fetch(&url, range, checksum, context.clone())
                .await
                .map_err(|_| context.failure())?;
            if response.data.len() as u64 != length {
                return Err(ProviderFailure::Generic);
            }
            match checksum {
                Some(Checksum::ETag(expected))
                    if !response.etag.as_ref().is_some_and(|actual| {
                        icechunk::storage::strip_quotes(actual)
                            == icechunk::storage::strip_quotes(&expected.0)
                    }) =>
                {
                    return Err(ProviderFailure::ObjectModified);
                }
                Some(Checksum::LastModified(cutoff))
                    if !response
                        .last_modified
                        .is_some_and(|actual| actual <= cutoff.0) =>
                {
                    return Err(ProviderFailure::ObjectModified);
                }
                _ => {}
            }
            Ok(response)
        };
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => Err(ProviderFailure::OperationTimeout),
            result = operation => result,
        }
    }
}

fn xml_name(name: &[u8]) -> bool {
    name.first()
        .is_some_and(|b| b.is_ascii_alphabetic() || matches!(b, b'_' | b':'))
        && name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.'))
}
fn valid_attributes(start: &BytesStart<'_>) -> bool {
    if !xml_name(start.name().as_ref()) {
        return false;
    }
    // quick-xml accepts adjacent quoted attributes in unchecked iteration.
    // Require XML whitespace at each attribute boundary before iterating.
    let raw: &[u8] = start.as_ref();
    let mut at = start.name().as_ref().len();
    while at < raw.len() {
        if !matches!(raw[at], b' ' | b'\t' | b'\r' | b'\n') {
            return false;
        }
        while at < raw.len() && matches!(raw[at], b' ' | b'\t' | b'\r' | b'\n') {
            at += 1;
        }
        if at == raw.len() {
            break;
        }
        while at < raw.len() && raw[at] != b'=' && !matches!(raw[at], b' ' | b'\t' | b'\r' | b'\n')
        {
            at += 1;
        }
        while at < raw.len() && matches!(raw[at], b' ' | b'\t' | b'\r' | b'\n') {
            at += 1;
        }
        if raw.get(at) != Some(&b'=') {
            return false;
        }
        at += 1;
        while at < raw.len() && matches!(raw[at], b' ' | b'\t' | b'\r' | b'\n') {
            at += 1;
        }
        let Some(&quote) = raw.get(at).filter(|q| matches!(q, b'\'' | b'"')) else {
            return false;
        };
        at += 1;
        while at < raw.len() && raw[at] != quote {
            at += 1;
        }
        if at == raw.len() {
            return false;
        }
        at += 1;
    }
    // A fixed duplicate-key set bounds scratch even for a densely packed tag.
    // quick-xml still checks attribute syntax; its growing duplicate-key Vec is disabled.
    let mut names: [&[u8]; 32] = [&[]; 32];
    for (index, attr) in start.attributes().with_checks(false).enumerate() {
        if index == names.len() {
            return false;
        }
        let Ok(attr) = attr else {
            return false;
        };
        let name = attr.key.into_inner();
        if !xml_name(name) || attr.value.contains(&b'<') || names[..index].contains(&name) {
            return false;
        }
        names[index] = name;
    }
    true
}

/// Borrow events from a bounded UTF-8 body, with a fixed borrowed-name stack.
/// Duplicate checks use a fixed set of 32 borrowed keys; parser errors can allocate bounded scratch.
/// No entity expansion, DOM, Serde or read_text/read_to_end is used.
pub(crate) fn slow_down(body: &[u8]) -> bool {
    if body.len() > 4096
        || std::str::from_utf8(body).map_or(true, |text| {
            text.chars().any(|c| matches!(c, '\u{fffe}' | '\u{ffff}'))
        })
        || body
            .iter()
            .any(|b| *b == b'&' || (*b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r')))
    {
        return false;
    }
    let mut reader = Reader::from_reader(body);
    reader.config_mut().check_end_names = false;
    reader.config_mut().check_comments = true;
    let mut stack: [Option<BytesStart<'_>>; 8] = std::array::from_fn(|_| None);
    let mut depth = 0;
    let mut events = 0;
    let mut root = false;
    let mut declaration = false;
    let mut codes = 0;
    let mut code = [0u8; 64];
    let mut code_len = 0;
    loop {
        events += 1;
        if events > 256 {
            return false;
        }
        let before_event = reader.buffer_position();
        let Ok(event) = reader.read_event() else {
            return false;
        };
        match event {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let start = match event {
                    Event::Start(start) | Event::Empty(start) => start,
                    _ => unreachable!(),
                };
                let name = start.name().into_inner();
                if depth == 8
                    || !valid_attributes(&start)
                    || (depth > 0
                        && stack[depth - 1]
                            .as_ref()
                            .is_some_and(|tag| tag.name().as_ref() == b"Code"))
                {
                    return false;
                }
                if depth == 0 {
                    if root || name != b"Error" {
                        return false;
                    }
                    root = true;
                }
                if name == b"Code" {
                    codes += 1;
                    if codes != 1 || depth != 1 || start.attributes().next().is_some() {
                        return false;
                    }
                }
                if !empty {
                    stack[depth] = Some(start);
                    depth += 1;
                }
            }
            Event::End(end) => {
                if depth == 0
                    || !stack[depth - 1]
                        .as_ref()
                        .is_some_and(|tag| tag.name() == end.name())
                {
                    return false;
                }
                depth -= 1;
                stack[depth] = None;
            }
            Event::Text(text) => {
                let bytes: &[u8] = text.as_ref();
                if bytes.windows(3).any(|w| w == b"]]>") {
                    return false;
                }
                if depth == 0 && !bytes.iter().all(u8::is_ascii_whitespace) {
                    return false;
                }
                if depth > 0
                    && stack[depth - 1]
                        .as_ref()
                        .is_some_and(|tag| tag.name().as_ref() == b"Code")
                {
                    if !bytes.is_ascii() || bytes.len() > code.len() - code_len {
                        return false;
                    }
                    code[code_len..code_len + bytes.len()].copy_from_slice(bytes);
                    code_len += bytes.len();
                }
            }
            Event::Decl(decl) => {
                if root || declaration || before_event != 0 {
                    return false;
                }
                declaration = true;
                let Ok(content) = std::str::from_utf8(decl.as_ref()) else {
                    return false;
                };
                let attrs = BytesStart::from_content(content, 3);
                if !valid_attributes(&attrs) || !decl.version().is_ok_and(|v| v.as_ref() == b"1.0")
                {
                    return false;
                }
                for attr in attrs.attributes().with_checks(false) {
                    let Ok(attr) = attr else {
                        return false;
                    };
                    match attr.key.as_ref() {
                        b"version" if attr.value.as_ref() == b"1.0" => {}
                        b"encoding" if attr.value.eq_ignore_ascii_case(b"UTF-8") => {}
                        b"standalone" if matches!(attr.value.as_ref(), b"yes" | b"no") => {}
                        _ => return false,
                    }
                }
            }
            Event::Comment(_) => {
                if depth > 0
                    && stack[depth - 1]
                        .as_ref()
                        .is_some_and(|tag| tag.name().as_ref() == b"Code")
                {
                    return false;
                }
            }
            Event::Eof => {
                return root && depth == 0 && codes == 1 && &code[..code_len] == b"SlowDown";
            }
            Event::CData(_) | Event::DocType(_) | Event::PI(_) | Event::GeneralRef(_) => {
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests;
