//! Virtual path normalization: a guest path made root-relative and
//! `/`-separated, with `.` and `..` resolved.
//!
//! [`normalize_vpath`] walks `..` back up the path and fails with
//! [`PathError::EscapesRoot`] if it climbs past the root: it is for paths that
//! arrive from the guest and must name something inside a root. It accepts
//! `/` and `\` as separators.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    EscapesRoot,
    /// The path normalizes to the root itself (zero components) where a
    /// non-root path was required — e.g. a declared managed root, which must
    /// name a real subtree rather than "everything". See
    /// `RootMap::with_capacity`, which is the caller that
    /// matters: a root with zero components would match every path handed to
    /// it, with the whole path left over as the remainder.
    EmptyRoot,
}

/// Normalize a root-relative virtual path to canonical `/`-separated form,
/// resolving `..`.
/// `""` denotes the root. Deeper NT concerns (`\Device\…`, RootDirectory-relative
/// opens, 8.3 short names) are edge/shim concerns and out of scope here.
pub fn normalize_vpath(raw: &str) -> Result<String, PathError> {
    // Strip known NT/DOS long-path prefixes first (either slash form).
    let mut s = raw;
    for prefix in [r"\??\", r"\\?\", "/??/", "//?/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }

    let mut out = String::with_capacity(s.len());
    for comp in s.split(['/', '\\']) {
        match comp {
            "" | "." => continue,
            ".." => {
                if !pop_component(&mut out) {
                    return Err(PathError::EscapesRoot);
                }
            }
            other => push_component(&mut out, other),
        }
    }
    Ok(out)
}

/// Append `comp` to the `/`-joined `out`. With [`pop_component`], the stack
/// of components `..` walks back up, kept in one string sized for the input:
/// a path costs one allocation however many components it has.
pub(crate) fn push_component(out: &mut String, comp: &str) {
    if !out.is_empty() {
        out.push('/');
    }
    out.push_str(comp);
}

/// Drop the last component of the `/`-joined `out`; `false` if it had none.
pub(crate) fn pop_component(out: &mut String) -> bool {
    if out.is_empty() {
        return false;
    }
    let keep = out.rfind('/').unwrap_or(0);
    out.truncate(keep);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_separators_and_trims() {
        assert_eq!(
            normalize_vpath("data\\meshes\\a.nif").unwrap(),
            "data/meshes/a.nif"
        );
        assert_eq!(normalize_vpath("/data/").unwrap(), "data");
        assert_eq!(normalize_vpath("data//meshes").unwrap(), "data/meshes");
    }

    #[test]
    fn empty_and_dot_are_root() {
        assert_eq!(normalize_vpath("").unwrap(), "");
        assert_eq!(normalize_vpath(".").unwrap(), "");
        assert_eq!(normalize_vpath("/").unwrap(), "");
    }

    #[test]
    fn resolves_dotdot() {
        assert_eq!(normalize_vpath("data/x/../y").unwrap(), "data/y");
        assert_eq!(normalize_vpath("a/b/../..").unwrap(), "");
    }

    #[test]
    fn dotdot_escaping_root_errors() {
        assert_eq!(normalize_vpath("..").unwrap_err(), PathError::EscapesRoot);
        assert_eq!(
            normalize_vpath("data/../..").unwrap_err(),
            PathError::EscapesRoot
        );
    }

    #[test]
    fn strips_nt_and_dos_prefixes() {
        assert_eq!(normalize_vpath(r"\??\data\a").unwrap(), "data/a");
        assert_eq!(normalize_vpath(r"\\?\data\a").unwrap(), "data/a");
    }
}
