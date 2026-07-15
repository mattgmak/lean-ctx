//! `{pack_dir:@ns/name}` expansion for an addon's `[mcp.env]` (GH #727).
//!
//! An addon that ships its content in a `kind=skills` pack must learn where
//! that pack was materialized. The author names the variable and states which
//! pack it refers to; lean-ctx expands the placeholder at wiring time against
//! the resolved dependency version:
//!
//! ```toml
//! [mcp.env]
//! LEAN_MD_SKILLS_DIR = "{pack_dir:@dasTholo/lean-md-skills}"
//! ```
//!
//! Pure by construction: the store root is a parameter, never read from the
//! environment here, so the expansion is a deterministic function of
//! (declared env, resolved deps, store root).
//!
//! No env-scrub change is required. [`super::env_scrub::apply_env`] applies the
//! declared env *after* `env_clear()`, so an expanded value reaches the child
//! without an allowlist entry — lean-ctx computed this value, it is not a host
//! variable smuggled through.

use std::collections::BTreeMap;
use std::path::Path;

use crate::core::context_package::deps::ResolvedDep;
use crate::core::context_package::skills::skills_dir;

const SCHEME: &str = "pack_dir:";

/// Every pack name referenced by a `{pack_dir:…}` placeholder in `value`.
///
/// A `{` always opens a placeholder — there is no literal-brace escape. An
/// unterminated brace, or a `{…}` whose body is not `pack_dir:<name>`, is a
/// hard error: a typo must never survive as an env value with braces in it.
pub fn referenced_packs(value: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| format!("unterminated `{{` in `{value}`"))?;
        let body = &after[..close];
        let name = body.strip_prefix(SCHEME).ok_or_else(|| {
            format!(
                "unknown placeholder `{{{body}}}` in `{value}` — only \
                 `{{pack_dir:@ns/name}}` is supported"
            )
        })?;
        if name.trim().is_empty() {
            return Err(format!("empty pack name in `{value}`"));
        }
        names.push(name.to_string());
        rest = &after[close + 1..];
    }
    Ok(names)
}

/// Expand every `{pack_dir:@ns/name}` in `declared_env` against `resolved`.
///
/// A placeholder naming a pack that is not a resolved dependency is a hard
/// error; a value without a placeholder passes through unchanged.
pub fn expand_pack_env(
    declared_env: &BTreeMap<String, String>,
    resolved: &[ResolvedDep],
    store_root: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (key, value) in declared_env {
        let names = referenced_packs(value).map_err(|e| format!("[mcp.env] `{key}`: {e}"))?;
        if names.is_empty() {
            out.insert(key.clone(), value.clone());
            continue;
        }
        let mut expanded = value.clone();
        for name in names {
            let dep = resolved.iter().find(|d| d.name == name).ok_or_else(|| {
                format!(
                    "[mcp.env] `{key}`: `{{pack_dir:{name}}}` names a pack that is not a \
                     declared dependency"
                )
            })?;
            let dir = skills_dir(store_root, &dep.name, &dep.version);
            expanded = expanded.replace(&format!("{{{SCHEME}{name}}}"), &dir.display().to_string());
        }
        out.insert(key.clone(), expanded);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(name: &str, version: &str) -> ResolvedDep {
        let bare = name.trim_start_matches('@');
        let (ns, slug) = bare.split_once('/').expect("scoped name");
        ResolvedDep {
            name: name.to_string(),
            namespace: ns.to_string(),
            slug: slug.to_string(),
            version: version.to_string(),
            artifact_sha256: "a".repeat(64),
        }
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn one_placeholder_expands_to_the_skills_dir() {
        let root = Path::new("/store");
        let deps = [dep("@dasTholo/lean-md-skills", "0.2.0")];
        let out = expand_pack_env(
            &env(&[("LEAN_MD_SKILLS_DIR", "{pack_dir:@dasTholo/lean-md-skills}")]),
            &deps,
            root,
        )
        .expect("expands");
        // Segment names are explicit here (only the separator comes from the
        // platform): `Path::join` is production's own separator, so this
        // asserts the real invariant instead of duplicating the code under test.
        let expected = Path::new("/store")
            .join("skills")
            .join("@dasTholo__lean-md-skills")
            .join("0.2.0")
            .display()
            .to_string();
        assert_eq!(out["LEAN_MD_SKILLS_DIR"], expected);
    }

    #[test]
    fn two_dependencies_two_variables_both_expand() {
        let root = Path::new("/store");
        let deps = [dep("@ns/one", "1.0.0"), dep("@ns/two", "2.3.4")];
        let out = expand_pack_env(
            &env(&[
                ("ONE_DIR", "{pack_dir:@ns/one}"),
                ("TWO_DIR", "{pack_dir:@ns/two}"),
            ]),
            &deps,
            root,
        )
        .expect("expands");
        let expected_one = Path::new("/store")
            .join("skills")
            .join("@ns__one")
            .join("1.0.0")
            .display()
            .to_string();
        let expected_two = Path::new("/store")
            .join("skills")
            .join("@ns__two")
            .join("2.3.4")
            .display()
            .to_string();
        assert_eq!(out["ONE_DIR"], expected_one);
        assert_eq!(out["TWO_DIR"], expected_two);
    }

    #[test]
    fn placeholder_naming_an_unknown_pack_is_an_error() {
        let deps = [dep("@ns/one", "1.0.0")];
        let err = expand_pack_env(
            &env(&[("D", "{pack_dir:@ns/other}")]),
            &deps,
            Path::new("/store"),
        )
        .expect_err("unknown pack");
        assert!(err.contains("not a declared dependency"), "{err}");
    }

    #[test]
    fn unknown_placeholder_scheme_is_an_error() {
        let err = expand_pack_env(
            &env(&[("D", "{bin_dir:@ns/one}")]),
            &[],
            Path::new("/store"),
        )
        .expect_err("unknown scheme");
        assert!(err.contains("unknown placeholder"), "{err}");
    }

    #[test]
    fn value_without_a_placeholder_passes_through_unchanged() {
        let out = expand_pack_env(
            &env(&[("PLAIN", "/etc/passwd"), ("EMPTY", "")]),
            &[],
            Path::new("/store"),
        )
        .expect("passes through");
        assert_eq!(out["PLAIN"], "/etc/passwd");
        assert_eq!(out["EMPTY"], "");
    }

    /// Incidental braces are an ERROR, not a pass-through: a `{` opens a
    /// placeholder unconditionally, so a typo fails loudly at manifest parse
    /// instead of reaching the child as a literal `{…}` env value.
    #[test]
    fn incidental_braces_are_an_error() {
        let unterminated =
            expand_pack_env(&env(&[("D", "a{b")]), &[], Path::new("/store")).expect_err("open");
        assert!(unterminated.contains("unterminated"), "{unterminated}");

        let bare =
            expand_pack_env(&env(&[("D", "{HOME}")]), &[], Path::new("/store")).expect_err("bare");
        assert!(bare.contains("unknown placeholder"), "{bare}");

        let empty = expand_pack_env(&env(&[("D", "{pack_dir:}")]), &[], Path::new("/store"))
            .expect_err("empty");
        assert!(empty.contains("empty pack name"), "{empty}");
    }
}
