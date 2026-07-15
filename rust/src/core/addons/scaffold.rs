//! `lean-ctx addon init` scaffolding (P4 — lower the floor).
//!
//! Generates a ready-to-edit `lean-ctx-addon.toml` so an author starts from a
//! valid, secure-by-default manifest instead of a blank file. The output always
//! parses, validates and passes [`super::audit`] cleanly — guarded by a test —
//! so `addon init` → `addon audit` → `addon add ./lean-ctx-addon.toml` works end
//! to end on a fresh scaffold.

use crate::core::addons::trust;
use crate::core::mcp_catalog::TransportKind;

/// The manifest filename an addon ships and `addon add <path>` expects.
pub const MANIFEST_FILENAME: &str = "lean-ctx-addon.toml";

/// Render a starter `lean-ctx-addon.toml` for `slug` and `transport`. Pure: the
/// caller decides where (and whether) to write it.
///
/// `command` (stdio only) is the optional `--command` argv the author passed
/// (`["npx", "-y", "pkg@1.2.3"]`): when given it wires that command verbatim and
/// picks capabilities that actually let it run — a package runner (`npx`/`npm`/…)
/// gets `network = full` + `filesystem = read_write` so the scaffold is not
/// silently sandbox-blocked at install (GH #1079). Without a command, the
/// scaffold keeps the secure-by-default profile for a local binary.
#[must_use]
pub fn addon_manifest(slug: &str, transport: TransportKind, command: Option<&[String]>) -> String {
    let display = title_case(slug);
    let (wiring, capabilities) = match transport {
        TransportKind::Stdio => stdio_sections(slug, command),
        TransportKind::Http => (http_wiring(), http_capabilities()),
    };

    format!(
        "# lean-ctx addon manifest — see docs/guides/addons.md\n\
         # Validate before publishing:  lean-ctx addon audit ./{MANIFEST_FILENAME}\n\
         \n\
         [addon]\n\
         name = \"{slug}\"\n\
         display_name = \"{display}\"\n\
         description = \"One line describing what this addon does.\"\n\
         version = \"0.1.0\"\n\
         author = \"\"                  # your name or org (required to get listed)\n\
         homepage = \"\"                # repo / homepage URL (required to get listed)\n\
         license = \"Apache-2.0\"\n\
         categories = [\"workflow\"]\n\
         keywords = []\n\
         \n\
         {wiring}\
         {capabilities}"
    )
}

/// The `[mcp]` + `[capabilities]` sections for a stdio addon. Splits cleanly so
/// the package-runner capability choice stays in one place.
fn stdio_sections(slug: &str, command: Option<&[String]>) -> (String, String) {
    let (cmd, args): (String, Vec<String>) = match command {
        Some(argv) if !argv.is_empty() => (argv[0].clone(), argv[1..].to_vec()),
        _ => (format!("{slug}-mcp"), vec!["serve".to_string()]),
    };
    let wiring = format!(
        "[mcp]\n\
         transport = \"stdio\"\n\
         command = \"{cmd}\"\n\
         args = {args}\n\
         # env = {{ MY_TOKEN = \"...\" }}  # extra child-process env (avoid secrets here)\n\
         # sha256 = \"<shasum -a 256 {cmd}>\"  # pin the binary (required for verified/paid)\n",
        args = render_args(&args),
    );

    let capabilities = if trust::command_is_package_runner(&cmd) {
        // npx/npm/uvx fetch + execute a package: they need outbound network and
        // a writable package cache, so the secure default (network = none /
        // read_only) would make the OS sandbox block the spawn (GH #1079).
        "\n\
         [capabilities]\n\
         network = \"full\"          # package runner fetches from a registry\n\
         filesystem = \"read_write\"  # ...and writes to its package cache\n\
         env = []                   # host env var names your tool may receive\n\
         exec = \"none\"             # or [\"lean-ctx\"] if you spawn subprocesses\n"
            .to_string()
    } else {
        // Secure-by-default for a local binary: no network, read-only fs.
        "\n\
         [capabilities]\n\
         network = \"none\"          # \"full\" if your tool reaches the internet (e.g. npx/npm fetch a package)\n\
         filesystem = \"read_only\"  # \"read_write\" if it writes outside a scratch tmp\n\
         env = []                   # host env var names your tool may receive\n\
         exec = \"none\"             # or [\"lean-ctx\"] if you spawn subprocesses (e.g. call back into lean-ctx)\n"
            .to_string()
    };
    (wiring, capabilities)
}

fn http_wiring() -> String {
    "[mcp]\n\
     transport = \"http\"\n\
     url = \"https://your-service.example/mcp\"   # streamable-HTTP MCP endpoint\n\
     # headers = { Authorization = \"Bearer ...\" }\n"
        .to_string()
}

fn http_capabilities() -> String {
    // An HTTP addon inherently uses the network; declaring it keeps the audit
    // coherent.
    "\n\
     [capabilities]\n\
     network = \"full\"          # an HTTP endpoint inherently uses the network\n"
        .to_string()
}

/// Render an args list as a TOML array literal (`["-y", "pkg@1.2.3"]`).
fn render_args(args: &[String]) -> String {
    if args.is_empty() {
        return "[]".to_string();
    }
    let quoted: Vec<String> = args
        .iter()
        .map(|a| format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// A slug derived from `name` (or a directory name): lowercase, non-alnum → `-`,
/// collapsed and trimmed. Returns `None` if nothing usable remains.
#[must_use]
pub fn slugify(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let slug = out.trim_end_matches('-').to_string();
    (!slug.is_empty()).then_some(slug)
}

fn title_case(slug: &str) -> String {
    slug.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map_or_else(String::new, |f| {
                f.to_ascii_uppercase().to_string() + c.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::addons::audit::{self, AuditVerdict};
    use crate::core::addons::manifest::AddonManifest;

    #[test]
    fn scaffold_stdio_is_valid_and_audits_clean() {
        let toml = addon_manifest("my-tool", TransportKind::Stdio, None);
        let m = AddonManifest::from_toml(&toml).expect("scaffold parses");
        m.validate().expect("scaffold validates");
        assert!(m.is_installable(), "stdio scaffold is installable");
        let report = audit::audit(&m);
        assert_eq!(report.verdict, AuditVerdict::Pass, "{:?}", report.findings);
        assert!(report.capability_coherent);
    }

    #[test]
    fn scaffold_npx_command_gets_network_capabilities() {
        // GH #1079: an `addon init --command "npx -y pkg@1.2.3"` scaffold must
        // declare the network + filesystem the runner needs, so it is coherent
        // (not under-declared) and won't be sandbox-blocked at install.
        let argv = vec![
            "npx".to_string(),
            "-y".to_string(),
            "@scope/pkg@1.2.3".to_string(),
        ];
        let toml = addon_manifest("my-tool", TransportKind::Stdio, Some(&argv));
        assert!(toml.contains("command = \"npx\""));
        assert!(toml.contains("\"@scope/pkg@1.2.3\""));
        assert!(toml.contains("network = \"full\""));
        assert!(toml.contains("filesystem = \"read_write\""));
        let m = AddonManifest::from_toml(&toml).expect("scaffold parses");
        m.validate().expect("scaffold validates");
        assert!(m.is_installable());
        let report = audit::audit(&m);
        assert!(
            report.capability_coherent,
            "npx scaffold declares the caps it needs: {:?}",
            report.findings
        );
    }

    #[test]
    fn scaffold_http_is_valid_and_coherent() {
        let toml = addon_manifest("remote-svc", TransportKind::Http, None);
        let m = AddonManifest::from_toml(&toml).expect("scaffold parses");
        m.validate().expect("scaffold validates");
        let report = audit::audit(&m);
        assert!(
            report.capability_coherent,
            "http + network=full is coherent"
        );
        // HTTP endpoint is high-capability → review, never a fail.
        assert_ne!(report.verdict, AuditVerdict::Fail);
    }

    #[test]
    fn slugify_normalizes() {
        assert_eq!(slugify("My Cool Addon").as_deref(), Some("my-cool-addon"));
        assert_eq!(slugify("  weird__name!! ").as_deref(), Some("weird-name"));
        assert_eq!(slugify("already-good").as_deref(), Some("already-good"));
        assert_eq!(slugify("***").as_deref(), None);
    }

    #[test]
    fn slug_roundtrips_through_manifest_validation() {
        let slug = slugify("Acme Plans").unwrap();
        let m =
            AddonManifest::from_toml(&addon_manifest(&slug, TransportKind::Stdio, None)).unwrap();
        assert_eq!(m.addon.name, "acme-plans");
        assert_eq!(m.display_name(), "Acme Plans");
    }
}
