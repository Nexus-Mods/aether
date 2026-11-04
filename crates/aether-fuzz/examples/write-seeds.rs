//! `cargo run -p aether-fuzz --example write-seeds -- <dir>`: writes every
//! seed into `<dir>/<target>/seed-<n>`, the corpus layout cargo-fuzz reads.

fn main() {
    let dir = std::env::args_os()
        .nth(1)
        .expect("usage: write-seeds <corpus dir>");
    let dir = std::path::Path::new(&dir);
    for target in aether_fuzz::TARGETS {
        std::fs::create_dir_all(dir.join(target)).expect("create the corpus folder");
    }
    let mut n = std::collections::HashMap::new();
    for (target, bytes) in aether_fuzz::seeds() {
        let i = n.entry(target).or_insert(0usize);
        std::fs::write(dir.join(target).join(format!("seed-{i:03}")), bytes).expect("write a seed");
        *i += 1;
    }
}
