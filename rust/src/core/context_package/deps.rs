//! Depth-1 dependency resolution at install time (GH #727, Phase 3).
//!
//! A package may declare [`PackageDependency`] entries (SemVer ranges). On
//! `pack install` / `addon add`, the direct dependencies of the root package
//! are resolved against the registry index and installed alongside it — one
//! consent surface listing everything that will land.
//!
//! **Depth-1 is deliberate** (issue non-goal: no transitive graphs): only the
//! root's own dependencies resolve; a dependency's dependencies do not. That
//! keeps resolution O(deps), makes cycles impossible beyond self-reference
//! (which is refused), and keeps the consent prompt honest — nothing installs
//! that was not listed.
//!
//! Determinism: given the same registry index, resolution always picks the
//! **highest non-yanked version matching the range** — and repeated installs
//! short-circuit offline via the lockfile + local store (`already_satisfied`).

use super::manifest::PackageDependency;
use super::remote::{self, VersionInfo};

/// One resolved direct dependency, ready to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDep {
    /// Scoped name as declared (`@ns/name`).
    pub name: String,
    /// Registry namespace (without `@`).
    pub namespace: String,
    /// Bare package name (slug).
    pub slug: String,
    /// The picked version (highest non-yanked match of the range).
    pub version: String,
    /// Artifact hash from the registry index (verified again on download).
    pub artifact_sha256: String,
}

/// Resolve the direct, non-optional dependencies in `deps` (declared by the
/// root package whose scoped reference is `root_name`) against the registry at
/// `base`. Fails on: unscoped names, self-dependency, invalid ranges, and
/// ranges with no installable match — a partially-resolved install is worse
/// than a refused one.
///
/// `root_name` is the root's **scoped** reference (`Some("@ns/name")`) or
/// `None` when the install source cannot name one (see [`resolve_one`]).
///
/// Takes the dependency slice + root name directly (rather than a
/// `PackageManifest`) so every install source can resolve the same way — a
/// local `lean-ctx-addon.toml` carries its `[[dependencies]]` in
/// [`crate::core::addons::manifest::AddonManifest`], with no hosted
/// `PackageManifest` to key off (GH #727, Finding A).
pub fn resolve_dependencies(
    deps: &[PackageDependency],
    root_name: Option<&str>,
    base: &str,
    token: Option<&str>,
) -> Result<Vec<ResolvedDep>, String> {
    let mut resolved = Vec::new();
    for dep in deps {
        if dep.optional {
            continue;
        }
        resolved.push(resolve_one(root_name, dep, base, token)?);
    }
    Ok(resolved)
}

/// Resolve a single declared dependency against the registry index.
///
/// `root_name` is the root package's **scoped** reference (`@ns/name`) or
/// `None` when the install source cannot name one. Pass a scoped reference,
/// never a bare `[addon] name` slug: the self-dependency guard compares against
/// the scoped dependency name, so a bare slug can never match and would
/// silently disable the guard (GH #727, Finding A). A local
/// `lean-ctx-addon.toml` has only a bare slug — a self-reference is unnameable
/// there, so its caller passes `None` and the guard is vacuous by construction.
pub fn resolve_one(
    root_name: Option<&str>,
    dep: &PackageDependency,
    base: &str,
    token: Option<&str>,
) -> Result<ResolvedDep, String> {
    let Some(remote_ref) = remote::parse_remote_ref(&dep.name) else {
        return Err(format!(
            "dependency `{}` is not a scoped @ns/name reference — unresolvable",
            dep.name
        ));
    };
    if let Some(root) = root_name
        && dep.name.trim_start_matches('@') == root.trim_start_matches('@')
    {
        return Err(format!(
            "package depends on itself (`{}`) — refused",
            dep.name
        ));
    }
    let req = parse_version_req(&dep.version_req)
        .map_err(|e| format!("dependency `{}`: {e}", dep.name))?;

    let versions = remote::fetch_versions(base, &remote_ref.namespace, &remote_ref.name, token)
        .map_err(|e| format!("dependency `{}`: {e}", dep.name))?;
    let best = pick_highest_match(&versions, &req).ok_or_else(|| {
        format!(
            "dependency `{}`: no installable version matches `{}` (available: {})",
            dep.name,
            dep.version_req,
            versions
                .iter()
                .map(|v| v.version.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    Ok(ResolvedDep {
        name: dep.name.clone(),
        namespace: remote_ref.namespace,
        slug: remote_ref.name,
        version: best.version.clone(),
        artifact_sha256: best.artifact_sha256.clone(),
    })
}

/// Parse a SemVer range. An empty/`*` requirement means "any version".
pub fn parse_version_req(req: &str) -> Result<semver::VersionReq, String> {
    let trimmed = req.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return Ok(semver::VersionReq::STAR);
    }
    semver::VersionReq::parse(trimmed).map_err(|e| format!("invalid version range `{req}`: {e}"))
}

/// Highest non-yanked version matching `req`. Non-SemVer versions in the
/// index are skipped (they can never match a range).
pub fn pick_highest_match<'a>(
    versions: &'a [VersionInfo],
    req: &semver::VersionReq,
) -> Option<&'a VersionInfo> {
    versions
        .iter()
        .filter(|v| !v.yanked)
        .filter_map(|v| Some((semver::Version::parse(&v.version).ok()?, v)))
        .filter(|(parsed, _)| req.matches(parsed))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, v)| v)
}

/// Version of `name` pinned in the project lockfile, if any.
pub fn locked_version(name: &str, project_root: &std::path::Path) -> Option<String> {
    let lock = super::lockfile::load(project_root).ok()?;
    lock.packages
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.version.clone())
}

/// The resolved dependency when `name@version-satisfying-req` is already
/// pinned in the lockfile **and** present in the local store — the
/// offline-reproducible fast path: a second `pack install` touches no network
/// for satisfied dependencies.
///
/// Returns the full [`ResolvedDep`] (at the **locked** version, not a fresh
/// highest-match) so the install step can hand the exact same version to
/// `[mcp.env]` `{pack_dir:}` expansion — the wiring must point at the version
/// that actually landed on disk (GH #727, Finding B).
pub fn already_satisfied(
    project_root: &std::path::Path,
    registry: &super::registry::LocalRegistry,
    dep: &PackageDependency,
) -> Option<ResolvedDep> {
    let lock = super::lockfile::load(project_root).ok()?;
    let locked = lock.packages.iter().find(|p| p.name == dep.name)?;
    let req = parse_version_req(&dep.version_req).ok()?;
    let version = semver::Version::parse(&locked.version).ok()?;
    if !req.matches(&version) {
        return None;
    }
    let installed = registry.get(&dep.name, Some(&locked.version)).ok()??;
    let remote_ref = remote::parse_remote_ref(&dep.name)?;
    Some(ResolvedDep {
        name: dep.name.clone(),
        namespace: remote_ref.namespace,
        slug: remote_ref.name,
        version: installed.version,
        artifact_sha256: locked.artifact_sha256.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(version: &str, yanked: bool) -> VersionInfo {
        VersionInfo {
            version: version.into(),
            artifact_sha256: "a".repeat(64),
            yanked,
        }
    }

    #[test]
    fn picks_highest_matching_version() {
        let versions = [v("1.0.0", false), v("1.2.0", false), v("2.0.0", false)];
        let req = parse_version_req("^1.0").unwrap();
        assert_eq!(
            pick_highest_match(&versions, &req).unwrap().version,
            "1.2.0"
        );
    }

    #[test]
    fn yanked_versions_never_match() {
        let versions = [v("1.0.0", false), v("1.3.0", true)];
        let req = parse_version_req("^1.0").unwrap();
        assert_eq!(
            pick_highest_match(&versions, &req).unwrap().version,
            "1.0.0"
        );
    }

    #[test]
    fn no_match_yields_none() {
        let versions = [v("1.0.0", false)];
        let req = parse_version_req("^2.0").unwrap();
        assert!(pick_highest_match(&versions, &req).is_none());
    }

    #[test]
    fn star_and_empty_match_anything() {
        let versions = [v("0.3.7", false)];
        for raw in ["", "*", "  "] {
            let req = parse_version_req(raw).unwrap();
            assert_eq!(
                pick_highest_match(&versions, &req).unwrap().version,
                "0.3.7",
                "req `{raw}`"
            );
        }
    }

    #[test]
    fn non_semver_index_entries_are_skipped() {
        let versions = [v("not-a-version", false), v("1.1.0", false)];
        let req = parse_version_req("^1").unwrap();
        assert_eq!(
            pick_highest_match(&versions, &req).unwrap().version,
            "1.1.0"
        );
    }

    #[test]
    fn invalid_range_is_an_error() {
        assert!(parse_version_req(">>nope<<").is_err());
    }

    #[test]
    fn self_dependency_is_refused() {
        let mut manifest = crate::core::context_package::manifest::PackageManifest {
            dependencies: vec![PackageDependency {
                name: "@acme/root".into(),
                version_req: "^1".into(),
                optional: false,
            }],
            ..minimal("@acme/root")
        };
        // resolve_one is exercised via resolve_dependencies; the self-check
        // fires before any network I/O, so an invalid base URL never matters.
        let err = resolve_dependencies(
            &manifest.dependencies,
            Some(&manifest.name),
            "http://127.0.0.1:1",
            None,
        )
        .unwrap_err();
        assert!(err.contains("depends on itself"), "got: {err}");

        // Optional dependencies are skipped entirely.
        manifest.dependencies[0].optional = true;
        assert_eq!(
            resolve_dependencies(
                &manifest.dependencies,
                Some(&manifest.name),
                "http://127.0.0.1:1",
                None
            )
            .unwrap(),
            Vec::new()
        );
    }

    /// The **addon** install path (GH #727, Finding A) resolves self-dependency
    /// against the addon's *scoped* self-reference (`@ns/slug`), which
    /// [`crate::cli::addon_cmd::addon_self_ref`] derives from the install
    /// source — never the bare `[addon] name` slug that the pre-fix code
    /// passed. The existing `self_dependency_is_refused` goes through
    /// `PackageManifest.name` (already scoped) and so never covered this path.
    ///
    /// This is the regression guard: with the scoped root the guard fires
    /// before any network I/O; with the pre-fix bare slug it never fires (the
    /// bare slug cannot equal a scoped `@ns/name` dependency), so resolution
    /// falls through to the network instead of refusing.
    #[test]
    fn addon_scoped_self_dependency_is_refused() {
        let dep = PackageDependency {
            name: "@dasTholo/demo".into(),
            version_req: "^0.2".into(),
            optional: false,
        };

        // Fixed addon path: scoped self-ref → refused before any network I/O
        // (the bad base URL is never reached).
        let err =
            resolve_one(Some("@dasTholo/demo"), &dep, "http://127.0.0.1:1", None).unwrap_err();
        assert!(err.contains("depends on itself"), "got: {err}");

        // Regression witness: the pre-fix bare slug (`[addon] name` = "demo")
        // does NOT trip the guard — proving why a bare name must never be
        // passed as the root. Resolution proceeds to the (refused) network, so
        // the error is anything BUT "depends on itself".
        let err_bare = resolve_one(Some("demo"), &dep, "http://127.0.0.1:1", None).unwrap_err();
        assert!(
            !err_bare.contains("depends on itself"),
            "bare slug must not trip the self-guard; got: {err_bare}"
        );

        // A source with no namespace (`None`) is vacuous by construction — same
        // fall-through, never a false self-match.
        let err_none = resolve_one(None, &dep, "http://127.0.0.1:1", None).unwrap_err();
        assert!(
            !err_none.contains("depends on itself"),
            "None root must not trip the self-guard; got: {err_none}"
        );
    }

    #[test]
    fn unscoped_dependency_is_refused() {
        let manifest = crate::core::context_package::manifest::PackageManifest {
            dependencies: vec![PackageDependency {
                name: "plain-name".into(),
                version_req: "^1".into(),
                optional: false,
            }],
            ..minimal("@acme/root")
        };
        let err = resolve_dependencies(
            &manifest.dependencies,
            Some(&manifest.name),
            "http://127.0.0.1:1",
            None,
        )
        .unwrap_err();
        assert!(err.contains("not a scoped"), "got: {err}");
    }

    #[test]
    fn already_satisfied_returns_the_locked_version_as_a_resolved_dep() {
        use crate::core::context_package::lockfile::{self, LockedPackage};

        let store = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let registry = super::super::registry::LocalRegistry::open_at(store.path()).unwrap();

        // The pack is installed on disk at the older, in-range 0.2.0…
        let mut manifest = minimal("@ns/skills");
        manifest.version = "0.2.0".into();
        registry
            .install(&manifest, &super::super::content::PackageContent::default())
            .unwrap();

        // …and the lockfile pins exactly that version.
        lockfile::upsert(
            proj.path(),
            LockedPackage {
                name: "@ns/skills".into(),
                version: "0.2.0".into(),
                artifact_sha256: "a".repeat(64),
                registry: "https://example.test".into(),
            },
        )
        .unwrap();

        let dep = PackageDependency {
            name: "@ns/skills".into(),
            version_req: "^0.2".into(),
            optional: false,
        };
        let resolved =
            already_satisfied(proj.path(), &registry, &dep).expect("locked + present on disk");

        // Regression (GH #727, Finding B): the version the env path burns in must
        // be the LOCKED/on-disk one — never a fresh highest-match resolve that
        // could point `{pack_dir:}` at a directory that does not exist.
        assert_eq!(resolved.version, "0.2.0");
        assert_eq!(resolved.name, "@ns/skills");
        assert_eq!(resolved.namespace, "ns");
        assert_eq!(resolved.slug, "skills");
    }

    fn minimal(name: &str) -> crate::core::context_package::manifest::PackageManifest {
        use crate::core::context_package::manifest::*;
        PackageManifest {
            schema_version: crate::core::contracts::CONTEXT_PACKAGE_V2_SCHEMA_VERSION,
            conformance_level: None,
            kind: PackageKind::default(),
            name: name.into(),
            version: "1.0.0".into(),
            description: "d".into(),
            author: None,
            scope: None,
            created_at: chrono::Utc::now(),
            updated_at: None,
            layers: vec![],
            dependencies: vec![],
            tags: vec![],
            visibility: None,
            integrity: PackageIntegrity {
                sha256: "a".repeat(64),
                content_hash: "b".repeat(64),
                byte_size: 1,
            },
            provenance: PackageProvenance {
                tool: "lean-ctx".into(),
                tool_version: "0".into(),
                project_hash: None,
                source_session_id: None,
            },
            compatibility: CompatibilitySpec::default(),
            stats: PackageStats::default(),
            signature: None,
            graph_summary: None,
            marketplace: None,
        }
    }
}
