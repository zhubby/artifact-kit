use anyhow::{Result, bail, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::{fs::File, path::Path};

pub fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains(['\\', ':', '\0'])
        && !path.starts_with('/')
        && path.split('/').all(|s| !matches!(s, "" | "." | ".."))
}
pub fn collect(root: &Path, patterns: &[String]) -> Result<Vec<String>> {
    let globs = patterns
        .iter()
        .map(|p| {
            ensure!(safe_relative(p), "unsafe artifact pattern");
            Ok(globset::GlobBuilder::new(p)
                .literal_separator(true)
                .build()?
                .compile_matcher())
        })
        .collect::<Result<Vec<_>>>()?;
    let mut count = vec![0; patterns.len()];
    let mut files = Vec::new();
    let entries = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !(entry.depth() == 1 && entry.file_name() == ".git"));
    for entry in entries {
        let entry = entry?;
        if entry.depth() == 0 || entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        let mut matched = false;
        for (index, glob) in globs.iter().enumerate() {
            if glob.is_match(&relative) {
                ensure!(
                    entry.file_type().is_file(),
                    "artifact is not a regular file: {relative}"
                );
                count[index] += 1;
                matched = true;
            }
        }
        if matched {
            files.push(relative.clone());
        }
        if entry.file_type().is_symlink()
            && patterns.iter().any(|p| {
                p.split('*')
                    .next()
                    .is_some_and(|prefix| prefix.starts_with(&format!("{relative}/")))
            })
        {
            bail!("artifact directory is a symlink: {relative}");
        }
    }
    for (index, count) in count.iter().enumerate() {
        ensure!(
            *count > 0,
            "required artifact pattern matched no files: {}",
            patterns[index]
        );
    }
    files.sort();
    Ok(files)
}
pub fn open(root: &Path, path: &str) -> Result<File> {
    ensure!(safe_relative(path), "unsafe artifact path");
    let mut dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            dir = dir.open_dir_nofollow(part)?;
        } else {
            let mut opts = OpenOptions::new();
            opts.read(true).follow(FollowSymlinks::No);
            let file = dir.open_with(part, &opts)?;
            ensure!(file.metadata()?.is_file(), "artifact is not a regular file");
            return Ok(file.into_std());
        }
    }
    bail!("empty artifact path")
}
