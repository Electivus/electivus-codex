use anyhow::anyhow;
use sqlx::ConnectOptions;
use sqlx::postgres::PgConnectOptions;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use url::ParseError;
use url::Url;

use super::config::PostgresConnectionSource;

/// A validated passwordless PostgreSQL mTLS connection description.
///
/// The descriptor keeps connection material behind a redacting debug
/// representation so callers can pass one value from source resolution to
/// physical pool creation without logging its contents.
#[derive(Clone)]
pub(super) struct PostgresMtlsConnectionDescriptor {
    connect_options: PgConnectOptions,
}

impl PostgresMtlsConnectionDescriptor {
    pub(super) fn parse(value: &str, source: &PostgresConnectionSource) -> anyhow::Result<Self> {
        let mut url = Url::parse(value).map_err(|error| match error {
            ParseError::EmptyHost => {
                invalid_descriptor(source, "include a non-empty PostgreSQL host")
            }
            _ => invalid_descriptor(source, "contain a valid URL"),
        })?;
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            return Err(invalid_descriptor(
                source,
                "use the `postgres` or `postgresql` scheme",
            ));
        }
        if url.username().is_empty() {
            return Err(invalid_descriptor(
                source,
                "include a non-empty PostgreSQL user",
            ));
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err(invalid_descriptor(
                source,
                "include a non-empty PostgreSQL host",
            ));
        }
        if url.path().trim_start_matches('/').is_empty() {
            return Err(invalid_descriptor(
                source,
                "include a non-empty PostgreSQL database",
            ));
        }
        if url.password().is_some() {
            return Err(invalid_descriptor(source, "must not contain a password"));
        }
        if url.fragment().is_some() {
            return Err(invalid_descriptor(source, "must not contain a fragment"));
        }
        let tls_parameters = validate_tls_parameters(&url, source)?;
        validate_tls_file("sslrootcert", &tls_parameters.root_certificate, source)?;
        validate_tls_file("sslcert", &tls_parameters.client_certificate, source)?;
        let key_metadata = validate_tls_file("sslkey", &tls_parameters.client_key, source)?;
        validate_client_key_permissions(&key_metadata, source)?;

        // An explicit empty password prevents SQLx from consulting PGPASSWORD
        // or a password file after this passwordless descriptor is validated.
        url.set_password(Some(""))
            .map_err(|()| invalid_descriptor(source, "contain a valid PostgreSQL authority"))?;
        let connect_options = PgConnectOptions::from_url(&url)
            .map_err(|_| invalid_descriptor(source, "contain valid connection fields"))?
            .port(url.port().unwrap_or(5432));
        Ok(Self { connect_options })
    }

    pub(super) fn connect_options(&self) -> PgConnectOptions {
        self.connect_options.clone()
    }
}

struct TlsParameters {
    root_certificate: PathBuf,
    client_certificate: PathBuf,
    client_key: PathBuf,
}

fn validate_tls_parameters(
    url: &Url,
    source: &PostgresConnectionSource,
) -> anyhow::Result<TlsParameters> {
    let Some(query) = url.query() else {
        return Err(invalid_descriptor(
            source,
            "include exactly one each of `sslmode`, `sslrootcert`, `sslcert`, and `sslkey`",
        ));
    };
    if query.split('&').any(|pair| {
        !matches!(
            pair.split_once('=').map_or(pair, |(key, _)| key),
            "sslmode" | "sslrootcert" | "sslcert" | "sslkey"
        )
    }) {
        return Err(invalid_descriptor(
            source,
            "contain only the canonical TLS parameters `sslmode`, `sslrootcert`, `sslcert`, and `sslkey`",
        ));
    }

    let mut sslmode = None;
    let mut sslrootcert = None;
    let mut sslcert = None;
    let mut sslkey = None;
    for (key, value) in url.query_pairs() {
        let target = match key.as_ref() {
            "sslmode" => &mut sslmode,
            "sslrootcert" => &mut sslrootcert,
            "sslcert" => &mut sslcert,
            "sslkey" => &mut sslkey,
            _ => {
                return Err(invalid_descriptor(
                    source,
                    "contain only the canonical TLS parameters `sslmode`, `sslrootcert`, `sslcert`, and `sslkey`",
                ));
            }
        };
        if target.replace(value.into_owned()).is_some() {
            return Err(invalid_descriptor(
                source,
                "include exactly one each of `sslmode`, `sslrootcert`, `sslcert`, and `sslkey`",
            ));
        }
    }
    let (Some(sslmode), Some(sslrootcert), Some(sslcert), Some(sslkey)) =
        (sslmode, sslrootcert, sslcert, sslkey)
    else {
        return Err(invalid_descriptor(
            source,
            "include exactly one each of `sslmode`, `sslrootcert`, `sslcert`, and `sslkey`",
        ));
    };
    if sslmode != "verify-full" {
        return Err(invalid_descriptor(source, "set `sslmode` to `verify-full`"));
    }
    let root_certificate = absolute_tls_path("sslrootcert", sslrootcert, source)?;
    let client_certificate = absolute_tls_path("sslcert", sslcert, source)?;
    let client_key = absolute_tls_path("sslkey", sslkey, source)?;
    Ok(TlsParameters {
        root_certificate,
        client_certificate,
        client_key,
    })
}

fn absolute_tls_path(
    parameter: &str,
    value: String,
    source: &PostgresConnectionSource,
) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(invalid_descriptor(
            source,
            &format!("set `{parameter}` to a non-empty absolute path"),
        ));
    }
    Ok(path)
}

fn validate_tls_file(
    parameter: &str,
    path: &Path,
    source: &PostgresConnectionSource,
) -> anyhow::Result<std::fs::Metadata> {
    if !std::fs::metadata(path)
        .map_err(|_| invalid_tls_file(source, parameter))?
        .is_file()
    {
        return Err(invalid_tls_file(source, parameter));
    }
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;

        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path);
    let file = file.map_err(|_| invalid_tls_file(source, parameter))?;
    let metadata = file
        .metadata()
        .map_err(|_| invalid_tls_file(source, parameter))?;
    if !metadata.is_file() {
        return Err(invalid_tls_file(source, parameter));
    }
    Ok(metadata)
}

fn invalid_tls_file(source: &PostgresConnectionSource, parameter: &str) -> anyhow::Error {
    invalid_descriptor(
        source,
        &format!("the file configured by `{parameter}` must be a readable regular file"),
    )
}

#[cfg(unix)]
fn validate_client_key_permissions(
    metadata: &std::fs::Metadata,
    source: &PostgresConnectionSource,
) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(invalid_descriptor(
            source,
            "use a private client key; the client key must not grant any group or other permissions",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_client_key_permissions(
    _metadata: &std::fs::Metadata,
    _source: &PostgresConnectionSource,
) -> anyhow::Result<()> {
    Ok(())
}

fn invalid_descriptor(source: &PostgresConnectionSource, reason: &str) -> anyhow::Error {
    match source {
        PostgresConnectionSource::Direct { .. } => anyhow!(
            "Direct PostgreSQL URL in `state.postgresql.url` does not contain a valid passwordless mTLS Connection Descriptor: it must satisfy this requirement: {reason}"
        ),
        PostgresConnectionSource::Environment { url_env } => anyhow!(
            "PostgreSQL URL environment variable `{url_env}` does not contain a valid passwordless mTLS Connection Descriptor: it must satisfy this requirement: {reason}"
        ),
    }
}

impl fmt::Debug for PostgresMtlsConnectionDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PostgresMtlsConnectionDescriptor([REDACTED])")
    }
}
