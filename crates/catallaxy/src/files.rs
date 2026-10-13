//! Expands the path arguments of `check` and `fmt`: files are taken as given, directories are
//! walked recursively for `*.cx` files.

use std::path::{Path, PathBuf};

/// Expands `args` into the files to process, in argument order (each directory's files sorted).
///
/// While walking a directory, hidden directories and `target/` are skipped, and so is any
/// directory named in `skip_dirs` (`fmt` skips `reject/`, whose programs are meant to be wrong).
/// A file named directly is always included. A path that does not exist is returned as is, so
/// the caller reports it.
pub fn expand(args: &[&str], skip_dirs: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in args {
        let p = Path::new(a);
        if p.is_dir() {
            let mut found = Vec::new();
            walk(p, skip_dirs, &mut found);
            found.sort();
            out.extend(found);
        } else {
            out.push(p.to_path_buf());
        }
    }
    out
}

fn walk(dir: &Path, skip_dirs: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if p.is_dir() {
            if name.starts_with('.') || name == "target" || skip_dirs.contains(&name.as_ref()) {
                continue;
            }
            walk(&p, skip_dirs, out);
        } else if p.extension().is_some_and(|x| x == "cx") {
            out.push(p);
        }
    }
}
