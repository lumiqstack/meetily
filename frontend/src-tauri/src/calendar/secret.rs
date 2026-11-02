//! The published feed URL is a bearer secret: anyone holding it can read the
//! calendar. It lives in the OS credential store, never in the settings file.

use anyhow::{anyhow, Context, Result};
use keyring::Entry;

const SERVICE: &str = "Meetily";
const FEED_URL_USER: &str = "outlook-ics-feed-url";

fn entry() -> Result<Entry> {
    Entry::new(SERVICE, FEED_URL_USER).context("open credential store entry")
}

/// `Ok(None)` when no URL has been stored.
pub fn load_feed_url() -> Result<Option<String>> {
    match entry()?.get_password() {
        Ok(url) => Ok(Some(url)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(anyhow!("read feed URL from credential store: {error}")),
    }
}

pub fn store_feed_url(url: &str) -> Result<()> {
    entry()?
        .set_password(url)
        .map_err(|error| anyhow!("write feed URL to credential store: {error}"))
}

pub fn clear_feed_url() -> Result<()> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow!("remove feed URL from credential store: {error}")),
    }
}

/// Accepts what Outlook hands out (an `https://` ICS link, sometimes shown as
/// `webcal://`) and rejects everything else before it reaches the keyring.
pub fn normalize_feed_url(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("The calendar link is empty"));
    }
    let candidate = match trimmed.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => trimmed.to_string(),
    };
    let parsed = url::Url::parse(&candidate).map_err(|_| anyhow!("That is not a valid link"))?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err(anyhow!("The calendar link must start with https://"));
    }
    if parsed.host_str().is_none() {
        return Err(anyhow!("The calendar link has no host"));
    }
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webcal_becomes_https() {
        assert_eq!(
            normalize_feed_url(" webcal://outlook.office365.com/owa/calendar/x/calendar.ics ").unwrap(),
            "https://outlook.office365.com/owa/calendar/x/calendar.ics"
        );
    }

    #[test]
    fn rejects_non_http_and_empty() {
        assert!(normalize_feed_url("").is_err());
        assert!(normalize_feed_url("file:///tmp/cal.ics").is_err());
        assert!(normalize_feed_url("not a url").is_err());
    }
}
