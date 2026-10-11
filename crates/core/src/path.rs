//! Request-path checks shared by ferryman and ferryman-edge. Detection only:
//! the forwarded path is never rewritten.

use crate::route::RouteTable;

/// Would an upstream that treats `%2F`/`%5C`/`\` as `/` and drops `;params`
/// route `raw_path` differently from ferryman? True when any such reading
/// selects a different route (or none versus some) than
/// [`RouteTable::lookup`] on `raw_path`; callers answer 400.
///
/// Readings compared (each looked up like a raw path): separators flattened
/// to `/`; then `;params` dropped per segment; `;params` dropped up to the
/// next raw `/` first (Tomcat/Spring order); that, then flattened. The same
/// readings are also derived from the normalised path, which exposes
/// separators hidden behind odd escapes (`%2%46`, `%25%32%46`, which
/// normalises to `%252F`).
///
/// Takes the RAW path, like `lookup`, and requires [`bad_path`] to have
/// accepted it first (dot segments are not re-checked here). Costs nothing
/// unless the path contains `%2F`/`%5C` (any hex case), `\` or `;` (in the
/// raw or normalised form). Encoded separators that do not change the
/// selected route (`/api/v4/projects/group%2Fproject` under `/api`) stay
/// allowed.
///
/// # Known limits
///
/// Routes are case-sensitive, so an upstream that folds case (`/API/x`) is
/// not detected. Also not covered: overlong UTF-8 separators (`%c0%af`), `%3B`
/// (not treated as `;`), and decoding beyond the forms above (triple
/// encoding and the like).
pub fn ambiguous_route(table: &RouteTable, raw_path: &str) -> bool {
    let norm = crate::route::normalize(raw_path);
    if !(suspicious(raw_path) || suspicious(&norm)) {
        return false;
    }
    fn pick<'t>(t: &'t RouteTable, p: &str) -> Option<&'t str> {
        t.lookup(p).map(|r| r.prefix.as_str())
    }

    let canon = pick(table, raw_path);
    let ambiguous = [raw_path, &*norm].into_iter().any(|s| {
        let stripped = strip_params(s);
        [
            flatten(s),
            strip_params(&flatten(s)),
            stripped.clone(),
            flatten(&stripped),
        ]
        .iter()
        .any(|r| pick(table, r) != canon)
    });
    ambiguous
}

fn suspicious(s: &str) -> bool {
    let b = s.as_bytes();
    b.iter().enumerate().any(|(i, &c)| match c {
        b'\\' | b';' => true,
        b'%' => {
            matches!(
                b.get(i + 1..i + 3),
                Some([b'2', b'f' | b'F'] | [b'5', b'c' | b'C'])
            ) || matches!(
                b.get(i + 1..i + 5),
                Some([b'2', b'5', b'2', b'f' | b'F'] | [b'2', b'5', b'5', b'c' | b'C'])
            )
        }
        _ => false,
    })
}

/// `%2F`, `%5C` (any hex case) and `\` become `/`.
fn flatten(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i..] {
            [b'%', b'2', b'5', b'2', b'f' | b'F', ..]
            | [b'%', b'2', b'5', b'5', b'c' | b'C', ..] => {
                out.push('/');
                i += 5;
            }
            [b'%', b'2', b'f' | b'F', ..] | [b'%', b'5', b'c' | b'C', ..] => {
                out.push('/');
                i += 3;
            }
            [b'\\', ..] => {
                out.push('/');
                i += 1;
            }
            _ => {
                let n = s[i..].chars().next().map_or(1, char::len_utf8);
                out.push_str(&s[i..i + n]);
                i += n;
            }
        }
    }
    out
}

/// Drop `;...` up to the next literal `/`, per segment.
fn strip_params(s: &str) -> String {
    s.split('/')
        .map(|seg| seg.split(';').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("/")
}

/// True if `path` could be read as a dot segment by a normalising
/// upstream. `/`, `\`, `%2f` and `%5c` all count as separators, a `;param`
/// suffix is ignored per piece (Tomcat), and a piece that is `.` or `..`
/// (also `%2e`-encoded, any case) is rejected. Encoded separators inside an
/// otherwise ordinary segment (`group%2Fproject`) are allowed. Also rejected:
/// `%00`, `%u`/`%U` (non-standard) and double-encoded dot/slash (`%252e`,
/// `%252f`, `%255c`). Detection only; the forwarded path is never rewritten.
/// Not covered: overlong UTF-8 (`%c0%ae`) and Windows trailing-dot/space
/// trimming.
pub fn bad_path(path: &str) -> bool {
    let b = path.as_bytes();
    let (mut start, mut i) = (0, 0);
    while i <= b.len() {
        let sep = match b[i..] {
            [] => Some(0),
            [b'/' | b'\\', ..] => Some(1),
            [b'%', b'2', b'f' | b'F', ..] | [b'%', b'5', b'c' | b'C', ..] => Some(3),
            [b'%', b'0', b'0', ..] | [b'%', b'u' | b'U', ..] => return true,
            [b'%', b'2', b'5', b'2', b'e' | b'E' | b'f' | b'F', ..]
            | [b'%', b'2', b'5', b'5', b'c' | b'C', ..] => return true,
            _ => None,
        };
        match sep {
            Some(n) => {
                if dot_piece(&b[start..i]) {
                    return true;
                }
                i += n.max(1);
                start = i;
            }
            None => i += 1,
        }
    }
    false
}

/// [`bad_path`] on the raw path, and also on its normalised form when the raw
/// path contains `%`. Catches mixed encodings that only become a dot segment
/// after normalisation (`/api/%2%65%2%65/x` normalises to `/api/%2e%2e/x`).
/// This is what the proxy calls; `bad_path` keeps its raw-path semantics.
pub fn bad_path_normalized(raw: &str) -> bool {
    bad_path(raw) || (raw.contains('%') && bad_path(&crate::route::normalize(raw)))
}

/// `.` or `..` after dropping a `;...` suffix and decoding `%2e`.
fn dot_piece(piece: &[u8]) -> bool {
    let end = piece.iter().position(|&c| c == b';').unwrap_or(piece.len());
    let b = &piece[..end];
    let (mut i, mut dots) = (0, 0);
    while i < b.len() {
        match b[i..] {
            [b'.', ..] => i += 1,
            [b'%', b'2', b'e' | b'E', ..] => i += 3,
            _ => return false,
        }
        dots += 1;
    }
    matches!(dots, 1 | 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::{Route, Upstream};
    use std::time::Duration;

    #[test]
    fn mixed_encodings_hiding_dot_segments_are_rejected() {
        for p in ["/api/%2%65%2%65/x", "/api/%%32%65%%32%65/x", "/api/%2%65/x"] {
            assert!(!bad_path(p), "{p} is invisible to the raw check");
            assert!(bad_path_normalized(p), "{p}");
        }
        for p in ["/api/x", "/api/a%2eb", "/api/%41", "/api/%zz"] {
            assert!(!bad_path_normalized(p), "{p}");
        }
    }

    #[test]
    fn dot_segments_are_rejected() {
        for p in [
            "/api/../admin",
            "/api/%2e%2e/admin",
            "/api/..%2fadmin",
            "/api/./../admin",
            "/api/..",
            "/api/%2E%2E/x",
            "/api/%2e%2E/x",
            "/api/.%2e/x",
            "/api/..;/admin",
            "/api/.;x/y",
            "/api/%2e%2e;/x",
            "/api/..%5cx",
            "/api/..%5Cx",
            "/api/a\\..\\b",
            "/api/%2e/x",
            "/..",
            "/api/a%2f..",
            "/api/%2e%2e%2f",
            "/api/..%00",
            "/api/.%00.",
            "/api/%u002e%u002e",
            "/api/%U002e",
            "/api/%252e%252e",
            "/api/%252E",
            "/api/%252f",
            "/api/%252F",
            "/api/%255c",
            "/api/a%00b",
            "/api/a%5c..",
            "/api/a%5C..%5Cb",
        ] {
            assert!(bad_path(p), "{p}");
        }
    }

    #[test]
    fn legitimate_paths_are_allowed() {
        for p in [
            "/a..b/",
            "/.well-known/acme",
            "/file.tar.gz",
            "/api/v1.2/x",
            "/",
            "/api/...",
            "/api/a;..",
            "/api/%2e%2e%2e/x",
            "/api/%41/x",
            "/api/.a/x",
            "/api/a%2/",
            "/api/v4/projects/group%2Fproject",
            "/api/queues/%2F/q",
            "/@scope%2fpkg",
            "/%2F",
            "/api/%25",
            "/api/100%25",
            "/api/%c0%ae",
        ] {
            assert!(!bad_path(p), "{p}");
        }
    }

    fn table(prefixes: &[&str]) -> RouteTable {
        let up =
            Upstream::new("http://localhost:8001".parse().unwrap(), Default::default()).unwrap();
        let routes = prefixes
            .iter()
            .map(|p| Route::new(*p, up.clone()))
            .collect();
        RouteTable::new(routes, Duration::from_secs(30))
    }

    #[test]
    fn ambiguous_paths_are_flagged() {
        let t = table(&["/api", "/"]);
        for p in [
            "/api%2Fsecret",
            "/api%2fsecret",
            "/api%5Csecret",
            "/api%5csecret",
            "/api\\secret",
            "/api;x/secret",
            "/api;x",
            "/api%2%46secret",
            "/api%25%32%46secret",
        ] {
            assert!(ambiguous_route(&t, p), "{p}");
        }
        // some route vs none
        assert!(ambiguous_route(&table(&["/api"]), "/api%2Fx"));
        // `;params` stripped up to the next raw `/` before decoding
        let t = table(&["/", "/admin"]);
        for p in ["/;%2Fx/admin", "/;x%2Fy/admin", "/;%5Cz/admin"] {
            assert!(ambiguous_route(&t, p), "{p}");
        }
        assert!(ambiguous_route(&table(&["/", "/a", "/a/b"]), "/a/;x%2Fq/b"));
    }

    #[test]
    fn unambiguous_paths_are_allowed() {
        let t = table(&["/api", "/"]);
        for p in [
            "/api/v4/projects/group%2Fproject",
            "/api/secret",
            "/other/a%2Fb;c",
            "/api/a;x/b",
            "/api/x;jsessionid=1",
            "/",
        ] {
            assert!(!ambiguous_route(&t, p), "{p}");
        }
        assert!(!ambiguous_route(&table(&["/"]), "/files/a%2Fb"));
    }

    // Reference model: the four independent readings, written with plain
    // string ops, over every short path built from a small token set.
    #[test]
    fn exhaustive_short_paths_match_reference_model() {
        fn flat(s: &str) -> String {
            ["%2F", "%2f", "%5C", "%5c", "\\"]
                .iter()
                .fold(s.to_string(), |s, k| s.replace(k, "/"))
        }
        fn strip(s: &str) -> String {
            s.split('/')
                .map(|g| g.split(';').next().unwrap())
                .collect::<Vec<_>>()
                .join("/")
        }
        let toks = ["a", "b", "/", ";", "%2F", "x", "\\"];
        for routes in [
            &["/", "/a", "/a/b", "/b"][..],
            &["/a", "/a/b"][..],
            &["/a/", "/a/b/", "/"][..],
        ] {
            let t = table(routes);
            let pre = |p: &str| t.lookup(p).map(|r| r.prefix.clone());
            let mut stack = vec![("/".to_string(), 0)];
            while let Some((p, d)) = stack.pop() {
                if d < 4 {
                    stack.extend(toks.iter().map(|k| (format!("{p}{k}"), d + 1)));
                }
                if bad_path(&p) {
                    continue;
                }
                let c = pre(&p);
                let differs = [strip(&flat(&p)), flat(&strip(&p)), flat(&p), strip(&p)]
                    .iter()
                    .any(|r| pre(r) != c);
                assert_eq!(ambiguous_route(&t, &p), differs, "{routes:?} {p}");
            }
        }
    }
}
