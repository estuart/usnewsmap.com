//! Error text for a public page (09 §9.4): pipeline errors can name storage
//! accounts, Cosmos endpoints, registry hosts, addresses, identities and
//! worker hostnames. Those are replaced with a placeholder; LoC URLs and batch
//! names are public data and stay.

/// Longest error shown, in characters.
pub const MAX_CHARS: usize = 200;

/// Host suffixes that are never shown (Azure services and sign-in).
const PRIVATE_SUFFIXES: [&str; 9] = [
    ".windows.net",
    ".azure.com",
    ".azure.net",
    ".azure.us",
    ".azurecr.io",
    ".azurecontainerapps.io",
    ".azurewebsites.net",
    ".microsoftonline.com",
    ".internal",
];

/// Name prefixes of this deployment's Azure resources in their legacy form
/// (08 §8.1), which some service errors quote without a hostname. Names in
/// ADR-0017's form are recognized by [`is_adr_0017_name`].
const RESOURCE_PREFIXES: [&str; 18] = [
    "cosmos-usnm",
    "stusnm",
    "crusnm",
    "caj-usnm",
    "ca-usnm",
    "cae-usnm",
    "id-usnm",
    "pe-usnm",
    "appi-usnm",
    "log-usnm",
    "rg-usnm",
    "vnet-usnm",
    "snet-usnm",
    "di-usnm",
    "ag-usnm",
    "dcr-usnm",
    "alert-usnm",
    "webtest-usnm",
];

/// ADR-0017's type abbreviations for resources (infra/naming.bicep,
/// `typeAbbreviations`, less the `display: true` ones): the token before the
/// environment in `usnm-{desc}-{abbr}-{env}[-{suffix}]`. scripts/ci/check-names.py
/// fails when this list, or [`COMPACT_ABBREVIATIONS`], differs from naming.bicep.
const TYPE_ABBREVIATIONS: [&str; 20] = [
    "ag", "alert", "appi", "budget", "ca", "caj", "cae", "cosno", "cr", "dcr", "di", "id", "kv",
    "log", "nic", "pep", "rg", "st", "vnet", "webtest",
];

/// The abbreviations of the types whose names are compact,
/// `usnm{fn}{abbr}{env}{suffix}` (letters and digits only): naming.bicep's
/// `form: 'compact'` ones.
const COMPACT_ABBREVIATIONS: [&str; 3] = ["st", "cr", "kv"];

/// `error` with private details replaced and cut to [`MAX_CHARS`].
pub fn sanitize(error: &str) -> String {
    let mut out = String::with_capacity(error.len().min(MAX_CHARS * 2));
    let mut token = String::new();
    for c in error.chars() {
        if is_token_char(c) {
            token.push(c);
        } else {
            flush(&mut token, &mut out);
            // Control characters (newlines in multi-line errors) become spaces.
            out.push(if c.is_control() { ' ' } else { c });
        }
    }
    flush(&mut token, &mut out);
    truncate(out.trim())
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#@!$&'*+,;=%".contains(c)
}

fn flush(token: &mut String, out: &mut String) {
    if !token.is_empty() {
        // Sentence punctuation after a token isn't part of it.
        let trimmed = token.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'']);
        let tail = &token[trimmed.len()..];
        out.push_str(&redact(trimmed));
        out.push_str(tail);
        token.clear();
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_CHARS {
        return s.to_owned();
    }
    let mut cut: String = s.chars().take(MAX_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// One whitespace/punctuation-delimited token, or its replacement.
fn redact(token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }
    if let Some((scheme, rest)) = token.split_once("://") {
        if scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            && !scheme.is_empty()
        {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            // Credentials in the authority are never shown, even for LoC.
            let host = if authority.contains('@') {
                ""
            } else {
                authority
            };
            let host = strip_port(host).to_ascii_lowercase();
            let public =
                scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http");
            return if public && is_loc(&host) {
                token.to_owned()
            } else {
                "[url]".to_owned()
            };
        }
    }
    // Paths, key=value pairs and lists put identifiers between these, so
    // each component is checked (and replaced) on its own.
    let mut out = String::with_capacity(token.len());
    let mut component = String::new();
    for c in token.chars() {
        if COMPONENT_DELIMITERS.contains(&c) {
            out.push_str(redact_component(&component));
            component.clear();
            out.push(c);
        } else {
            component.push(c);
        }
    }
    out.push_str(redact_component(&component));
    out
}

/// Separators inside a token between which identifiers appear.
const COMPONENT_DELIMITERS: [char; 7] = ['/', '=', '&', ',', ';', '?', '#'];

/// One component of a token, or its placeholder.
fn redact_component(c: &str) -> &str {
    if c.is_empty() {
        return c;
    }
    let lower = c.to_ascii_lowercase();
    // `host:port`, `user@host`.
    if lower
        .split('@')
        .any(|part| is_private_host(strip_port(part)))
    {
        return "[host]";
    }
    // Worker ids (host-pid-nanos) name the replica.
    if has_worker_id(c) {
        return "[worker]";
    }
    let pieces = || lower.split(':');
    if pieces().any(|p| RESOURCE_PREFIXES.iter().any(|r| p.starts_with(r)) || is_adr_0017_name(p)) {
        return "[resource]";
    }
    if c.contains('@') && c.rsplit('@').next().is_some_and(|d| d.contains('.')) {
        return "[email]";
    }
    if is_ipv4(strip_port(c)) || is_ipv6(c) {
        return "[ip]";
    }
    if pieces().any(is_guid) {
        return "[id]";
    }
    c
}

/// An environment name (ADR-0017): prod, dev, or x and 1-4 lowercase
/// letters or digits.
fn is_env(s: &str) -> bool {
    s == "prod"
        || s == "dev"
        || (s.len() >= 2
            && s.len() <= 5
            && s.starts_with('x')
            && s[1..]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// A resource name in ADR-0017's form (lower case): hyphenated
/// `usnm-{desc}-{abbr}-{env}`, with anything after it (a global name's
/// suffix, a job execution's id), or compact `usnm{fn}{abbr}{env}{suffix}`.
/// `usnm` alone (the Cosmos database), crate and binary names such as
/// `usnm-ingest`, and the OCR nodes' `usnmocr` group don't match.
fn is_adr_0017_name(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix("usnm-") {
        let tokens: Vec<&str> = rest.split('-').collect();
        // At least one description token before the abbreviation.
        return (1..tokens.len().saturating_sub(1))
            .any(|i| TYPE_ABBREVIATIONS.contains(&tokens[i]) && is_env(tokens[i + 1]));
    }
    let Some(rest) = s.strip_prefix("usnm") else {
        return false;
    };
    if !rest
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return false;
    }
    // {fn}{abbr}{env}{suffix}: a function word of letters, then the
    // abbreviation, the environment and the 6-character deployment suffix.
    (1..rest.len()).any(|i| {
        let (word, tail) = rest.split_at(i);
        word.bytes().all(|b| b.is_ascii_lowercase())
            && COMPACT_ABBREVIATIONS.iter().any(|a| {
                tail.strip_prefix(a)
                    .is_some_and(|t| t.len() > 6 && is_env(&t[..t.len() - 6]))
            })
    })
}

fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    }
}

fn is_loc(host: &str) -> bool {
    host == "loc.gov" || host.ends_with(".loc.gov")
}

fn is_private_host(part: &str) -> bool {
    PRIVATE_SUFFIXES.iter().any(|s| part.ends_with(s))
}

fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u8>().is_ok())
}

/// Hex groups and colons with a `::` or a hex letter (so times like 10:00:00
/// don't match), with or without a zone (`fe80::1%eth0`).
fn is_ipv6(s: &str) -> bool {
    let s = s.split_once('%').map_or(s, |(addr, _)| addr);
    let colons = s.matches(':').count();
    colons >= 2
        && s.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
        && (s.contains("::") || s.chars().any(|c| c.is_ascii_alphabetic()) || colons >= 5)
        && s.split(':').all(|g| g.len() <= 4)
}

fn is_guid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Contains `…-{pid}-{8 hex}`, the tail of [`usnm_ingest::owner_id`]-style ids.
fn has_worker_id(segment: &str) -> bool {
    let parts: Vec<&str> = segment.split('-').collect();
    parts.windows(2).any(|w| {
        !w[0].is_empty()
            && w[0].bytes().all(|b| b.is_ascii_digit())
            && w[1].len() == 8
            && w[1].bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }) && parts.len() >= 3
}

/// A short, opaque worker id for display: the owner id's last six characters.
pub fn short_id(owner: &str) -> String {
    let chars: Vec<char> = owner.chars().collect();
    chars[chars.len().saturating_sub(6)..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_loc_urls_and_batch_names() {
        let e = "GET https://chroniclingamerica.loc.gov/data/batches/batch_az_acacia_ver01.tar.bz2 returned 503";
        assert_eq!(sanitize(e), e);
        let e =
            "archive sha256 abc does not match for batch_dlc_elf_ver02 (https://tile.loc.gov/x)";
        assert_eq!(sanitize(e), e);
        assert_eq!(
            sanitize("see https://www.loc.gov/."),
            "see https://www.loc.gov/."
        );
    }

    #[test]
    fn replaces_other_urls() {
        assert_eq!(
            sanitize("error sending request for url (https://cosmos-usnm-prod-x1.documents.azure.com/dbs/usnm/colls/batches/docs)"),
            "error sending request for url ([url])"
        );
        assert_eq!(
            sanitize("GET https://evil.example/loc.gov failed"),
            "GET [url] failed"
        );
        assert_eq!(sanitize("http://127.0.0.1:7280/api/v1"), "[url]");
        // Credentials are never shown, even in a LoC URL.
        assert_eq!(sanitize("https://alice:secret@loc.gov/file"), "[url]");
        assert_eq!(sanitize("azure://qw-index/x"), "[url]");
        assert_eq!(
            sanitize("git+ssh://h/x svn.ssh://h/y x-y://z"),
            "[url] [url] [url]"
        );
        assert_eq!(
            sanitize("https://user@chroniclingamerica.loc.gov.evil.io/"),
            "[url]"
        );
    }

    #[test]
    fn replaces_azure_hosts_without_a_scheme() {
        assert_eq!(
            sanitize("blob stusnmdataabc.blob.core.windows.net/curated/x: 403"),
            "blob [host]/curated/x: 403"
        );
        assert_eq!(
            sanitize("pull crusnmprod.azurecr.io/usnewsmap-ingest"),
            "pull [host]/usnewsmap-ingest"
        );
        assert_eq!(
            sanitize("host cosmos-usnm.documents.azure.com:443"),
            "host [host]"
        );
        assert_eq!(
            sanitize("token from login.microsoftonline.com refused"),
            "token from [host] refused"
        );
    }

    /// A private (RFC 1918) address, built from its octets: none is written
    /// out in the repository (scripts/ci/no-private-addresses.sh).
    fn private_ip() -> String {
        let ip = std::net::Ipv4Addr::new(10, 0, 2, 4);
        assert!(ip.is_private());
        ip.to_string()
    }

    #[test]
    fn checks_each_component_of_key_value_pairs_and_lists() {
        assert_eq!(
            sanitize("account=stusnmdataxyz resource=cosmos-usnm-prod"),
            "account=[resource] resource=[resource]"
        );
        assert_eq!(
            sanitize(&format!(
                "endpoint={}:443&client_id=3fa85f64-5717-4562-b3fc-2c963f66afa6",
                private_ip()
            )),
            "endpoint=[ip]&client_id=[id]"
        );
        assert_eq!(
            sanitize("host=cosmos.documents.azure.com;owner=ops@example.com,principal:3FA85F64-5717-4562-B3FC-2C963F66AFA6"),
            "host=[host];owner=[email],[id]"
        );
        assert_eq!(
            sanitize("batch=batch_az_acacia_ver01"),
            "batch=batch_az_acacia_ver01"
        );
    }

    #[test]
    fn replaces_resource_names() {
        assert_eq!(
            sanitize("Request blocked by Auth cosmos-usnm-prod-ab12 : principal lacks RBAC"),
            "Request blocked by Auth [resource] : principal lacks RBAC"
        );
        assert_eq!(
            sanitize("account stusnmdataxyz: 403"),
            "account [resource]: 403"
        );
    }

    #[test]
    fn replaces_adr_0017_names() {
        // Hyphenated, global (with the suffix) and a job execution.
        assert_eq!(
            sanitize("job usnm-ingest-caj-dev failed; usnm-state-cosno-dev-abc123 refused"),
            "job [resource] failed; [resource] refused"
        );
        assert_eq!(
            sanitize("execution usnm-backfill-caj-x1-ab12cd3: Failed"),
            "execution [resource]: Failed"
        );
        assert_eq!(
            sanitize("scope=/resourceGroups/usnm-core-rg-xabcd/x"),
            "scope=/resourceGroups/[resource]/x"
        );
        // Compact: storage, registry and Key Vault.
        assert_eq!(
            sanitize("account usnmdatastdevabc123: 403, usnmcorecrprodzz9y8x, usnmocrkvx1abc123"),
            "account [resource]: 403, [resource], [resource]"
        );
        // Legacy names the list lacked.
        assert_eq!(
            sanitize("di-usnm-prod-abc123 and ag-usnm-prod"),
            "[resource] and [resource]"
        );
        // Not resources: the database, binaries, the nodes' group, the image.
        let e = "usnm usnm-ingest usnm-qwcluster usnm-ocr-worker usnmocr usnewsmap-ingest dbs/usnm/colls";
        assert_eq!(sanitize(e), e);
    }

    #[test]
    fn replaces_addresses_ids_and_emails() {
        assert_eq!(
            sanitize(&format!("connect {}:443 refused", private_ip())),
            "connect [ip] refused"
        );
        assert_eq!(
            sanitize("connect 192.0.2.4:443 refused"),
            "connect [ip] refused"
        );
        assert_eq!(sanitize("from [fe80::1]"), "from [[ip]]");
        assert_eq!(
            sanitize("via fe80::1%eth0 and fe80::1%25en0"),
            "via [ip] and [ip]"
        );
        assert_eq!(
            sanitize("at 2026-09-29T10:00:00Z, 10:00:00"),
            "at 2026-09-29T10:00:00Z, 10:00:00"
        );
        assert_eq!(
            sanitize("principal 3fa85f64-5717-4562-b3fc-2c963f66afa6 lacks a role"),
            "principal [id] lacks a role"
        );
        assert_eq!(sanitize("owner ops@example.com"), "owner [email]");
        // Version numbers aren't addresses.
        assert_eq!(sanitize("quickwit v0.9.1"), "quickwit v0.9.1");
    }

    #[test]
    fn replaces_worker_ids() {
        assert_eq!(
            sanitize("lock `quickwit-writer` is held by `caj-usnm-ingest-prod-x7k2p-14-0a1b2c3d`"),
            "lock `quickwit-writer` is held by `[worker]`"
        );
        assert_eq!(
            sanitize("`pages/batch_x/v01/20260915T101010Z-caj-usnm-backfill-q-7-00ab12cd-a2/part-0000.parquet` already exists"),
            "`pages/batch_x/v01/[worker]/part-0000.parquet` already exists"
        );
        // Batch names and dates are left alone.
        assert_eq!(
            sanitize("batch_dlc_1-2_ver01 1896-07-10"),
            "batch_dlc_1-2_ver01 1896-07-10"
        );
    }

    #[test]
    fn flattens_lines_and_caps_length() {
        assert_eq!(sanitize("a\nb\tc"), "a b c");
        let long = "x ".repeat(300);
        let s = sanitize(&long);
        assert_eq!(s.chars().count(), MAX_CHARS);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn short_ids_are_the_last_six_characters() {
        assert_eq!(short_id("caj-usnm-backfill-q-7-00ab12cd"), "ab12cd");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
    }
}
