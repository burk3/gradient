/*
 * SPDX-FileCopyrightText: 2026 Wavelens GmbH <info@wavelens.io>
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

use anyhow::{Context, Result};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::collections::BTreeMap;

const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
const PATH: &AsciiSet = &UNRESERVED.remove(b'/');

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedTarball {
    pub url: String,
    pub rev: String,
    pub rev_count: Option<u64>,
    pub last_modified: Option<i64>,
}

pub fn immutable_link(value: &str) -> Option<&str> {
    let rest = value.trim().strip_prefix('<')?;
    let (url, params) = rest.split_once('>')?;
    params
        .eq_ignore_ascii_case("; rel=\"immutable\"")
        .then_some(url)
}

impl PinnedTarball {
    pub fn parse(link: &str) -> Result<Self> {
        let parsed =
            url::Url::parse(link).with_context(|| format!("parsing immutable link `{link}`"))?;

        let mut rev = None;
        let mut rev_count = None;
        let mut last_modified = None;
        let mut kept = BTreeMap::new();
        for pair in parsed.query().unwrap_or_default().split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let (key, value) = (decode(key)?, decode(value)?);
            match key.as_str() {
                "rev" => rev = Some(value),
                "revCount" => {
                    rev_count = Some(value.parse().with_context(|| format!("revCount `{value}`"))?);
                }
                "lastModified" => {
                    last_modified =
                        Some(value.parse().with_context(|| format!("lastModified `{value}`"))?);
                }
                "narHash" => {}
                _ => {
                    kept.insert(key, value);
                }
            }
        }

        let path = utf8_percent_encode(&decode(parsed.path())?, PATH).to_string();
        let mut url = format!("{}{path}", &parsed[..url::Position::BeforePath]);
        if !kept.is_empty() {
            let query: Vec<String> = kept
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}={}",
                        utf8_percent_encode(k, UNRESERVED),
                        utf8_percent_encode(v, UNRESERVED)
                    )
                })
                .collect();
            url.push('?');
            url.push_str(&query.join("&"));
        }

        Ok(Self {
            url,
            rev: rev.with_context(|| format!("immutable link `{link}` carries no `rev`"))?,
            rev_count,
            last_modified,
        })
    }
}

fn decode(s: &str) -> Result<String> {
    Ok(percent_decode_str(s)
        .decode_utf8()
        .context("decoding percent-encoded URL part")?
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAKEHUB: &str = "https://api.flakehub.com/f/pinned/NixOS/nixpkgs/0.2605.1015218+rev-78e9c786dc08cd4f3420c2395cd977206a9b1da2/01a0f5fe-57e5-7340-8937-4fd0d4bb0220/source.tar.gz?rev=78e9c786dc08cd4f3420c2395cd977206a9b1da2&revCount=1015218";
    const CHANNEL: &str = "https://releases.nixos.org/nixos/unstable/nixos-26.11pre1083223.c59305bab206/nixexprs.tar.xz?rev=c59305bab2065cfecc4944690d9eedbb56f3a9fa&lastModified=1790822859";

    #[test]
    fn immutable_link_matches_the_whole_header_value() {
        assert_eq!(
            immutable_link(&format!("<{FLAKEHUB}>; rel=\"immutable\"")),
            Some(FLAKEHUB)
        );
        assert_eq!(
            immutable_link("<https://h/x>; REL=\"Immutable\""),
            Some("https://h/x")
        );
        assert_eq!(immutable_link("<https://h/x>; rel=\"preload\""), None);
        assert_eq!(immutable_link("<https://h/x>; rel=\"immutable\"; as=x"), None);
        assert_eq!(immutable_link("https://h/x"), None);
    }

    #[test]
    fn flakehub_link_pins_url_rev_and_rev_count() {
        let pinned = PinnedTarball::parse(FLAKEHUB).unwrap();
        assert_eq!(
            pinned,
            PinnedTarball {
                url: "https://api.flakehub.com/f/pinned/NixOS/nixpkgs/0.2605.1015218%2Brev-78e9c786dc08cd4f3420c2395cd977206a9b1da2/01a0f5fe-57e5-7340-8937-4fd0d4bb0220/source.tar.gz".into(),
                rev: "78e9c786dc08cd4f3420c2395cd977206a9b1da2".into(),
                rev_count: Some(1015218),
                last_modified: None,
            }
        );
    }

    #[test]
    fn channel_link_pins_url_rev_and_last_modified() {
        let pinned = PinnedTarball::parse(CHANNEL).unwrap();
        assert_eq!(
            pinned,
            PinnedTarball {
                url: "https://releases.nixos.org/nixos/unstable/nixos-26.11pre1083223.c59305bab206/nixexprs.tar.xz".into(),
                rev: "c59305bab2065cfecc4944690d9eedbb56f3a9fa".into(),
                rev_count: None,
                last_modified: Some(1790822859),
            }
        );
    }

    #[test]
    fn already_encoded_path_is_not_encoded_twice() {
        let pinned = PinnedTarball::parse("https://h/0.1%2Brev-x/s.tar.gz?rev=x").unwrap();
        assert_eq!(pinned.url, "https://h/0.1%2Brev-x/s.tar.gz");
    }

    #[test]
    fn unknown_query_params_are_kept_sorted() {
        let pinned =
            PinnedTarball::parse("https://h/x.tar.gz?z=1&rev=abc&narHash=sha256-x&a=b%20c").unwrap();
        assert_eq!(pinned.url, "https://h/x.tar.gz?a=b%20c&z=1");
    }

    #[test]
    fn link_without_rev_is_rejected() {
        let err = PinnedTarball::parse("https://h/x.tar.gz?revCount=3").unwrap_err();
        assert!(format!("{err:#}").contains("rev"), "unexpected error: {err:#}");
    }
}
