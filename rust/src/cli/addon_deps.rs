//! Dependency resolution and installation for the addon `add`/`update` paths
//! (GH #727): a declared depth-1 dependency is part of the install consent
//! surface, so it must be previewed before the user is asked and installed
//! before anything is wired. Split out of `addon_cmd` to keep that file under
//! the LOC gate (`scripts/loc-gate.sh`, limit 1500 lines).

/// The addon's own **scoped** package reference (`@ns/slug`), derived from the
/// recorded install `source`. This is the self-dependency root handed to the
/// depth-1 resolver (GH #727, Finding A): a declared dependency whose name
/// equals it is the addon depending on its own pack and is refused.
///
/// Only a hosted `ctxpkg:@ns/slug@ver` source carries a namespace. A local
/// manifest (`"local"`) or a bundled-registry slug (`"registry"`) has none, so
/// a self-reference is unnameable and the guard is deliberately vacuous
/// (`None`) — never the bare `[addon] name` slug, which could never equal a
/// scoped `@ns/name` dependency and would only give the guard the false
/// appearance of being active.
pub(super) fn addon_self_ref(source: &str) -> Option<String> {
    let spec = source.strip_prefix("ctxpkg:")?;
    let remote_ref = crate::core::context_package::remote::parse_remote_ref(spec)?;
    Some(format!("@{}/{}", remote_ref.namespace, remote_ref.name))
}

/// Re-resolve and install the declared dependencies of an addon (GH #727) —
/// used on `addon update`, where a dependency can move forward independently of
/// the addon binary.
pub(super) fn refresh_pack_dependencies(
    deps: &[crate::core::context_package::manifest::PackageDependency],
    root_name: Option<&str>,
    args: &[String],
) {
    if deps.iter().all(|d| d.optional) {
        return;
    }
    let base = crate::core::context_package::remote::registry_base(
        super::addon_cmd::flag_value(args, "--registry").as_deref(),
    );
    let token = crate::core::context_package::remote::publish_token(None);
    let project_root = super::common::detect_project_root(args);
    println!("Refreshing declared dependencies (depth-1) …");
    if let Err(e) = super::pack_remote::install_declared_dependencies(
        deps,
        root_name,
        &base,
        token.as_deref(),
        &project_root,
        true,
    ) {
        eprintln!("Warning: dependency refresh failed: {e}\n  The addon update itself succeeded.");
    }
}

/// Resolve the declared depth-1 dependencies of an addon so the caller can
/// show them in the consent preview (GH #727). Exits on a resolution failure —
/// nothing has been touched at this point.
///
/// This is a **preview only**: it picks the highest in-range version and does
/// not consult `ctxpkg.lock`. The authoritative versions that get wired into
/// `[mcp.env]` come from [`install_declared_deps`] (which honours the lockfile),
/// so in the rare case where the lockfile pins an older in-range version this
/// preview may list a newer version than the install actually lands. The wiring
/// is always correct; only the pre-consent print can differ.
pub(super) fn resolve_declared_deps(
    deps: &[crate::core::context_package::manifest::PackageDependency],
    root_name: Option<&str>,
    args: &[String],
) -> Vec<crate::core::context_package::deps::ResolvedDep> {
    if deps.iter().all(|d| d.optional) {
        return Vec::new();
    }
    let base = crate::core::context_package::remote::registry_base(
        super::addon_cmd::flag_value(args, "--registry").as_deref(),
    );
    let token = crate::core::context_package::remote::publish_token(None);
    match crate::core::context_package::deps::resolve_dependencies(
        deps,
        root_name,
        &base,
        token.as_deref(),
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

/// Install the declared dependencies **before** anything is wired and return
/// the [`ResolvedDep`]s that actually landed (locked versions honoured). A path
/// burned into `[mcp.env]` must exist before it is burned in, so a failed
/// dependency install wires nothing at all — and the wiring expands
/// `{pack_dir:}` against exactly this returned slice (GH #727, Finding B).
pub(super) fn install_declared_deps(
    deps: &[crate::core::context_package::manifest::PackageDependency],
    root_name: Option<&str>,
    args: &[String],
) -> Vec<crate::core::context_package::deps::ResolvedDep> {
    let base = crate::core::context_package::remote::registry_base(
        super::addon_cmd::flag_value(args, "--registry").as_deref(),
    );
    let token = crate::core::context_package::remote::publish_token(None);
    let project_root = super::common::detect_project_root(args);
    match super::pack_remote::install_declared_dependencies(
        deps,
        root_name,
        &base,
        token.as_deref(),
        &project_root,
        false,
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: dependency install failed: {e}\n  Nothing was wired.");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pure part of the addon install path that GH #727 Finding A got
    /// wrong: deriving the self-dependency root from the recorded install
    /// source. A hosted `ctxpkg:@ns/slug@ver` source yields the scoped
    /// `@ns/slug` the resolver's guard compares against; a `local` manifest or
    /// a bundled `registry` slug has no namespace, so the guard is deliberately
    /// vacuous (`None`) — the pre-fix code instead passed the bare `addon.name`
    /// slug, which could never match a scoped dependency and silently disabled
    /// the guard on this path.
    #[test]
    fn addon_self_ref_is_scoped_for_hosted_sources_and_none_otherwise() {
        // Hosted pack: scoped `@ns/slug`, version pin stripped.
        assert_eq!(
            addon_self_ref("ctxpkg:@dasTholo/lean-md@0.2.0").as_deref(),
            Some("@dasTholo/lean-md")
        );
        assert_eq!(
            addon_self_ref("ctxpkg:@dasTholo/lean-md").as_deref(),
            Some("@dasTholo/lean-md")
        );

        // No namespace ⇒ self-reference unnameable ⇒ guard vacuous.
        assert_eq!(addon_self_ref("local"), None);
        assert_eq!(addon_self_ref("registry"), None);
        // A bare slug is never returned, so the bug (bare root) is unreachable.
        assert_eq!(addon_self_ref("ctxpkg:demo"), None);
    }
}
