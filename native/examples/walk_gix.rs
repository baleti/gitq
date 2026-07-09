//! Isolation benchmark: the same walk+format as examples/walk.rs (git2),
//! implemented with gix — to measure whether gix delivers the deep-walk
//! advantage libgit2 didn't.
//! Usage: cargo run --release --example walk_gix -- /path/to/repo <sha>

use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: walk_gix REPO SHA");
    let sha = args.next().expect("usage: walk_gix REPO SHA");
    let repo = gix::discover(&path).unwrap();
    let id = gix::ObjectId::from_hex(sha.as_bytes()).unwrap();

    let t = Instant::now();
    let walk = repo
        .rev_walk(Some(id))
        .use_commit_graph(true)
        .all()
        .unwrap();
    let infos: Vec<_> = walk.map(|i| i.unwrap()).collect();
    eprintln!("revwalk only: {} commits in {:?}", infos.len(), t.elapsed());

    let t = Instant::now();
    let mut out = Vec::new();
    for info in &infos {
        let commit = repo.find_object(info.id).unwrap().try_into_commit().unwrap();
        let commit = commit.decode().unwrap();
        out.extend_from_slice(info.id.to_hex().to_string().as_bytes());
        out.push(0);
        let author = commit.author().unwrap();
        out.extend_from_slice(author.email);
        out.push(0);
        out.extend_from_slice(author.name);
        out.push(0);
        out.push(0); // skip date formatting for isolation (matches walk.rs)
        for p in &info.parent_ids {
            out.extend_from_slice(p.to_hex().to_string().as_bytes());
        }
        out.push(0);
        out.extend_from_slice(commit.tree().to_hex().to_string().as_bytes());
        out.push(0);
        out.extend_from_slice(commit.message().summary().as_ref());
        out.push(b'\n');
    }
    eprintln!("decode+format: {} bytes in {:?}", out.len(), t.elapsed());
}
