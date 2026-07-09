//! Isolation benchmark: walk a repo's full history and format records,
//! timing the pure libgit2 side without any FFI or Haskell involvement.
//! Usage: cargo run --release --example walk -- /path/to/repo <sha>

use git2::{Repository, Sort};
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: walk REPO SHA");
    let sha = args.next().expect("usage: walk REPO SHA");
    let repo = Repository::discover(&path).unwrap();
    let id = git2::Oid::from_str(&sha).unwrap();

    let t = Instant::now();
    let mut rw = repo.revwalk().unwrap();
    rw.set_sorting(Sort::NONE).unwrap();
    rw.push(id).unwrap();
    let ids: Vec<git2::Oid> = rw.map(|o| o.unwrap()).collect();
    eprintln!("revwalk only: {} commits in {:?}", ids.len(), t.elapsed());

    let t = Instant::now();
    let mut out = Vec::new();
    for oid in &ids {
        let commit = repo.find_commit(*oid).unwrap();
        out.extend_from_slice(commit.id().to_string().as_bytes());
        out.push(0);
        let author = commit.author();
        out.extend_from_slice(author.email_bytes());
        out.push(0);
        out.extend_from_slice(author.name_bytes());
        out.push(0);
        out.push(0); // skip date formatting for isolation
        for p in commit.parent_ids() {
            out.extend_from_slice(p.to_string().as_bytes());
        }
        out.push(0);
        out.extend_from_slice(commit.tree_id().to_string().as_bytes());
        out.push(0);
        if let Some(s) = commit.summary_bytes() {
            out.extend_from_slice(s);
        }
        out.push(b'\n');
    }
    eprintln!("find_commit+format: {} bytes in {:?}", out.len(), t.elapsed());
}
