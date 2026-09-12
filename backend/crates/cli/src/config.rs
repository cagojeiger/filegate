use std::fs::File;
use std::io::Read;

use reqwest::Url;
use reqwest::header::HeaderValue;

use crate::args::Args;
use crate::error::Error;

pub fn endpoint(raw: Option<&str>) -> Result<Url, Error> {
    let raw = raw.ok_or_else(|| Error::input("Set --endpoint or GROVE_ENDPOINT"))?;
    let invalid =
        || Error::input("Endpoint must be an HTTPS origin, or an HTTP literal loopback origin");
    if raw.trim() != raw || raw.contains('\\') || raw.chars().any(char::is_control) {
        return Err(invalid());
    }
    let url = Url::parse(raw).map_err(|_| invalid())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host_str().is_none()
    {
        return Err(invalid());
    }
    // Validate the supplied origin too: URL parsing normalizes dot paths and short IPv4 forms.
    let (_, authority) = raw.split_once("://").ok_or_else(invalid)?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.contains(['/', '@', '?', '#']) {
        return Err(invalid());
    }
    let literal_host = authority
        .strip_suffix(&format!(":{}", url.port_or_known_default().unwrap_or(0)))
        .unwrap_or(authority);
    match url.scheme() {
        "https" => Ok(url),
        "http" if matches!(literal_host, "localhost" | "127.0.0.1" | "[::1]") => Ok(url),
        _ => Err(invalid()),
    }
}

pub fn authorization(args: &Args) -> Result<HeaderValue, Error> {
    let token = if let Some(path) = &args.token_file {
        if !std::fs::metadata(path)
            .map_err(|_| Error::input("Cannot inspect token file"))?
            .is_file()
        {
            return Err(Error::input("Token file must be a regular file"));
        }
        let file = File::open(path).map_err(|_| Error::input("Cannot read token file"))?;
        if !file
            .metadata()
            .map_err(|_| Error::input("Cannot inspect token file"))?
            .is_file()
        {
            return Err(Error::input("Token file must be a regular file"));
        }
        let mut text = String::new();
        file.take(8193)
            .read_to_string(&mut text)
            .map_err(|_| Error::input("Cannot read UTF-8 token file"))?;
        if text.len() > 8192 {
            return Err(Error::input("Token file exceeds 8192 bytes"));
        }
        text.strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(&text)
            .to_owned()
    } else {
        std::env::var("GROVE_OPERATOR_TOKEN")
            .map_err(|_| Error::input("Set --token-file or GROVE_OPERATOR_TOKEN"))?
    };
    if token.is_empty() || token.len() > 8192 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::input(
            "Operator token must be a single nonempty ASCII bearer value",
        ));
    }
    let mut header = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| Error::input("Invalid operator token"))?;
    header.set_sensitive(true);
    Ok(header)
}
