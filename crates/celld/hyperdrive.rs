// Copyright 2026 Deno Land Inc. Apache-2.0 license.

//! Hyperdrive bindings: a PostgreSQL endpoint the operator chooses at deploy
//! time, reached directly. celld has no pooler and no cache, so the binding is
//! the validated connection string and the fields it describes.
//!
//! The connection string reaches the Worker verbatim, and the driver parses it
//! again. `HyperdriveConfig::parse` therefore accepts only a subset of the
//! URLs a driver would accept: the strings for which the pinned
//! `pg-connection-string` reads the same host, port, user, password, and
//! database that the binding's fields report. Every rule below closes a case
//! where the two readings differ, and a string outside the subset is refused
//! rather than rewritten, so the operator's string is never changed.
use anyhow::anyhow;
use std::fmt;

const DEFAULT_PORT: u16 = 5432;

/// The query keys that neither move the endpoint nor change identity or TLS.
/// `pg` sends these to the server or applies them as client timeouts. Every
/// other key is refused, including `ssl` and the `ssl*` file keys, which reach
/// the driver's TLS config without passing through `sslmode`.
const QUERY_KEYS: &[&str] = &[
    "sslmode",
    "application_name",
    "fallback_application_name",
    "options",
    "connect_timeout",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "query_timeout",
];

/// The escapes `decodeURI` leaves encoded, which `pg-connection-string` uses
/// for the database. Rust decodes them, so a database name holding one would
/// reach the driver differently from the field.
const UNDECODED_BY_DECODE_URI: &[u8] = b"#$&+,/:;=?@";

/// Whether the driver must negotiate TLS. celld verifies every TLS connection,
/// so the verifying `sslmode` values are one mode here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tls {
    Disabled,
    Required,
}

/// One validated Hyperdrive binding.
///
/// The fields are private and `parse` is the only constructor, so a value of
/// this type is proof that the string passed validation.
#[derive(Clone)]
pub struct HyperdriveConfig {
    name: String,
    /// As the operator supplied it.
    connection_string: String,
    /// URL form: an IPv6 literal keeps its brackets.
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
    tls: Tls,
}

impl HyperdriveConfig {
    /// Validate `raw` as the connection string of the binding `name`. Errors
    /// name the binding and the offending part, never the string itself.
    pub fn parse(name: &str, raw: &str) -> anyhow::Result<Self> {
        let error = |message: String| anyhow!("Hyperdrive binding {name:?}: {message}");
        // Whitespace, a stray `%`, or a non-ASCII character makes `pg` repair
        // the whole string with `encodeURI`, which re-encodes valid escapes
        // elsewhere. A `#` starts a fragment the driver drops.
        let bytes = raw.as_bytes();
        for (index, &byte) in bytes.iter().enumerate() {
            if !byte.is_ascii_graphic() {
                return Err(error(
                    "the connection string contains whitespace, a control character, or a \
                     non-ASCII character; percent-encode it, and write a host name in its \
                     ASCII form"
                        .into(),
                ));
            }
            if byte == b'#' {
                return Err(error(
                    "the connection string contains `#`; percent-encode it as %23".into(),
                ));
            }
            if byte == b'%'
                && !(bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                    && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit))
            {
                return Err(error(
                    "the connection string has a `%` that does not begin a two-digit escape; \
                     write %25 for a literal `%`"
                        .into(),
                ));
            }
        }

        let rest = match raw.split_once("://") {
            Some(("postgres" | "postgresql", rest)) => rest,
            Some(("mysql", _)) => {
                return Err(error(
                    "MySQL is not supported; celld supports PostgreSQL only".into(),
                ))
            }
            _ => {
                return Err(error(
                    "the connection string must start with postgres:// or postgresql://".into(),
                ))
            }
        };
        let (authority, rest) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
        let (path, query) = match rest.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (rest, None),
        };

        // An empty user makes `pg` fall back to PGUSER or the OS user, which is
        // an ambient choice rather than the operator's.
        let (userinfo, host_port) = authority.rsplit_once('@').ok_or_else(|| {
            error("the connection string has no user; write postgres://USER@HOST/DATABASE".into())
        })?;
        let (user, password) = match userinfo.split_once(':') {
            Some((user, password)) => (user, password),
            None => (userinfo, ""),
        };
        let user = decode_userinfo(user, false).map_err(|why| error(format!("the user {why}")))?;
        if user.is_empty() {
            return Err(error("the connection string has an empty user".into()));
        }
        // An empty password is legitimate behind a pooler with trust auth.
        let password =
            decode_userinfo(password, true).map_err(|why| error(format!("the password {why}")))?;

        let (host, port) = split_host_port(host_port).map_err(|why| error(why.into()))?;
        let port = match port {
            None => DEFAULT_PORT,
            Some(port) => port
                .bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| port.parse::<u16>().ok())
                .flatten()
                .filter(|&port| port != 0)
                .ok_or_else(|| error("the port must be a number from 1 to 65535".into()))?,
        };

        // `…@host` and `…@host/` give `pg` no database, and it then connects to
        // the database named after the user: a different one from what the
        // binding reports.
        let database = path.strip_prefix('/').unwrap_or(path);
        if database.is_empty() {
            return Err(error(
                "the connection string has no database; write postgres://USER@HOST/DATABASE".into(),
            ));
        }
        let database =
            decode_database(database).map_err(|why| error(format!("the database {why}")))?;

        let tls = query_tls(query.unwrap_or("")).map_err(error)?;

        Ok(Self {
            name: name.to_string(),
            connection_string: raw.to_string(),
            host: host.to_string(),
            port,
            user,
            password,
            database,
            tls,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `postgres://host:port/database`, for reports and logs.
    pub fn redacted(&self) -> String {
        format!("postgres://{}:{}/{}", self.host, self.port, self.database)
    }

    /// The object the harness installs as the binding. Rust computes every
    /// field, so the JavaScript side never parses the URL.
    pub fn descriptor_json(&self) -> String {
        serde_json::json!({
            "connectionString": self.connection_string,
            "host": self.host,
            "port": self.port,
            "user": self.user,
            "password": self.password,
            "database": self.database,
            "tlsRequired": self.tls == Tls::Required,
        })
        .to_string()
    }
}

/// Never prints the connection string or the password.
impl fmt::Debug for HyperdriveConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyperdriveConfig")
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("database", &self.database)
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

/// Split `host[:port]`. A host is a bracketed IPv6 literal or a name made of
/// letters, digits, `-`, `.`, and `_`. Escapes are refused: `pg` reads a host
/// starting with `%2F` as a Unix-socket path.
fn split_host_port(host_port: &str) -> Result<(&str, Option<&str>), &'static str> {
    let (host, port) = if host_port.starts_with('[') {
        let end = host_port
            .find(']')
            .ok_or("the host has an unterminated IPv6 literal")?;
        let (host, after) = host_port.split_at(end + 1);
        let address = host[1..end]
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| "the host is not a valid IPv6 literal")?;
        // The driver's URL parser rewrites any other spelling, so the field
        // and the driver would name one address two ways.
        if host[1..end] != canonical_ipv6(address) {
            return Err(
                "the host's IPv6 address must be in its canonical form: lowercase, \
                        without leading zeros, with `::` for the longest run of zero groups, \
                        and without a dotted IPv4 part",
            );
        }
        match after {
            "" => (host, None),
            after => (
                host,
                Some(
                    after
                        .strip_prefix(':')
                        .ok_or("the host is followed by something other than a port")?,
                ),
            ),
        }
    } else {
        let (host, port) = match host_port.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        };
        if host.is_empty() {
            return Err("the connection string has no host");
        }
        if !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
        {
            return Err(
                "the host must be a name of ASCII letters, digits, `-`, `.`, and `_`, \
                        an IPv4 address, or a bracketed IPv6 address, without escapes",
            );
        }
        (host, port)
    };
    Ok((host, port))
}

/// The WHATWG URL serialization of an IPv6 address: lowercase hexadecimal
/// groups, the first longest run of two or more zero groups as `::`, and no
/// dotted IPv4 form. Rust's `Display` differs for IPv4-mapped addresses.
fn canonical_ipv6(address: std::net::Ipv6Addr) -> String {
    let groups = address.segments();
    let (mut compress, mut longest, mut run) = (None, 1, 0);
    for (index, &group) in groups.iter().enumerate() {
        run = if group == 0 { run + 1 } else { 0 };
        if run > longest {
            longest = run;
            compress = Some(index + 1 - run);
        }
    }
    let mut out = String::new();
    let mut index = 0;
    while index < groups.len() {
        if Some(index) == compress {
            out.push_str(if index == 0 { "::" } else { ":" });
            index += longest;
            continue;
        }
        out.push_str(&format!("{:x}", groups[index]));
        if index != groups.len() - 1 {
            out.push(':');
        }
        index += 1;
    }
    out
}

/// Decode a user or a password. Raw characters are limited to the ones that
/// survive the driver's URL parse unchanged; anything else must be escaped.
fn decode_userinfo(raw: &str, password: bool) -> Result<String, String> {
    if let Some(byte) = raw.bytes().find(|&byte| {
        !(byte.is_ascii_alphanumeric()
            || b"-._~!$&'()*+,;=%".contains(&byte)
            || (password && byte == b':'))
    }) {
        return Err(format!(
            "contains `{}`; percent-encode it",
            char::from(byte)
        ));
    }
    decode(raw)
}

/// Decode the database: one path segment whose escapes `decodeURI` decodes.
fn decode_database(raw: &str) -> Result<String, String> {
    if raw.contains('/') {
        return Err("must be one path segment".into());
    }
    if let Some(byte) = raw
        .bytes()
        .find(|&byte| !(byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@%".contains(&byte)))
    {
        return Err(format!(
            "contains `{}`; percent-encode it",
            char::from(byte)
        ));
    }
    let bytes = raw.as_bytes();
    for (index, _) in raw.match_indices('%') {
        let escaped = u8::from_str_radix(&raw[index + 1..index + 3], 16).unwrap_or(0);
        if UNDECODED_BY_DECODE_URI.contains(&escaped) {
            return Err(format!(
                "contains the escape {}, which celld does not support in a database name",
                std::str::from_utf8(&bytes[index..index + 3]).unwrap_or("%")
            ));
        }
    }
    let database = decode(raw)?;
    // The URL parser resolves `.` and `..` segments away, leaving no database.
    if database == "." || database == ".." {
        return Err("cannot be `.` or `..`".into());
    }
    Ok(database)
}

/// Percent-decode to UTF-8 without control characters. The caller has checked
/// that every `%` begins a two-digit escape.
fn decode(raw: &str) -> Result<String, String> {
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| "does not decode to UTF-8".to_string())?;
    if decoded.chars().any(char::is_control) {
        return Err("decodes to a control character".into());
    }
    Ok(decoded.into_owned())
}

/// Check the query against the allowlist and return the TLS mode it selects.
/// Pairs are decoded as `URLSearchParams` decodes them, which is how the
/// driver reads them.
fn query_tls(query: &str) -> Result<Tls, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut tls = Tls::Disabled;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode_form(key).map_err(|why| format!("a query key {why}"))?;
        if !QUERY_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "the query key {key:?} is not supported; the supported keys are {}",
                QUERY_KEYS.join(", ")
            ));
        }
        // `pg` keeps the last value of a repeated key, so
        // `sslmode=require&sslmode=disable` would mean plaintext.
        if !seen.insert(key.clone()) {
            return Err(format!("the query repeats the key {key:?}"));
        }
        if key == "sslmode" {
            let value = decode_form(value).map_err(|why| format!("the sslmode {why}"))?;
            tls = match value.as_str() {
                "disable" => Tls::Disabled,
                "require" | "verify-ca" | "verify-full" => Tls::Required,
                // celld always verifies, so it cannot keep `prefer`'s promise
                // of accepting plaintext, only turn it into another mode.
                "allow" | "prefer" | "no-verify" => {
                    return Err(format!(
                        "sslmode={value} is not supported; use sslmode=require, or \
                         sslmode=disable for a server without TLS"
                    ))
                }
                _ => {
                    return Err(
                        "the sslmode must be disable, require, verify-ca, or verify-full".into(),
                    )
                }
            };
        }
    }
    Ok(tls)
}

fn decode_form(raw: &str) -> Result<String, String> {
    decode(&raw.replace('+', " "))
}
