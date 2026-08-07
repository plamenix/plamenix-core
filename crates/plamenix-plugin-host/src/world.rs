//! WIT worlds as a thing the host actually knows about.
//!
//! `wit/plamenix.wit` declares five worlds, each bundling a capability
//! tier, and its header promises the host will:
//!
//! 1. refuse to instantiate a plugin whose declared world is unknown,
//! 2. link only the imports the declared world exposes,
//! 3. cross-check the manifest's capability grants against the world's
//!    available imports.
//!
//! None of that happened. `validate_world_identifier` checked the
//! *shape* of the identifier — colons, an `@`, a parseable `SemVer` — and
//! nothing checked the world name itself, so
//! `plamenix:plugin@1.0.0/plugin-does-not-exist` parsed cleanly and
//! failed later as a raw wasmtime linker error. Worse, a plugin could
//! declare `plugin-minimal` (whose whole point is that it imports
//! nothing but `host`) and request `db.write:execute`, and the host
//! would grant it. The Object Capability Model the WIT header claims
//! "by construction" was not constructed.
//!
//! This module is points 1 and 3. Point 2 is only partly reachable
//! today and the [`PluginWorld::linkable`] docs say why.

use serde::{Deserialize, Serialize};

use crate::capability::{Permission, PermissionSet};
use crate::error::PluginError;
use crate::manifest::Edition;

/// The WIT package every world in this contract belongs to.
const PACKAGE: &str = "plamenix:plugin";

/// The capability tier a plugin declared, resolved from its manifest's
/// `plugin.world`.
///
/// The variants are ordered by tier, and each includes the one before
/// it — exactly as the `include` chain in `wit/plamenix.wit` does.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginWorld {
    /// Tier 0 — logging and a version probe. Themes, tip packs, pure-UI.
    Minimal,
    /// Tier 1 — read-only database access.
    DbReader,
    /// Tier 2 — read + write database access.
    DbWriter,
    /// Tier 3 — fs, net, event bus, settings, commands, clipboard.
    Integrated,
    /// Tier 3 plus the desktop-only capabilities: keyring and notify.
    IntegratedDesktop,
}

impl PluginWorld {
    /// The world name as it appears after the `/` in a manifest.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Minimal => "plugin-minimal",
            Self::DbReader => "plugin-db-reader",
            Self::DbWriter => "plugin-db-writer",
            Self::Integrated => "plugin-integrated",
            Self::IntegratedDesktop => "plugin-integrated-desktop",
        }
    }

    /// Every world the contract declares, in tier order.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Minimal,
            Self::DbReader,
            Self::DbWriter,
            Self::Integrated,
            Self::IntegratedDesktop,
        ]
    }

    /// Whether this host build can actually instantiate a plugin
    /// targeting this world.
    ///
    /// Only `plugin-minimal` can, because a world is linkable when the
    /// host implements every import it exposes, and the only host
    /// import implemented so far is `host` itself — `db`, `db-write`,
    /// `fs`, `net`, `event-bus`, `settings`, `command`, `clipboard`,
    /// `notify`, and `keyring` are declared in the WIT and have no
    /// implementation behind them.
    ///
    /// This is deliberately explicit rather than left to fail at
    /// instantiation. A plugin declaring `plugin-db-reader` used to get
    /// a wasmtime linker error naming a missing import, which reads as
    /// a bug in the plugin; it is a gap in the host, and the refusal
    /// should say so.
    #[must_use]
    pub const fn linkable(self) -> bool {
        matches!(self, Self::Minimal)
    }

    /// Whether this world can only be satisfied by the desktop edition.
    #[must_use]
    pub const fn desktop_only(self) -> bool {
        matches!(self, Self::IntegratedDesktop)
    }

    /// Whether `permission` is within this world's import surface.
    ///
    /// A capability is only meaningful if the world exposes the import
    /// that would exercise it. Granting `db:query.read` to a plugin
    /// that cannot see the `db` interface is not a smaller grant — it
    /// is a grant the user was asked to approve for no reason, which
    /// is how consent dialogs become noise.
    #[must_use]
    pub const fn permits(self, permission: &Permission) -> bool {
        let tier = match permission {
            // `db` interface.
            Permission::DbReadAny
            | Permission::DbReadTable(_)
            | Permission::DbSchemaList
            | Permission::DbSchemaDescribe => Self::DbReader,
            // `db-write` interface.
            Permission::DbWriteAny
            | Permission::DbWriteTable(_)
            | Permission::DbDdlAny
            | Permission::DbDdlTable(_) => Self::DbWriter,
            // Contribution capabilities, not import-backed ones.
            // `export.format` and `import.source` say the plugin
            // *provides* something the host will call into; they are
            // delivered through the contribution registry and the UI
            // half, and no world imports them. A theme-tier plugin
            // contributing a Markdown export format is legitimate.
            Permission::ExportFormat | Permission::ImportSource => return true,
            // The tier-3 bundle: fs, net, settings, command, clipboard.
            Permission::NetHttps
            | Permission::NetHttpsHost(_)
            | Permission::NetHttp
            | Permission::FsReadDir(_)
            | Permission::FsWriteDir(_)
            | Permission::ClipboardRead
            | Permission::ClipboardWrite
            | Permission::OsOpenUrl => Self::Integrated,
            // Desktop-only orthogonal capabilities.
            Permission::AuthOsKeyring(_) | Permission::OsNotify => Self::IntegratedDesktop,
        };
        self.tier() >= tier.tier()
    }

    /// Position in the `include` chain. Higher includes lower.
    const fn tier(self) -> u8 {
        match self {
            Self::Minimal => 0,
            Self::DbReader => 1,
            Self::DbWriter => 2,
            Self::Integrated => 3,
            Self::IntegratedDesktop => 4,
        }
    }
}

/// Parses a fully qualified WIT world identifier of the shape
/// `<namespace>:<package>@<version>/<world>` — e.g.
/// `plamenix:plugin@1.0.0/plugin-minimal`.
///
/// Unlike the syntax check this replaces, the world name is resolved
/// against the worlds the contract actually declares, and the package
/// against the one this host implements.
///
/// # Errors
///
/// [`PluginError::InvalidManifest`] when any segment is missing or
/// malformed, when the package is not `plamenix:plugin`, or when the
/// world name is not one of the five declared worlds.
pub fn parse_world_identifier(world: &str) -> Result<PluginWorld, PluginError> {
    let invalid = |detail: String| PluginError::InvalidManifest(detail);

    let (qualified, world_name) = world.rsplit_once('/').ok_or_else(|| {
        invalid(format!(
            "plugin.world `{world}` missing `/<world-name>` suffix"
        ))
    })?;
    if world_name.is_empty() {
        return Err(invalid(format!(
            "plugin.world `{world}` has empty world name after `/`"
        )));
    }
    let (package, version) = qualified.rsplit_once('@').ok_or_else(|| {
        invalid(format!(
            "plugin.world `{world}` missing `@<version>` between package and world name"
        ))
    })?;
    if package.is_empty() || !package.contains(':') {
        return Err(invalid(format!(
            "plugin.world `{world}` package prefix must be `<namespace>:<package>`"
        )));
    }
    if package != PACKAGE {
        return Err(invalid(format!(
            "plugin.world `{world}` targets package `{package}`, but this host implements `{PACKAGE}`",
        )));
    }
    semver::Version::parse(version).map_err(|err| {
        invalid(format!(
            "plugin.world `{world}` version segment is not valid SemVer: {err}"
        ))
    })?;

    PluginWorld::all()
        .into_iter()
        .find(|candidate| candidate.name() == world_name)
        .ok_or_else(|| {
            let known = PluginWorld::all()
                .map(PluginWorld::name)
                .join(", ");
            invalid(format!(
                "plugin.world `{world}` names world `{world_name}`, which does not exist in `{PACKAGE}`. Known worlds: {known}",
            ))
        })
}

/// Refuses capabilities the declared world cannot exercise.
///
/// Checks required and optional permissions alike. An optional
/// permission outside the world is still a prompt the user would be
/// asked to approve for an import the plugin cannot reach.
///
/// # Errors
///
/// [`PluginError::InvalidManifest`] naming the first offending
/// capability and the world that would have to be declared instead.
pub fn check_permissions(
    world: PluginWorld,
    permissions: &PermissionSet,
) -> Result<(), PluginError> {
    let offender = permissions
        .required
        .iter()
        .chain(permissions.optional.iter())
        .map(|grant| &grant.capability)
        .find(|permission| !world.permits(permission));

    offender.map_or(Ok(()), |permission| {
        let needed = PluginWorld::all()
            .into_iter()
            .find(|candidate| candidate.permits(permission))
            .map_or_else(
                || "a world that exposes it".to_owned(),
                |candidate| format!("`{}`", candidate.name()),
            );
        Err(PluginError::InvalidManifest(format!(
            "plugin.world `{}` does not expose the import behind capability `{permission:?}`; declare {needed} instead",
            world.name(),
        )))
    })
}

/// Refuses a world this host build cannot link.
///
/// # Errors
///
/// [`PluginError::InvalidManifest`] when the world is declared in the
/// contract but its imports have no host implementation.
pub fn check_linkable(world: PluginWorld) -> Result<(), PluginError> {
    if world.linkable() {
        return Ok(());
    }
    Err(PluginError::InvalidManifest(format!(
        "plugin.world `{}` is declared in the contract but this host build implements only `{}` — the imports the higher tiers expose (db, fs, net, settings, command, clipboard, notify, keyring) have no host implementation yet",
        world.name(),
        PluginWorld::Minimal.name(),
    )))
}

/// Refuses a manifest whose declared world and declared editions
/// disagree.
///
/// `plugin-integrated-desktop` exists to reach the OS keyring and OS
/// notifications, neither of which the web edition has. A manifest that
/// declares it while also listing `web` in `plugin.targets` is asking
/// for something that cannot exist, and catching it here means the
/// author learns at validation time rather than from a user's install
/// failing on one edition only.
///
/// # Errors
///
/// [`PluginError::InvalidManifest`] when a desktop-only world is paired
/// with a non-desktop target.
pub fn check_targets(world: PluginWorld, targets: &[Edition]) -> Result<(), PluginError> {
    if !world.desktop_only() {
        return Ok(());
    }
    if targets.iter().any(|target| *target != Edition::Desktop) {
        return Err(PluginError::InvalidManifest(format!(
            "plugin.world `{}` reaches the OS keyring and OS notifications, so it cannot run on the web edition; plugin.targets must be [\"desktop\"] alone",
            world.name(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::capability::{LogicalDir, PermissionGrant};

    fn permissions(required: Vec<Permission>) -> PermissionSet {
        PermissionSet {
            required: required.into_iter().map(PermissionGrant::new).collect(),
            optional: Vec::new(),
        }
    }

    #[test]
    fn every_declared_world_resolves_by_name() {
        for world in PluginWorld::all() {
            let id = format!("plamenix:plugin@1.0.0/{}", world.name());
            assert_eq!(parse_world_identifier(&id).unwrap(), world);
        }
    }

    #[test]
    fn a_world_that_does_not_exist_is_refused_by_name() {
        // The case that used to pass: syntactically perfect, and the
        // world simply is not there.
        let err =
            parse_world_identifier("plamenix:plugin@1.0.0/plugin-does-not-exist").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("plugin-does-not-exist") && message.contains("Known worlds"),
            "the refusal should name the offender and the alternatives: {message}",
        );
    }

    #[test]
    fn another_packages_world_is_refused_even_when_the_name_matches() {
        let err = parse_world_identifier("evil:plugin@1.0.0/plugin-minimal").unwrap_err();
        assert!(err.to_string().contains("this host implements"));
    }

    #[test]
    fn the_shape_checks_still_hold() {
        for bad in [
            "plamenix:plugin@1.0.0",
            "plamenix:plugin@1.0.0/",
            "plamenix:plugin/plugin-minimal",
            "plamenix@1.0.0/plugin-minimal",
            "plamenix:plugin@not-semver/plugin-minimal",
        ] {
            assert!(
                parse_world_identifier(bad).is_err(),
                "`{bad}` should not parse",
            );
        }
    }

    #[test]
    fn contribution_capabilities_are_not_tiered() {
        // `export.format` says the plugin *provides* a format the host
        // will call into. Nothing imports it, so gating it behind a
        // world would stop a theme-tier plugin from contributing a
        // Markdown exporter for no reason.
        for world in PluginWorld::all() {
            assert!(world.permits(&Permission::ExportFormat));
            assert!(world.permits(&Permission::ImportSource));
        }
    }

    #[test]
    fn minimal_permits_nothing_beyond_logging() {
        // The whole claim of tier 0. If this ever passes something,
        // the capability model has a hole in its floor.
        for permission in [
            Permission::DbSchemaList,
            Permission::DbReadAny,
            Permission::DbWriteAny,
            Permission::NetHttps,
            Permission::ClipboardRead,
            Permission::OsNotify,
        ] {
            assert!(
                !PluginWorld::Minimal.permits(&permission),
                "minimal must not permit {permission:?}",
            );
        }
    }

    #[test]
    fn a_tier_permits_everything_the_tiers_below_it_do() {
        // The `include` chain in the WIT is transitive; this asserts the
        // Rust mirror of it did not drift.
        let ladder = [
            Permission::DbSchemaList,
            Permission::DbWriteAny,
            Permission::NetHttps,
            Permission::OsNotify,
        ];
        for (index, permission) in ladder.iter().enumerate() {
            for world in PluginWorld::all() {
                let expected = usize::from(world.tier()) > index;
                assert_eq!(
                    world.permits(permission),
                    expected,
                    "{}.permits({permission:?})",
                    world.name(),
                );
            }
        }
    }

    #[test]
    fn a_capability_outside_the_world_is_refused_with_the_world_to_use() {
        let err = check_permissions(
            PluginWorld::Minimal,
            &permissions(vec![Permission::DbWriteAny]),
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("plugin-db-writer"),
            "the refusal should name the world that would work: {message}",
        );
    }

    #[test]
    fn an_optional_capability_outside_the_world_is_refused_too() {
        // Optional is still a prompt the user gets asked, for an import
        // the plugin could never reach.
        let set = PermissionSet {
            required: Vec::new(),
            optional: vec![PermissionGrant::new(Permission::FsReadDir(
                LogicalDir::PluginData,
            ))],
        };
        assert!(check_permissions(PluginWorld::DbReader, &set).is_err());
    }

    #[test]
    fn a_capability_inside_the_world_passes() {
        assert!(
            check_permissions(
                PluginWorld::Integrated,
                &permissions(vec![Permission::DbReadAny, Permission::NetHttps]),
            )
            .is_ok()
        );
    }

    #[test]
    fn only_minimal_is_linkable_and_the_refusal_explains_whose_fault_it_is() {
        assert!(check_linkable(PluginWorld::Minimal).is_ok());
        let err = check_linkable(PluginWorld::DbReader).unwrap_err();
        assert!(
            err.to_string().contains("no host implementation"),
            "the refusal must not read like the plugin is at fault",
        );
    }

    #[test]
    fn a_desktop_only_world_may_not_also_target_web() {
        assert!(check_targets(PluginWorld::IntegratedDesktop, &[Edition::Desktop]).is_ok());
        assert!(
            check_targets(
                PluginWorld::IntegratedDesktop,
                &[Edition::Desktop, Edition::Web],
            )
            .is_err(),
            "declaring both is the interesting case — the plugin would install and then fail on one edition",
        );
        assert!(check_targets(PluginWorld::Integrated, &[Edition::Web]).is_ok());
    }
}
