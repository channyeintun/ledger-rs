//! Configuration, read once from the environment at startup.
//!
//! Every knob has a defensible default, and every default is stated here rather
//! than scattered across call sites. A misconfigured timeout in a service that
//! holds row locks is an outage, so these are explicit and documented.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::db::PoolConfig;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub pool: PoolConfig,
    /// Ceiling on a single request. Must exceed `pool.acquire_timeout`, or a
    /// request queued for a connection is killed before it ever gets one.
    pub request_timeout: Duration,
    /// Ceiling on a request body. Transfers are a few hundred bytes.
    pub max_body_bytes: usize,
    /// How long to let in-flight requests finish after a shutdown signal.
    pub shutdown_grace: Duration,
    /// Whether `POST /accounts` may open an account exempt from the
    /// non-negative balance constraint.
    ///
    /// Defaults to **false**, because that flag is the one control separating
    /// an ordinary account from one that can mint money. Left to the caller, it
    /// makes "can create an account" equivalent to "can create money"; as an
    /// operator decision, a caller who reaches the endpoint still cannot mint.
    /// Funding accounts are rare and long-lived, so enabling this briefly at
    /// provisioning time — or seeding them out of band — costs little.
    pub allow_funding_account_creation: bool,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        let database_url = std::env::var("DATABASE_URL").context(
            "DATABASE_URL must be set (e.g. postgres://postgres:postgres@localhost/ledger)",
        )?;

        let bind_addr: SocketAddr = env_or("BIND_ADDR", "0.0.0.0:3000")?
            .parse()
            .context("BIND_ADDR must be a valid socket address")?;

        let pool = PoolConfig {
            max_connections: env_parse("DATABASE_MAX_CONNECTIONS", 16)?,
            min_connections: env_parse("DATABASE_MIN_CONNECTIONS", 1)?,
            acquire_timeout: env_secs("DATABASE_ACQUIRE_TIMEOUT_SECS", 10)?,
            max_lifetime: env_secs("DATABASE_MAX_LIFETIME_SECS", 30 * 60)?,
            idle_timeout: env_secs("DATABASE_IDLE_TIMEOUT_SECS", 10 * 60)?,
            statement_timeout: env_secs("DATABASE_STATEMENT_TIMEOUT_SECS", 30)?,
            lock_timeout: env_secs("DATABASE_LOCK_TIMEOUT_SECS", 5)?,
            idle_in_transaction_timeout: env_secs("DATABASE_IDLE_IN_TXN_TIMEOUT_SECS", 30)?,
        };

        let config = Config {
            database_url,
            bind_addr,
            pool,
            request_timeout: env_secs("REQUEST_TIMEOUT_SECS", 30)?,
            max_body_bytes: env_parse("MAX_BODY_BYTES", 64 * 1024)?,
            shutdown_grace: env_secs("SHUTDOWN_GRACE_SECS", 20)?,
            allow_funding_account_creation: env_parse("ALLOW_FUNDING_ACCOUNT_CREATION", false)?,
        };

        config.validate()?;
        Ok(config)
    }

    /// Rejects combinations that would misbehave under load rather than
    /// failing obviously. Each of these has a specific failure mode.
    fn validate(&self) -> anyhow::Result<()> {
        if self.pool.max_connections == 0 {
            bail!("DATABASE_MAX_CONNECTIONS must be greater than zero");
        }
        if self.pool.min_connections > self.pool.max_connections {
            bail!(
                "DATABASE_MIN_CONNECTIONS ({}) exceeds DATABASE_MAX_CONNECTIONS ({})",
                self.pool.min_connections,
                self.pool.max_connections
            );
        }
        if self.request_timeout <= self.pool.acquire_timeout {
            bail!(
                "REQUEST_TIMEOUT_SECS ({}s) must exceed DATABASE_ACQUIRE_TIMEOUT_SECS ({}s), \
                 otherwise a request waiting for a connection is cancelled before it can get one",
                self.request_timeout.as_secs(),
                self.pool.acquire_timeout.as_secs()
            );
        }
        // `statement_timeout` counts time spent waiting for a lock, so a
        // statement timeout at or below the lock timeout would kill contended
        // transfers before `lock_timeout` ever reported the real cause.
        if self.pool.statement_timeout <= self.pool.lock_timeout {
            bail!(
                "DATABASE_STATEMENT_TIMEOUT_SECS ({}s) must exceed DATABASE_LOCK_TIMEOUT_SECS ({}s)",
                self.pool.statement_timeout.as_secs(),
                self.pool.lock_timeout.as_secs()
            );
        }
        Ok(())
    }

    /// The connection string with any password replaced, for logging.
    pub fn redacted_database_url(&self) -> String {
        redact_url(&self.database_url)
    }
}

fn env_or(key: &str, default: &str) -> anyhow::Result<String> {
    Ok(std::env::var(key).unwrap_or_else(|_| default.to_string()))
}

fn env_parse<T>(key: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(raw) => raw
            .parse()
            .map_err(|err| anyhow::anyhow!("{key} is invalid: {err}")),
        Err(_) => Ok(default),
    }
}

fn env_secs(key: &str, default_secs: u64) -> anyhow::Result<Duration> {
    Ok(Duration::from_secs(env_parse(key, default_secs)?))
}

/// Replaces the password in a `scheme://user:password@host/...` URL.
///
/// Connection strings reach logs through error messages more often than anyone
/// intends, and a leaked database password is a leaked ledger — one that
/// bypasses every control in this service, including the append-only triggers.
///
/// Two parsing details are load-bearing, and getting either wrong leaks exactly
/// what this function exists to hide:
///
/// * The authority ends at the first `/`, `?` or `#`. Without that bound, an
///   `@` later in the path would be mistaken for the userinfo delimiter.
/// * Within the authority, userinfo ends at the **last** `@` (RFC 3986), not
///   the first. Splitting on the first would treat a password containing a
///   literal `@` as ending there and print the remainder verbatim.
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);

    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };

    match userinfo.split_once(':') {
        Some((user, _password)) => format!("{scheme}://{user}:****@{host}{tail}"),
        // Userinfo with no password: nothing to hide.
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config {
            database_url: "postgres://u:p@localhost/ledger".into(),
            bind_addr: "127.0.0.1:3000".parse().unwrap(),
            pool: PoolConfig::default(),
            request_timeout: Duration::from_secs(30),
            max_body_bytes: 64 * 1024,
            shutdown_grace: Duration::from_secs(20),
            allow_funding_account_creation: false,
        }
    }

    #[test]
    fn passwords_are_redacted_from_connection_strings() {
        assert_eq!(
            redact_url("postgres://ledger:hunter2@db.internal:5432/ledger?sslmode=require"),
            "postgres://ledger:****@db.internal:5432/ledger?sslmode=require"
        );
        // Nothing to redact, nothing mangled.
        assert_eq!(
            redact_url("postgres://localhost/ledger"),
            "postgres://localhost/ledger"
        );
        assert_eq!(
            redact_url("postgres://ledger@db.internal/ledger"),
            "postgres://ledger@db.internal/ledger"
        );
        assert_eq!(redact_url("not a url"), "not a url");
    }

    /// Regression: userinfo ends at the *last* `@`, not the first. Splitting on
    /// the first printed the tail of the password verbatim into the startup log.
    #[test]
    fn a_password_containing_an_at_sign_is_fully_redacted() {
        for (url, expected) in [
            (
                "postgres://ledger:p@ssw0rd@db.internal:5432/ledger",
                "postgres://ledger:****@db.internal:5432/ledger",
            ),
            (
                "postgres://ledger:a@b@c@db.internal:5432/ledger",
                "postgres://ledger:****@db.internal:5432/ledger",
            ),
            (
                "postgres://ledger:p@ss@db.internal/ledger?sslmode=require",
                "postgres://ledger:****@db.internal/ledger?sslmode=require",
            ),
        ] {
            let redacted = redact_url(url);
            assert_eq!(redacted, expected, "redacting {url}");
            for secret in ["p@ssw0rd", "ssw0rd", "a@b@c", "b@c", "p@ss", "ss@"] {
                assert!(
                    !redacted.contains(secret),
                    "{redacted:?} still leaks {secret:?}"
                );
            }
        }
    }

    /// An `@` after the authority belongs to the path and must not be mistaken
    /// for the userinfo delimiter.
    #[test]
    fn an_at_sign_in_the_path_does_not_confuse_the_parser() {
        assert_eq!(
            redact_url("postgres://ledger:secret@db.internal/ledger@shard1"),
            "postgres://ledger:****@db.internal/ledger@shard1"
        );
        // No credentials at all, but an `@` in the path.
        assert_eq!(
            redact_url("postgres://db.internal/ledger@shard1"),
            "postgres://db.internal/ledger@shard1"
        );
    }

    #[test]
    fn a_request_timeout_below_the_acquire_timeout_is_rejected() {
        let mut config = base();
        config.request_timeout = Duration::from_secs(5);
        config.pool.acquire_timeout = Duration::from_secs(10);
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_statement_timeout_below_the_lock_timeout_is_rejected() {
        let mut config = base();
        config.pool.statement_timeout = Duration::from_secs(2);
        config.pool.lock_timeout = Duration::from_secs(5);
        assert!(config.validate().is_err());
    }

    #[test]
    fn pool_bounds_must_be_coherent() {
        let mut config = base();
        config.pool.max_connections = 0;
        assert!(config.validate().is_err());

        let mut config = base();
        config.pool.min_connections = 20;
        config.pool.max_connections = 4;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_defaults_are_self_consistent() {
        base().validate().expect("shipped defaults must validate");
    }
}
