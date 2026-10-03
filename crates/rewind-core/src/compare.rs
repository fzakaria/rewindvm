//! Comparing the outputs of a Nix run with other builds of the same store
//! paths: the copy in this machine's store, if it has one, and the builds
//! the binary caches Nix substitutes from publish.
//!
//! The guest reports each output's NAR hash, the hash Nix records as a
//! path's narHash and a cache publishes as NarHash in the path's
//! `.narinfo`. Only that small file is fetched, never the NAR it points to.
//! A store path names a build's inputs, not its contents, so another build
//! of the same path that differs in bytes does not mean the run went wrong:
//! the package may not be bit-reproducible.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use nix_derivation::{NixHash, StoreDir};
use nix_narinfo::NarInfo;

/// How long one cache may take to answer for one path.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How many lookups run at once, so a long list of caches and outputs does
/// not open dozens of connections.
const LOOKUP_THREADS: usize = 16;

/// The schemes of the caches Rewind asks: binary caches served over HTTP.
const HTTP_SCHEMES: [&str; 2] = ["http://", "https://"];

/// How many characters of a store path's file name are its hash, which
/// names its `.narinfo` in a cache.
const STORE_HASH_LEN: usize = 32;

/// Where another build of an output was looked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// This machine's Nix store.
    Store,
    /// A binary cache, by its URL.
    Cache(String),
}

/// What a source said about one output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Its build has the same NAR hash as the run's output.
    Matches,
    /// Its build of the same path has other contents.
    Differs,
    /// It has no build of the path.
    Absent,
    /// The cache answers only with credentials, and none it accepted were
    /// found in the netrc file.
    NeedsCredentials,
    /// The cache could not be asked, and why.
    Unreachable(String),
}

/// One source's verdict on one output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    pub source: Source,
    pub verdict: Verdict,
}

/// Every verdict on one output, the store's first if it has a copy, then
/// each cache's in the order they were configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputComparisons {
    pub path: String,
    pub comparisons: Vec<Comparison>,
}

/// Which caches to ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The substituters Nix is configured with, and these besides.
    Configured { extra: Vec<String> },
    /// None: only this machine's store is compared with.
    Off,
}

/// The caches to ask, and the substituters skipped because they are not
/// HTTP binary caches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caches {
    pub http: Vec<String>,
    pub skipped: Vec<String>,
}

/// The caches `lookup` names, reading Nix's configuration when it needs it.
pub fn caches(lookup: &Lookup) -> Result<Caches> {
    let Lookup::Configured { extra } = lookup else {
        return Ok(Caches::default());
    };
    let configured = nix_setting("substituters")?;
    Ok(split_substituters(&configured, extra))
}

/// The HTTP caches among a space-separated list of substituters, then
/// `extra`, without queries such as `?priority=100` or a trailing slash,
/// each once; and the substituters that are not HTTP caches.
fn split_substituters(list: &str, extra: &[String]) -> Caches {
    let mut caches = Caches::default();
    for url in list
        .split_whitespace()
        .chain(extra.iter().map(String::as_str))
    {
        let base = url.split('?').next().unwrap_or(url).trim_end_matches('/');
        if !HTTP_SCHEMES.iter().any(|scheme| base.starts_with(scheme)) {
            caches.skipped.push(base.to_string());
            continue;
        }
        if !caches.http.iter().any(|c| c == base) {
            caches.http.push(base.to_string());
        }
    }
    caches
}

/// A login from a netrc file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Login {
    pub login: String,
    pub password: String,
}

/// The logins in a netrc file, by host, and the default for any other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Netrc {
    machines: BTreeMap<String, Login>,
    default: Option<Login>,
}

impl Netrc {
    /// The netrc file Nix is configured to read, or no logins when there is
    /// none or it cannot be read, as for a user it is not readable by.
    pub fn load() -> Netrc {
        let Ok(path) = nix_setting("netrc-file") else {
            return Netrc::default();
        };
        let path = PathBuf::from(path.trim());
        std::fs::read_to_string(path)
            .map(|text| Netrc::parse(&text))
            .unwrap_or_default()
    }

    /// Parses netrc text: `machine`, `default`, `login`, `password` and
    /// `account` tokens, with a `macdef` skipped to its blank line.
    pub fn parse(text: &str) -> Netrc {
        let mut netrc = Netrc::default();
        let mut entry: Option<(Option<String>, Login)> = None;
        let finish =
            |entry: &mut Option<(Option<String>, Login)>, netrc: &mut Netrc| match entry.take() {
                Some((Some(host), login)) => {
                    netrc.machines.insert(host, login);
                }
                Some((None, login)) => netrc.default = Some(login),
                None => {}
            };
        let empty = || Login {
            login: String::new(),
            password: String::new(),
        };

        // A macdef runs to the next blank line, so lines are walked here
        // and tokens within them.
        let mut in_macdef = false;
        for line in text.lines() {
            if in_macdef {
                in_macdef = !line.trim().is_empty();
                continue;
            }
            let mut tokens = line.split_whitespace();
            while let Some(token) = tokens.next() {
                match token {
                    "machine" => {
                        finish(&mut entry, &mut netrc);
                        let host = tokens.next().unwrap_or_default().to_string();
                        entry = Some((Some(host), empty()));
                    }
                    "default" => {
                        finish(&mut entry, &mut netrc);
                        entry = Some((None, empty()));
                    }
                    "login" | "password" | "account" => {
                        let value = tokens.next().unwrap_or_default().to_string();
                        let Some((_, login)) = entry.as_mut() else {
                            continue;
                        };
                        match token {
                            "login" => login.login = value,
                            "password" => login.password = value,
                            _ => {}
                        }
                    }
                    "macdef" => {
                        in_macdef = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        finish(&mut entry, &mut netrc);
        netrc
    }

    /// The login for `host`: its own entry, else the default.
    pub fn for_host(&self, host: &str) -> Option<&Login> {
        self.machines.get(host).or(self.default.as_ref())
    }
}

/// A Nix setting as `nix config show` prints it, falling back to the name
/// that command had before Nix 2.20.
fn nix_setting(name: &str) -> Result<String> {
    for args in [vec!["config", "show", name], vec!["show-config", name]] {
        let out = Command::new("nix")
            .args(["--extra-experimental-features", "nix-command"])
            .args(&args)
            .output()
            .context("running nix; is it on PATH?")?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
    }
    anyhow::bail!("nix config show {name} failed")
}

/// A hash as the guest writes one, `sha256:` and hex, or in any form Nix
/// writes.
fn parse_hash(hash: &str) -> Option<NixHash> {
    NixHash::parse(hash).ok()
}

/// Compares each output, a store path and the NAR hash the guest reported
/// for it, with this machine's copy and with every cache in `caches`.
/// Lookups run several at a time, and each that fails says why in its
/// verdict, so this does not fail.
pub fn compare(
    outputs: &[(String, String)],
    caches: &Caches,
    netrc: &Netrc,
) -> Vec<OutputComparisons> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(LOOKUP_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();

    // Every (output, cache) pair, asked a batch at a time.
    let pairs: Vec<(usize, &String)> = (0..outputs.len())
        .flat_map(|i| caches.http.iter().map(move |cache| (i, cache)))
        .collect();
    let mut answers: Vec<Verdict> = Vec::with_capacity(pairs.len());
    for batch in pairs.chunks(LOOKUP_THREADS) {
        let verdicts: Vec<Verdict> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|&(i, cache)| {
                    let (path, hash) = &outputs[i];
                    let agent = &agent;
                    scope.spawn(move || ask_cache(agent, cache, path, hash, netrc))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Verdict::Unreachable("lookup panicked".into()))
                })
                .collect()
        });
        answers.extend(verdicts);
    }

    // The store's verdict first, then the caches' in their order.
    let mut answers = answers.into_iter();
    outputs
        .iter()
        .map(|(path, hash)| {
            let mut comparisons = Vec::new();
            let store = parse_hash(hash).and_then(|h| compare_with_store(Path::new(path), &h));
            if let Some(verdict) = store {
                comparisons.push(Comparison {
                    source: Source::Store,
                    verdict,
                });
            }
            for cache in &caches.http {
                let verdict = answers.next().expect("one answer per output and cache");
                comparisons.push(Comparison {
                    source: Source::Cache(cache.clone()),
                    verdict,
                });
            }
            OutputComparisons {
                path: path.clone(),
                comparisons,
            }
        })
        .collect()
}

/// The verdict of this machine's copy of `path`, hashed as the guest hashed
/// its build; none when the machine has no copy.
fn compare_with_store(path: &Path, hash: &NixHash) -> Option<Verdict> {
    if std::fs::symlink_metadata(path).is_err() {
        return None;
    }
    let verdict = match rewind_init::nar_hash(path)
        .ok()
        .and_then(|h| parse_hash(&h))
    {
        Some(ours) if &ours == hash => Verdict::Matches,
        Some(_) => Verdict::Differs,
        None => Verdict::Unreachable(format!("{} could not be read", path.display())),
    };
    Some(verdict)
}

/// Asks one cache for the `.narinfo` of `path` and compares its NarHash
/// with `hash`, logging in with the netrc entry for the cache's host if
/// there is one.
fn ask_cache(agent: &ureq::Agent, cache: &str, path: &str, hash: &str, netrc: &Netrc) -> Verdict {
    let Some(ours) = parse_hash(hash) else {
        return Verdict::Unreachable(format!("the run reported {hash}, which is not a hash"));
    };
    let name = Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let Some(hash_part) = name.get(..STORE_HASH_LEN) else {
        return Verdict::Unreachable(format!("{path} is not a store path"));
    };

    let url = format!("{cache}/{hash_part}.narinfo");
    let mut request = agent.get(&url);
    if let Some(login) = netrc.for_host(host_of(cache)) {
        let pair = format!("{}:{}", login.login, login.password);
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pair);
        request = request.header("Authorization", format!("Basic {encoded}"));
    }
    match request.call() {
        Ok(mut response) => {
            let status = response.status().as_u16();
            let body = response.body_mut().read_to_string().unwrap_or_default();
            verdict_for(status, &body, &ours)
        }
        Err(e) => Verdict::Unreachable(e.to_string()),
    }
}

/// What a cache's answer to a `.narinfo` request says about an output whose
/// NAR hash is `ours`.
fn verdict_for(status: u16, body: &str, ours: &NixHash) -> Verdict {
    match status {
        200 => match NarInfo::parse_in(&StoreDir::default(), body.as_bytes()) {
            Ok(info) if info.nar_hash() == ours => Verdict::Matches,
            Ok(_) => Verdict::Differs,
            Err(e) => Verdict::Unreachable(format!("its narinfo does not parse: {e}")),
        },
        404 => Verdict::Absent,
        401 | 403 => Verdict::NeedsCredentials,
        other => Verdict::Unreachable(format!("it answered {other}")),
    }
}

/// The host of a cache URL, without its port, which is what a netrc entry
/// names.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or(rest);
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

#[cfg(test)]
mod tests {
    // The pure parts against fixed text, and a lookup against a small HTTP
    // server on loopback that serves a real narinfo from cache.nixos.org,
    // to one client only when it brings the right credentials.
    use super::*;

    const PKGCONF: &str = "/nix/store/0rk0cxq5669yg1bwzwk3s6nkhk9fqk6w-pkgconf-2.4.3";
    const PKGCONF_HASH: &str =
        "sha256:dfd7aed38b3557dcbca63b4a1a7572f4af98c98e7649549a6d53cbfbcee928f4";
    const OTHER_HASH: &str =
        "sha256:1c37d01af40be2e80691de3cc3df44377a699afbb17c68f080964b2fd071fc13";
    const PKGCONF_NARINFO: &str = "StorePath: /nix/store/0rk0cxq5669yg1bwzwk3s6nkhk9fqk6w-pkgconf-2.4.3
URL: nar/14avp6a03i2nbw8m0p83ydp02h3vmy6kl3g8am1yssdy7iwy92n0.nar.xz
Compression: xz
FileHash: sha256:14avp6a03i2nbw8m0p83ydp02h3vmy6kl3g8am1yssdy7iwy92n0
FileSize: 23740
NarHash: sha256:1x18x77gpjskdnd58jbniv4ribzlf9siljivlsydqmrmig9sxmyz
NarSize: 92040
References: 7nbi22pcc92y2fqbkyp7h3srvvklmckb-glibc-2.40-224 zji6z8177mllb0p87l8jl5nf62b2smam-pkgconf-2.4.3-lib
Deriver: 2v5iazia059xg2fzpzk2g1dbwqhaks57-pkgconf-2.4.3.drv
Sig: cache.nixos.org-1:xWgEVZwsFlqD67Y+nn8zPPOgmG80avggRX73ki5opbVVWvx/b9JpmRAD80hyXtQufVMzAfGRcAb14wxcTYkFAA==
";

    #[test]
    fn substituters_split_into_http_caches_and_the_rest() {
        // `nix config show substituters` prints one line of URLs, some with
        // a query such as ?priority=100. HTTP ones are asked, without the
        // query or a trailing slash; the rest are skipped. Extra URLs come
        // after, and a URL given twice is asked once.
        let caches = split_substituters(
            "http://leviathan:5000?priority=100 https://cache.nixos.org/ s3://bucket ssh-ng://host\n",
            &[
                "https://example.cachix.org".to_string(),
                "https://cache.nixos.org".to_string(),
            ],
        );
        assert_eq!(
            caches.http,
            vec![
                "http://leviathan:5000",
                "https://cache.nixos.org",
                "https://example.cachix.org"
            ]
        );
        assert_eq!(caches.skipped, vec!["s3://bucket", "ssh-ng://host"]);
    }

    #[test]
    fn netrc_gives_each_host_its_login() {
        // A machine's own entry, then the default entry for any other host,
        // in the netrc format curl and Nix read. A macdef's lines are
        // skipped.
        let netrc = Netrc::parse(
            "machine cache.example.org\n  login alice\n  password secret\n\
             macdef init\nnot a token\n\n\
             default login anon password guest\n",
        );
        assert_eq!(
            netrc.for_host("cache.example.org"),
            Some(&Login {
                login: "alice".into(),
                password: "secret".into()
            })
        );
        assert_eq!(
            netrc.for_host("other.example.org"),
            Some(&Login {
                login: "anon".into(),
                password: "guest".into()
            })
        );
        assert_eq!(Netrc::parse("").for_host("cache.example.org"), None);
    }

    #[test]
    fn a_cache_answer_becomes_a_verdict() {
        // 200 with the same NarHash matches and with another differs; 404
        // means the cache has no build; 401 and 403 need credentials;
        // anything else, or a narinfo that does not parse, is unreachable.
        let ours = parse_hash(PKGCONF_HASH).unwrap();
        let other = parse_hash(OTHER_HASH).unwrap();
        assert_eq!(verdict_for(200, PKGCONF_NARINFO, &ours), Verdict::Matches);
        assert_eq!(verdict_for(200, PKGCONF_NARINFO, &other), Verdict::Differs);
        assert_eq!(verdict_for(404, "", &ours), Verdict::Absent);
        assert_eq!(verdict_for(401, "", &ours), Verdict::NeedsCredentials);
        assert_eq!(verdict_for(403, "", &ours), Verdict::NeedsCredentials);
        assert!(matches!(
            verdict_for(500, "", &ours),
            Verdict::Unreachable(_)
        ));
        assert!(matches!(
            verdict_for(200, "garbage", &ours),
            Verdict::Unreachable(_)
        ));
    }

    #[test]
    fn a_copy_on_disk_is_hashed_as_the_guest_hashed_it() {
        // A path this machine has is hashed and compared; one it lacks has
        // nothing to say.
        let dir = std::env::temp_dir().join(format!("rewind-compare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a");
        std::fs::write(&file, "hello\n").unwrap();
        assert_eq!(
            compare_with_store(&file, &parse_hash(OTHER_HASH).unwrap()),
            Some(Verdict::Matches)
        );
        assert_eq!(
            compare_with_store(&file, &parse_hash(PKGCONF_HASH).unwrap()),
            Some(Verdict::Differs)
        );
        assert_eq!(
            compare_with_store(&dir.join("missing"), &parse_hash(OTHER_HASH).unwrap()),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A one-file HTTP cache on loopback: it serves pkgconf's narinfo to a
    /// request with alice's credentials, answers 401 to one without, and 404
    /// for any other path. Returns its URL.
    fn private_cache() -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let expected = format!(
            "authorization: basic {}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "alice:secret")
        )
        .to_lowercase();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut authorized = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    authorized |= line.trim().to_lowercase() == expected;
                }
                let wanted = "GET /0rk0cxq5669yg1bwzwk3s6nkhk9fqk6w.narinfo ";
                let (status, body) = match (request.starts_with(wanted), authorized) {
                    (true, true) => ("200 OK", PKGCONF_NARINFO),
                    (true, false) => ("401 Unauthorized", ""),
                    (false, _) => ("404 Not Found", ""),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        url
    }

    #[test]
    fn a_private_cache_answers_with_the_netrc_login() {
        // The same lookup against the loopback cache: with no netrc entry
        // it needs credentials, with alice's it matches, and a path the
        // cache lacks is absent.
        let cache = private_cache();
        let host = host_of(&cache).to_string();
        let caches = Caches {
            http: vec![cache.clone()],
            skipped: vec![],
        };
        let outputs = vec![
            (PKGCONF.to_string(), PKGCONF_HASH.to_string()),
            (
                "/nix/store/00000000000000000000000000000000-absent".to_string(),
                PKGCONF_HASH.to_string(),
            ),
        ];

        let anonymous = compare(&outputs, &caches, &Netrc::default());
        assert_eq!(
            anonymous[0].comparisons[0].verdict,
            Verdict::NeedsCredentials
        );

        let netrc = Netrc::parse(&format!("machine {host} login alice password secret"));
        let found = compare(&outputs, &caches, &netrc);
        assert_eq!(
            found[0].comparisons,
            vec![Comparison {
                source: Source::Cache(cache.clone()),
                verdict: Verdict::Matches
            }]
        );
        assert_eq!(found[1].comparisons[0].verdict, Verdict::Absent);
    }
}
