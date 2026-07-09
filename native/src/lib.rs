//! In-process git backend for gitq, linked into the Haskell executable.
//!
//! One entry point resolves commit SHAs to records — either just the given
//! SHAs (`walk = 0`, the batched-lookup path) or their full ancestor
//! closure (`walk = 1`, the `parent*`/`parent+` path).  Records use the
//! exact byte layout of gitq's `git log` format string
//! (`%H%x00%ae%x00%an%x00%ai%x00%P%x00%T%x00%s`, one record per line), so
//! the Haskell side parses both backends with the same function.
//!
//! Errors never cross the boundary: any failure (unreadable repo, bad SHA,
//! panic) returns NULL and the caller falls back to the subprocess path.

use git2::{Oid, Repository, Sort};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::panic::AssertUnwindSafe;

/// Append one commit in the shared record format, or nothing if the SHA
/// doesn't resolve (matching `git log --no-walk` skipping unknown input is
/// not needed — unknown SHAs simply produce no record, and the Haskell
/// side reassembles by map lookup).
fn push_commit(repo: &Repository, oid: Oid, out: &mut Vec<u8>) {
    let commit = match repo.find_commit(oid) {
        Ok(c) => c,
        Err(_) => return,
    };
    out.extend_from_slice(commit.id().to_string().as_bytes());
    out.push(0);
    let author = commit.author();
    out.extend_from_slice(author.email_bytes());
    out.push(0);
    out.extend_from_slice(author.name_bytes());
    out.push(0);
    // git's %ai: author date as "YYYY-MM-DD HH:MM:SS +ZZZZ" in the
    // author's own utc offset
    let when = author.when();
    if let Some(utc) = chrono::DateTime::from_timestamp(when.seconds(), 0) {
        let offset = chrono::FixedOffset::east_opt(when.offset_minutes() * 60)
            .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap());
        let local = utc.with_timezone(&offset);
        out.extend_from_slice(local.format("%Y-%m-%d %H:%M:%S %z").to_string().as_bytes());
    }
    out.push(0);
    let mut first = true;
    for p in commit.parent_ids() {
        if !first {
            out.push(b' ');
        }
        first = false;
        out.extend_from_slice(p.to_string().as_bytes());
    }
    out.push(0);
    out.extend_from_slice(commit.tree_id().to_string().as_bytes());
    out.push(0);
    // %s (the subject): first paragraph, newlines collapsed to spaces —
    // libgit2's summary has the same semantics
    if let Some(s) = commit.summary_bytes() {
        out.extend_from_slice(s);
    }
    out.push(b'\n');
}

fn run(repo_path: &CStr, starts: &CStr, walk: c_int) -> Option<Vec<u8>> {
    let path = repo_path.to_str().ok()?;
    let input = starts.to_str().ok()?;
    let mut ids = Vec::new();
    for token in input.split_whitespace() {
        ids.push(Oid::from_str(token).ok()?);
    }
    if ids.is_empty() {
        return None;
    }
    let repo = Repository::discover(path).ok()?;
    let mut out = Vec::new();
    if walk != 0 {
        let mut rw = repo.revwalk().ok()?;
        rw.set_sorting(Sort::NONE).ok()?;
        for id in &ids {
            rw.push(*id).ok()?;
        }
        for oid in rw {
            push_commit(&repo, oid.ok()?, &mut out);
        }
    } else {
        for id in ids {
            push_commit(&repo, id, &mut out);
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
