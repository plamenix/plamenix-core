//! Does this plugin get to do that?
//!
//! Every host import asks this before it does anything, and the answer
//! is three checks in order — the world exposes the import, the
//! manifest declared the capability, the user granted it. They fail
//! differently on purpose: "your manifest never asked for this" and
//! "the user said no" are different problems with different fixes, and
//! a plugin author staring at one message needs to know which.
//!
//! ## Refusals are values, never traps
//!
//! `bindgen!` runs with `trappable_imports`, so a host import returns
//! `wasmtime::Result<Result<T, E>>` and the outer `Err` raises a guest
//! trap. A capability denial is always the *inner* `Err`.
//!
//! Trapping would be worse than rude. [`crate::trap`] would classify it
//! as the plugin's fault, `dispatch_event_supervised` would charge the
//! crash budget, and three refused calls in a minute would disable a
//! plugin for asking politely about a permission it had declared and
//! the user had simply not approved yet.
//!
//! ## Required is not the same as granted
//!
//! Both buckets of a manifest's `[permissions]` need a user grant.
//! Treating `required` as self-approving would let a plugin widen its
//! own reach by editing its own manifest, which is the opposite of what
//! a permission model is for. A plugin whose required capabilities are
//! unapproved still activates — it just gets refusals until the user
//! decides, which is what the permissions panel is for.

use std::fmt;

use crate::capability::Permission;
use crate::host_impl::HostState;

/// What a host import needs before it will act.
#[derive(Clone, Debug)]
pub enum Guard {
    /// Any one of these satisfies it.
    ///
    /// Used where several capabilities imply the same access —
    /// `db.read.any` or `db.write.any` both let a plugin see a row.
    Any(&'static [Permission]),
    /// All of these must hold.
    ///
    /// Used where two capabilities answer different questions and both
    /// have to be yes, as the keyring's "may reach the OS keyring" and
    /// "may touch this service" do.
    All(Vec<Permission>),
}

/// Why a host import was refused.
///
/// Each carries the capability so the message can name it. A refusal
/// the author cannot act on is a support ticket by construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Denial {
    /// The plugin's world does not expose the import behind this.
    ///
    /// Defence in depth. The linker should already have refused to
    /// instantiate a component importing an interface its world does
    /// not expose, so reaching this means something upstream is wrong.
    OutsideWorld(Permission),
    /// The manifest never asked for it. The plugin is operating
    /// outside its own declared contract.
    NotDeclared(Permission),
    /// The manifest asked and the user has not approved it.
    NotGranted(Permission),
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorld(permission) => write!(
                f,
                "capability `{permission}` is outside the plugin's declared world",
            ),
            Self::NotDeclared(permission) => write!(
                f,
                "capability `{permission}` is not declared in the plugin's manifest",
            ),
            Self::NotGranted(permission) => write!(
                f,
                "capability `{permission}` has not been granted by the user",
            ),
        }
    }
}

/// Checks `guard` against what the plugin declared and holds.
///
/// # Errors
///
/// The first [`Denial`] encountered. For [`Guard::Any`] that is the
/// denial for the last candidate, since an earlier one failing is not
/// itself a refusal — the message names something the plugin could
/// plausibly have asked for.
pub fn check(state: &HostState, guard: &Guard) -> Result<(), Denial> {
    match guard {
        Guard::All(permissions) => {
            for permission in permissions {
                check_one(state, permission)?;
            }
            Ok(())
        }
        Guard::Any(candidates) => {
            let mut last = None;
            for permission in *candidates {
                match check_one(state, permission) {
                    Ok(()) => return Ok(()),
                    Err(denial) => last = Some(denial),
                }
            }
            // An empty candidate list would silently permit everything,
            // so it is a refusal rather than a pass.
            Err(last.unwrap_or_else(|| Denial::NotDeclared(Permission::DbReadAny)))
        }
    }
}

fn check_one(state: &HostState, permission: &Permission) -> Result<(), Denial> {
    if !state.world.permits(permission) {
        return Err(Denial::OutsideWorld(permission.clone()));
    }
    if !state.declared.declares(permission) {
        return Err(Denial::NotDeclared(permission.clone()));
    }
    if !state.granted.contains(&permission.to_string()) {
        return Err(Denial::NotGranted(permission.clone()));
    }
    Ok(())
}

/// Checks that `url` is one the plugin may reach.
///
/// The allow-list decision stays in the host even though the request
/// itself is performed by the shell. Policy here, transport there: a
/// shell that forgot to re-check would otherwise be an open proxy for
/// every plugin.
///
/// Host matching is exact, with no subdomain wildcards. `net.https`
/// grants any host over TLS; `net.https.<host>` grants one.
///
/// # Errors
///
/// [`Denial`] naming the capability that would have permitted it.
pub fn check_net(state: &HostState, url: &str) -> Result<(), Denial> {
    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        // An unparseable URL cannot be matched against an allow-list,
        // so it is refused rather than passed to the shell to sort out.
        Err(_) => return Err(Denial::NotGranted(Permission::NetHttps)),
    };
    let host = parsed.host_str().unwrap_or_default().to_owned();

    let candidates: Vec<Permission> = match parsed.scheme() {
        "https" => vec![Permission::NetHttpsHost(host), Permission::NetHttps],
        // Plain HTTP is its own capability and is not implied by the
        // TLS one — downgrading silently is how a plugin ends up
        // sending a token in clear text.
        "http" => vec![Permission::NetHttp],
        _ => return Err(Denial::NotGranted(Permission::NetHttps)),
    };

    let mut last = None;
    for permission in candidates {
        match check_one(state, &permission) {
            Ok(()) => return Ok(()),
            Err(denial) => last = Some(denial),
        }
    }
    Err(last.unwrap_or(Denial::NotGranted(Permission::NetHttps)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;

    use super::*;
    use crate::capability::{PermissionGrant, PermissionSet};
    use crate::world::PluginWorld;

    fn state(world: PluginWorld, declared: &[Permission], granted: &[&str]) -> HostState {
        let mut host = HostState::new("dev.plamenix.test", "1.0.0-beta")
            .with_world(world)
            .with_declared_permissions(PermissionSet {
                required: declared.iter().cloned().map(PermissionGrant::new).collect(),
                optional: Vec::new(),
            });
        host.granted = granted
            .iter()
            .map(|g| (*g).to_owned())
            .collect::<HashSet<_>>();
        host
    }

    #[test]
    fn a_declared_and_granted_capability_passes() {
        let host = state(
            PluginWorld::DbReader,
            &[Permission::DbReadAny],
            &["db.read.any"],
        );
        assert!(check(&host, &Guard::Any(&[Permission::DbReadAny])).is_ok());
    }

    #[test]
    fn declaring_without_a_grant_is_refused() {
        // The case that matters most: a manifest cannot approve itself.
        let host = state(PluginWorld::DbReader, &[Permission::DbReadAny], &[]);
        assert_eq!(
            check(&host, &Guard::Any(&[Permission::DbReadAny])),
            Err(Denial::NotGranted(Permission::DbReadAny)),
        );
    }

    #[test]
    fn granting_something_never_declared_is_still_refused() {
        // A grant store that drifted, or a user prompted for something
        // the manifest no longer asks for.
        let host = state(PluginWorld::DbReader, &[], &["db.read.any"]);
        assert_eq!(
            check(&host, &Guard::Any(&[Permission::DbReadAny])),
            Err(Denial::NotDeclared(Permission::DbReadAny)),
        );
    }

    #[test]
    fn the_world_is_checked_before_anything_else() {
        // Declared and granted, but the tier cannot reach the import.
        let host = state(
            PluginWorld::Minimal,
            &[Permission::DbReadAny],
            &["db.read.any"],
        );
        assert_eq!(
            check(&host, &Guard::Any(&[Permission::DbReadAny])),
            Err(Denial::OutsideWorld(Permission::DbReadAny)),
        );
    }

    #[test]
    fn any_passes_when_the_second_candidate_holds() {
        let host = state(
            PluginWorld::DbWriter,
            &[Permission::DbWriteAny],
            &["db.write.any"],
        );
        assert!(
            check(
                &host,
                &Guard::Any(&[Permission::DbReadAny, Permission::DbWriteAny]),
            )
            .is_ok()
        );
    }

    #[test]
    fn all_requires_every_candidate() {
        // The keyring shape: reaching the keyring at all, and reaching
        // this service, are different questions.
        let host = state(
            PluginWorld::IntegratedDesktop,
            &[Permission::SecretsReadService("token".into())],
            &["secrets.read.service.token"],
        );
        assert!(
            check(
                &host,
                &Guard::All(vec![Permission::SecretsReadService("token".into())]),
            )
            .is_ok()
        );
        assert!(
            check(
                &host,
                &Guard::All(vec![
                    Permission::SecretsReadService("token".into()),
                    Permission::AuthOsKeyring(crate::capability::OsKeyring::Macos),
                ]),
            )
            .is_err(),
            "holding one of two required capabilities must not pass",
        );
    }

    #[test]
    fn a_host_specific_grant_permits_only_that_host() {
        let host = state(
            PluginWorld::Integrated,
            &[Permission::NetHttpsHost("api.example.com".into())],
            &["net.https.api.example.com"],
        );
        assert!(check_net(&host, "https://api.example.com/v1/things").is_ok());
        assert!(
            check_net(&host, "https://evil.example.com/").is_err(),
            "a grant for one host must not cover another",
        );
    }

    #[test]
    fn tls_does_not_imply_plaintext() {
        // Downgrading silently is how a plugin sends a token in clear.
        let host = state(
            PluginWorld::Integrated,
            &[Permission::NetHttps],
            &["net.https"],
        );
        assert!(check_net(&host, "https://api.example.com/").is_ok());
        assert!(check_net(&host, "http://api.example.com/").is_err());
    }

    #[test]
    fn an_unparseable_or_exotic_url_is_refused_rather_than_forwarded() {
        let host = state(
            PluginWorld::Integrated,
            &[Permission::NetHttps],
            &["net.https"],
        );
        assert!(check_net(&host, "not a url").is_err());
        assert!(
            check_net(&host, "file:///etc/passwd").is_err(),
            "only http and https are matchable at all",
        );
    }
}
