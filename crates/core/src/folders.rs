//! Interpret quick-folder queries without consulting the filesystem.

use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf, is_separator},
};

use crate::fuzzy::{Needle, fuzzy_score};

#[derive(Debug, PartialEq, Eq)]
pub enum FolderQuery {
    Known(String),
    LocalPath(PathBuf),
    Invalid,
}

/// Classify input and resolve explicit paths against local absolute roots.
/// Leaves `..` intact for filesystem resolution by the caller.
pub fn query(input: &str, base: &Path, home: Option<&Path>) -> FolderQuery {
    if let Some(tail) = input.strip_prefix('~')
        && (tail.is_empty() || tail.starts_with(is_separator))
    {
        return resolve_relative(home, Path::new(tail.trim_start_matches(is_separator)));
    }

    let path = Path::new(input);
    if is_local_absolute(path) {
        if has_ambiguous_component(path) {
            FolderQuery::Invalid
        } else {
            FolderQuery::LocalPath(path.to_path_buf())
        }
    } else if !is_unrooted(path) {
        FolderQuery::Invalid
    } else if matches!(input, "." | "..") || input.contains(is_separator) {
        resolve_relative(Some(base), path)
    } else {
        FolderQuery::Known(input.trim().to_owned())
    }
}

fn resolve_relative(root: Option<&Path>, path: &Path) -> FolderQuery {
    match root {
        Some(root)
            if is_local_absolute(root) && is_unrooted(path) && !has_ambiguous_component(path) =>
        {
            FolderQuery::LocalPath(if path.as_os_str().is_empty() {
                root.to_path_buf()
            } else {
                root.join(path)
            })
        }
        _ => FolderQuery::Invalid,
    }
}

fn is_unrooted(path: &Path) -> bool {
    !path.has_root() && !matches!(path.components().next(), Some(Component::Prefix(_)))
}

fn is_local_absolute(path: &Path) -> bool {
    #[cfg(windows)]
    if !matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::Disk(_))
    ) {
        return false;
    }
    path.is_absolute()
}

fn has_ambiguous_component(_path: &Path) -> bool {
    #[cfg(windows)]
    {
        // Win32 can trim trailing whitespace/dots, opening a different name.
        _path.components().any(|component| {
            matches!(component, Component::Normal(name)
                if matches!(name.as_encoded_bytes().last(), Some(b' ' | b'.')))
        })
    }
    #[cfg(not(windows))]
    false
}

/// Rank exact basenames, fuzzy basenames, then full paths. Equal scores keep the
/// source's MRU order, and display labels never serve as path identities.
pub fn rank_known(query: &str, dirs: &[PathBuf], limit: usize) -> Vec<PathBuf> {
    if limit == 0 {
        return Vec::new();
    }
    let needle = Needle::new(query);
    let mut seen = HashSet::new();
    let unique = dirs.iter().filter(|dir| seen.insert(dir.as_os_str()));
    if needle.lower.is_empty() {
        return unique.take(limit).cloned().collect();
    }

    let mut scored: Vec<_> = unique
        .filter_map(|dir| {
            if let Some(name) = dir.file_name() {
                let name = name.to_string_lossy();
                if name.to_lowercase() == needle.lower {
                    return Some(((0, 0), dir));
                }
                if let Some(score) = fuzzy_score(&needle.lower, &name) {
                    return Some(((1, score), dir));
                }
            }
            fuzzy_score(&needle.lower, &dir.to_string_lossy()).map(|score| ((2, score), dir))
        })
        .collect();
    scored.sort_by_key(|&(key, _)| key);
    scored
        .into_iter()
        .take(limit)
        .map(|(_, dir)| dir.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths<const N: usize>(items: [&str; N]) -> [PathBuf; N] {
        items.map(PathBuf::from)
    }

    fn roots() -> (PathBuf, PathBuf) {
        if cfg!(windows) {
            (r"C:\work\repo".into(), r"D:\home".into())
        } else {
            ("/work/repo".into(), "/home/user".into())
        }
    }

    #[test]
    fn plain_and_empty_text_filter_known_folders() {
        let (base, home) = roots();
        for (input, expected) in [
            ("", ""),
            ("  \t", ""),
            (" Repo One ", "Repo One"),
            ("~friend", "~friend"),
            (".hidden", ".hidden"),
        ] {
            assert_eq!(
                query(input, &base, Some(&home)),
                FolderQuery::Known(expected.into()),
                "{input:?}"
            );
        }
    }

    #[test]
    fn explicit_local_paths_resolve_against_supplied_base_or_home() {
        let (base, home) = roots();
        let absolute = if cfg!(windows) {
            "E:/some/folder"
        } else {
            "/some/folder"
        };
        for (input, expected) in [
            (absolute, PathBuf::from(absolute)),
            (".", base.clone()),
            ("..", base.join("..")),
            ("./child", base.join("child")),
            ("nested/Child Folder", base.join("nested/Child Folder")),
            ("~", home.clone()),
            ("~/", home.clone()),
            ("~/nested/child", home.join("nested/child")),
            ("~friend/child", base.join("~friend/child")),
        ] {
            assert_eq!(
                query(input, &base, Some(&home)),
                FolderQuery::LocalPath(expected),
                "{input:?}"
            );
        }
        for input in ["~", "~/child"] {
            assert_eq!(query(input, &base, None), FolderQuery::Invalid);
        }
        assert_eq!(
            query("a/b", Path::new("relative-base"), None),
            FolderQuery::Invalid
        );
        assert_eq!(
            query("~", &base, Some(Path::new("relative-home"))),
            FolderQuery::Invalid
        );
    }

    #[test]
    fn explicit_paths_preserve_literal_spaces() {
        let (base, home) = roots();
        assert_eq!(
            query(" folder/child", &base, Some(&home)),
            FolderQuery::LocalPath(base.join(" folder/child"))
        );
        let trailing = query("./repo ", &base, None);
        if cfg!(windows) {
            assert_eq!(
                trailing,
                FolderQuery::Invalid,
                "Windows can strip trailing spaces"
            );
        } else {
            assert_eq!(trailing, FolderQuery::LocalPath(base.join("repo ")));
        }
    }

    #[test]
    fn exact_basename_precedes_an_older_prefix_match() {
        let (base, _) = roots();
        let older = base.join("repo-old");
        let exact = base.join("repo");
        assert_eq!(
            rank_known("repo", &[older.clone(), exact.clone()], 2),
            [exact, older]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_backslashes_are_literal() {
        let (base, home) = roots();
        assert_eq!(
            query(r"~\child", &base, Some(&home)),
            FolderQuery::Known(r"~\child".into())
        );
        assert_eq!(
            query(r"./a\b", &base, None),
            FolderQuery::LocalPath(base.join(r"a\b"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolves_native_separators_but_rejects_nonlocal_and_implicit_roots() {
        let (base, home) = roots();
        for (input, expected) in [
            (r".\child", base.join("child")),
            (r"~\child", home.join("child")),
        ] {
            assert_eq!(
                query(input, &base, Some(&home)),
                FolderQuery::LocalPath(expected)
            );
        }
        for input in [
            "C:",
            "C:relative",
            "C:some/folder",
            r"\child",
            "/child",
            r"\\server\share",
            "//server/share",
            r"\\?\C:\child",
            r"\\.\device",
            "~/D:child",
            "~/D:/child",
        ] {
            assert_eq!(
                query(input, &base, Some(&home)),
                FolderQuery::Invalid,
                "{input:?}"
            );
        }
        let unc = Path::new(r"\\server\share");
        assert_eq!(query("./child", unc, None), FolderQuery::Invalid);
        assert_eq!(query("~/child", &base, Some(unc)), FolderQuery::Invalid);
    }

    #[test]
    fn ranks_basename_then_full_path_and_retains_mru_ties() {
        let dirs = paths([
            "folder/app/nonmatch",
            "longer/older/Apple",
            "a/APP",
            "a/Appetite",
            "a/ample-pad",
            "a/mapple",
            "unrelated/dir",
        ]);
        assert_eq!(
            rank_known(" APP ", &dirs, 99),
            paths([
                "a/APP",
                "longer/older/Apple",
                "a/Appetite",
                "a/mapple",
                "a/ample-pad",
                "folder/app/nonmatch"
            ])
        );
        assert_eq!(
            rank_known("folder/app", &dirs, 99),
            paths(["folder/app/nonmatch"])
        );
        assert!(rank_known("zz", &dirs, 99).is_empty());
    }

    #[test]
    fn deduplicates_before_applying_limit_including_empty_query() {
        let dirs = paths([
            "long/project",
            "long/project",
            "short/project",
            "other/project",
        ]);
        assert_eq!(
            rank_known("", &dirs, 2),
            paths(["long/project", "short/project"])
        );
        assert_eq!(
            rank_known("project", &dirs, 3),
            paths(["long/project", "short/project", "other/project"])
        );
        assert!(rank_known("project", &dirs, 0).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn paths_with_identical_lossy_labels_keep_distinct_identity() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let a = PathBuf::from(OsString::from_vec(b"dir/proj\xff".to_vec()));
        let b = PathBuf::from(OsString::from_vec(b"dir/proj\xfe".to_vec()));
        assert_eq!(
            rank_known("proj", &[a.clone(), a.clone(), b.clone()], 2),
            [a, b]
        );
    }
}
