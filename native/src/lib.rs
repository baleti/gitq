//! In-process git backend for gitq, linked into the Haskell executable.
//!
//! One entry point resolves commit SHAs to records — either just the given
//! SHAs (`walk = 0`, the batched-lookup path) or their full ancestor
//! closure (`walk = 1`, the `parent*`/`parent+` path).  Records use the
//! exact byte layout of gitq's `git log` format string
//! (`%H%x00%ae%x00%an%x00%ai%x00%P%x00%T%x00%s`, one record per line), so
//! the Haskell side parses both backends with the same function.
//!
//! Implemented with gix, chosen over git2/libgit2 on measurement (see
//! examples/walk.rs vs examples/walk_gix.rs): on git/git's 81k commits,
//! gix walks the closure in 0.08 s with a commit-graph present (0.7 s
//! without) where libgit2 takes 1.7 s and ignores the graph entirely.
//!
//! Errors never cross the boundary: any failure (unreadable repo, bad SHA,
//! panic) returns NULL and the caller falls back to the subprocess path.

use gix::ObjectId;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::panic::AssertUnwindSafe;

/// Append `chrono`-formatted `%ai` (author date, "YYYY-MM-DD HH:MM:SS
/// +ZZZZ" in the author's own offset) from the raw signature time bytes
/// ("1712345678 +0200").
fn push_date(raw: &[u8], out: &mut Vec<u8>) {
    let parsed = (|| {
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
    })();
    if let Some(s) = parsed {
        out.extend_from_slice(s.as_bytes());
    }
}

/// Append one commit in the shared record format.  Unknown/undecodable
/// SHAs simply produce no record (the Haskell side reassembles by map
/// lookup, so a missing record is a missing commit, same as the
/// subprocess path).  `parents` come from the walk when available,
/// avoiding a second decode.
fn push_commit(
    repo: &gix::Repository,
    id: ObjectId,
    walk_parents: Option<&[ObjectId]>,
    out: &mut Vec<u8>,
) {
    let commit = match repo
        .find_object(id)
        .ok()
        .and_then(|o| o.try_into_commit().ok())
    {
        Some(c) => c,
        None => return,
    };
    let decoded = match commit.decode() {
        Ok(d) => d,
        Err(_) => return,
    };
    out.extend_from_slice(id.to_hex().to_string().as_bytes());
    out.push(0);
    let author = decoded.author();
    match author {
        Ok(a) => {
            out.extend_from_slice(a.email);
            out.push(0);
            out.extend_from_slice(a.name);
            out.push(0);
            push_date(a.time.as_ref(), out);
            out.push(0);
        }
        Err(_) => {
            out.push(0);
            out.push(0);
            out.push(0);
        }
    }
    let mut first = true;
    let mut push_parent = |p: &ObjectId| {
        if !first {
            out.push(b' ');
        }
        first = false;
        out.extend_from_slice(p.to_hex().to_string().as_bytes());
    };
    match walk_parents {
        Some(ps) => ps.iter().for_each(&mut push_parent),
        None => decoded.parents().collect::<Vec<_>>().iter().for_each(&mut push_parent),
    }
    out.push(0);
    out.extend_from_slice(decoded.tree().to_hex().to_string().as_bytes());
    out.push(0);
    // %s (the subject): first paragraph, newlines collapsed to spaces —
    // gix's summary has the same semantics
    out.extend_from_slice(decoded.message().summary().as_ref());
    out.push(b'\n');
}

fn run(repo_path: &CStr, starts: &CStr, walk: c_int) -> Option<Vec<u8>> {
    let path = repo_path.to_str().ok()?;
    let input = starts.to_str().ok()?;
    let mut ids = Vec::new();
    for token in input.split_whitespace() {
        ids.push(ObjectId::from_hex(token.as_bytes()).ok()?);
    }
    if ids.is_empty() {
        return None;
    }
    let mut repo = gix::discover(path).ok()?;
    // delta chains resolve against cached bases instead of re-inflating
    repo.object_cache_size_if_unset(64 * 1024 * 1024);
    let mut out = Vec::new();
    if walk != 0 {
        let walk = repo
            .rev_walk(ids)
            .use_commit_graph(true)
            .all()
            .ok()?;
        for info in walk {
            let info = info.ok()?;
            push_commit(&repo, info.id, Some(&info.parent_ids), &mut out);
        }
    } else {
        for id in ids {
            push_commit(&repo, id, None, &mut out);
        }
    }
    Some(out)
}

/// Resolve SHAs (whitespace-separated, full hex) to commit records.
/// `walk = 1` walks the full ancestor closure; `walk = 0` resolves exactly
/// the given SHAs.  On success writes the buffer length to `out_len` and
/// returns the buffer (free with `gitq_native_free`); on any failure
/// returns NULL.
///
/// # Safety
/// `repo_path` and `starts` must be valid NUL-terminated strings and
/// `out_len` a valid pointer, for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn gitq_native_commits(
    repo_path: *const c_char,
    starts: *const c_char,
    walk: c_int,
    out_len: *mut usize,
) -> *mut u8 {
    if repo_path.is_null() || starts.is_null() || out_len.is_null() {
        return std::ptr::null_mut();
    }
    *out_len = 0;
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run(CStr::from_ptr(repo_path), CStr::from_ptr(starts), walk)
    }));
    match result {
        Ok(Some(out)) => {
            let boxed = out.into_boxed_slice();
            let len = boxed.len();
            let ptr = Box::into_raw(boxed) as *mut u8;
            *out_len = len;
            ptr
        }
        _ => std::ptr::null_mut(),
    }
}

/// Free a buffer returned by `gitq_native_commits`.
///
/// # Safety
/// `ptr`/`len` must be exactly what `gitq_native_commits` returned, and
/// must not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn gitq_native_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Box::from_raw(std::slice::from_raw_parts_mut(ptr, len)));
    }
}
