//! In-process git backend, via gix.
//!
//! One entry point resolves commit SHAs to frames — either just the given
//! SHAs (the batched-lookup path) or their full ancestor closure (the
//! `parent*`/`parent+` path).
//!
//! # What the port deleted here
//!
//! In the Haskell build this was a separate crate behind a C ABI: gix built
//! records in the exact byte layout of gitq's `git log` format string, the
//! Haskell side parsed them back with the same `parseCommitLine` it used for
//! subprocess output, and a custom `Setup.hs` injected the static library's
//! absolute path because `ghc-pkg` rejects relative `extra-lib-dirs`.  All
//! of that — the C entry point, the manual `Box::into_raw` buffer, the free
//! function, the `catch_unwind` guard, the cabal flag, the serialize/reparse
//! round-trip — is gone.  Frames are constructed directly.
//!
//! gix was chosen over git2/libgit2 on measurement: on git/git's 81k
//! commits, gix walks the closure in 0.08 s with a commit-graph present
//! (0.7 s without) where libgit2 takes 1.7 s and ignores the graph entirely.
//!
//! Failures never propagate: any problem (unreadable repo, bad SHA) returns
//! `None` and the caller falls back to the subprocess path, so the two
//! backends are always interchangeable.  `GITQ_NO_NATIVE=1` forces the
//! fallback, which is how the two are A/B'd from one binary.

use std::collections::BTreeMap;
use std::sync::Arc;

use gix::ObjectId;

use crate::frame::{Frame, FrameType, Value};

/// Whether the in-process backend is permitted.  A single binary can run
/// either path, so a corpus diff can attribute a difference to the backend
/// rather than to the build.
pub fn native_enabled() -> bool {
    !matches!(
        std::env::var("GITQ_NO_NATIVE").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// `%ai` (author date, "YYYY-MM-DD HH:MM:SS +ZZZZ" in the author's own
/// offset) from the raw signature time bytes ("1712345678 +0200").
///
/// gix exposes `SignatureRef.time` as a raw `&BStr`, so this is parsed by
/// hand — and it must match `git log --format=%ai` byte for byte, since the
/// subprocess path produces exactly that.
fn format_date(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?;
    let mut it = s.split_whitespace();
    let secs: i64 = it.next()?.parse().ok()?;
    let off = it.next().unwrap_or("+0000").as_bytes();
    let offset_secs: i32 = if off.len() == 5 {
        let sign = if off[0] == b'-' { -1 } else { 1 };
        let hh = (off[1] - b'0') as i32 * 10 + (off[2] - b'0') as i32;
        let mm = (off[3] - b'0') as i32 * 10 + (off[4] - b'0') as i32;
        sign * (hh * 3600 + mm * 60)
    } else {
        0
    };
    let utc = chrono::DateTime::from_timestamp(secs, 0)?;
    let offset = chrono::FixedOffset::east_opt(offset_secs)?;
    Some(
        utc.with_timezone(&offset)
            .format("%Y-%m-%d %H:%M:%S %z")
            .to_string(),
    )
}

fn s(v: &str) -> Value {
    Value::Str(Arc::from(v))
}

/// Build one commit frame.  Unknown or undecodable SHAs simply produce no
/// frame — callers reassemble by map lookup, so a missing frame is a missing
/// commit, exactly as on the subprocess path.
///
/// `walk_parents` come from the revwalk when available, avoiding a second
/// decode of the commit object.
fn commit_frame(
    repo: &gix::Repository,
    id: ObjectId,
    walk_parents: Option<&[ObjectId]>,
) -> Option<Frame> {
    let object = repo.find_object(id).ok()?.try_into_commit().ok()?;
    let decoded = object.decode().ok()?;

    let mut attrs = BTreeMap::new();
    attrs.insert("sha".to_string(), s(&id.to_hex().to_string()));

    match decoded.author() {
        Ok(a) => {
            attrs.insert(
                "email".to_string(),
                s(&String::from_utf8_lossy(a.email)),
            );
            attrs.insert("author".to_string(), s(&String::from_utf8_lossy(a.name)));
            attrs.insert(
                "date".to_string(),
                s(&format_date(a.time.as_ref()).unwrap_or_default()),
            );
        }
        Err(_) => {
            // The subprocess path emits empty fields rather than dropping
            // the commit; match that.
            attrs.insert("email".to_string(), s(""));
            attrs.insert("author".to_string(), s(""));
            attrs.insert("date".to_string(), s(""));
        }
    }

    attrs.insert(
        "tree".to_string(),
        s(&decoded.tree().to_hex().to_string()),
    );
    // %s (the subject): first paragraph, newlines collapsed to spaces —
    // gix's summary has the same semantics
    attrs.insert(
        "message".to_string(),
        s(&String::from_utf8_lossy(decoded.message().summary().as_ref())),
    );

    let parents: Vec<Arc<str>> = match walk_parents {
        Some(ps) => ps.iter().map(|p| Arc::from(p.to_hex().to_string())).collect(),
        None => decoded
            .parents()
            .map(|p| Arc::from(p.to_hex().to_string()))
            .collect(),
    };

    Some(Frame {
        ty: FrameType::Commit,
        parents,
        attrs,
    })
}

/// Resolve full-hex SHAs to commit frames.  `walk` walks the full ancestor
/// closure; otherwise exactly the given SHAs are resolved.
///
/// Returns `None` on any failure so the caller falls back to subprocess git.
pub fn native_commits(walk: bool, shas: &[Arc<str>]) -> Option<Vec<Frame>> {
    if !native_enabled() || shas.is_empty() {
        return None;
    }

    let mut ids = Vec::with_capacity(shas.len());
    for sha in shas {
        ids.push(ObjectId::from_hex(sha.as_bytes()).ok()?);
    }

    let mut repo = gix::discover(".").ok()?;
    // delta chains resolve against cached bases instead of re-inflating
    repo.object_cache_size_if_unset(64 * 1024 * 1024);

    let mut out = Vec::new();
    if walk {
        let walk = repo.rev_walk(ids).use_commit_graph(true).all().ok()?;
        for info in walk {
            let info = info.ok()?;
            if let Some(f) = commit_frame(&repo, info.id, Some(&info.parent_ids)) {
                out.push(f);
            }
        }
    } else {
        for id in ids {
            if let Some(f) = commit_frame(&repo, id, None) {
                out.push(f);
            }
        }
    }
    Some(out)
}

/// Index frames by their `sha`, for the callers that reassemble by lookup.
pub fn by_sha(frames: Vec<Frame>) -> BTreeMap<String, Frame> {
    frames
        .into_iter()
        .filter_map(|f| {
            let sha = f.field("sha")?.as_str()?.to_string();
            Some((sha, f))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_match_gits_percent_ai_format() {
        // "%Y-%m-%d %H:%M:%S %z" in the author's own offset, which is what
        // `git log --format=%ai` prints and what the subprocess path parses
        assert_eq!(
            format_date(b"1704106800 +0000").as_deref(),
            Some("2024-01-01 11:00:00 +0000")
        );
        // a non-UTC offset must be rendered in that offset, not normalised
        let d = format_date(b"1704106800 +0200").unwrap();
        assert!(d.ends_with("+0200"), "{d}");
        assert!(d.starts_with("2024-01-01 13:00:00"), "{d}");
        let d = format_date(b"1704106800 -0500").unwrap();
        assert!(d.ends_with("-0500"), "{d}");
    }

    #[test]
    fn a_missing_offset_defaults_to_utc() {
        assert!(format_date(b"1704106800").unwrap().ends_with("+0000"));
    }

    #[test]
    fn malformed_signature_times_yield_none_rather_than_a_wrong_date() {
        assert!(format_date(b"").is_none());
        assert!(format_date(b"not-a-number +0000").is_none());
    }

    #[test]
    fn the_env_switch_disables_the_backend() {
        // the switch is what makes one binary A/B-testable
        std::env::set_var("GITQ_NO_NATIVE", "1");
        assert!(!native_enabled());
        assert!(native_commits(false, &[Arc::from("deadbeef")]).is_none());
        std::env::remove_var("GITQ_NO_NATIVE");
        assert!(native_enabled());
    }

    #[test]
    fn an_empty_sha_list_never_reaches_gix() {
        assert!(native_commits(false, &[]).is_none());
    }

    #[test]
    fn non_hex_shas_fall_back_rather_than_erroring() {
        assert!(native_commits(false, &[Arc::from("not-a-sha")]).is_none());
    }
}
