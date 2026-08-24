use sqlx::PgConnection;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PostgresMtlsSessionEvidence {
    Missing,
    Present {
        tls_active: bool,
        client_certificate_dn: Option<String>,
    },
}

pub(super) async fn validate_physical_connection(
    connection: &mut PgConnection,
) -> Result<(), sqlx::Error> {
    let evidence = sqlx::query_as::<_, (bool, Option<String>)>(
        "SELECT ssl, client_dn FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    )
    .fetch_optional(connection)
    .await?
    .map_or(
        PostgresMtlsSessionEvidence::Missing,
        |(tls_active, client_certificate_dn)| PostgresMtlsSessionEvidence::Present {
            tls_active,
            client_certificate_dn,
        },
    );
    validate_session_evidence(evidence)
}

pub(super) fn validate_session_evidence(
    evidence: PostgresMtlsSessionEvidence,
) -> Result<(), sqlx::Error> {
    match evidence {
        PostgresMtlsSessionEvidence::Present {
            tls_active: true,
            client_certificate_dn: Some(client_certificate_dn),
        } if !client_certificate_dn.trim().is_empty() => Ok(()),
        PostgresMtlsSessionEvidence::Missing | PostgresMtlsSessionEvidence::Present { .. } => {
            Err(sqlx::Error::InvalidArgument(
                "PostgreSQL physical connection did not provide required mTLS session evidence"
                    .to_string(),
            ))
        }
    }
}
