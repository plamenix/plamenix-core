//! Interceptors — the control surface plugins use to refuse or rewrite
//! an operation before it commits.
//!
//! [`crate::dispatch`] is the notification half: an event subscriber
//! learns something happened and cannot object. This is the other half,
//! and the difference is the whole point. A `query.executing`
//! interceptor runs *before* the statement and may cancel it.
//!
//! `plamenix-ui/src/interceptors/` already ships a tested chain
//! primitive with priority ordering, cancel/replace/continue semantics,
//! a 500ms budget, and fail-open-on-trap. Eight chains are built on it
//! and first-party code has been able to intercept for some time. What
//! was missing was any way for a WASM plugin to participate: the WIT
//! contract had no interceptor export, so the plugin half of the
//! feature did not exist.
//!
//! This module is the host end of that bridge. It resolves which
//! plugins registered for a point, calls each one's `intercept` export,
//! and turns what comes back into something the TypeScript chain can
//! consume as an ordinary handler.
//!
//! ## Fail-open, deliberately
//!
//! A plugin that traps, overruns its deadline, or returns a cancel with
//! no reason is skipped and the operation proceeds. That is the
//! opposite of the usual security default and it is the right one here:
//! interceptors gate "the user is about to do X", so a missed policy
//! check is recoverable by the user's own judgement, while a failed
//! check that blocks the operation locks someone out of their own
//! database because a third-party plugin crashed. `plugin-interceptors.md`
//! calls this out explicitly and the TypeScript chain already behaves
//! this way; this module matches it rather than inventing a second
//! policy.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::concurrency::InFlightAcquireError;
use crate::epoch::CallClass;
use crate::error::PluginError;
use crate::instance::InstanceRegistry;
use crate::supervisor::{ExitReason, RestartDecision, Supervisor};
use crate::trap::{CallFailure, FailureKind};

/// The extension points a plugin may register an interceptor for.
///
/// A closed set rather than a free string: a typo in a manifest should
/// be an install-time refusal, not a handler that silently never runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionPoint {
    /// Before any statement executes.
    QueryExecuting,
    /// Before an inline cell edit commits.
    CellCommitting,
    /// Before a row insert.
    RowInserting,
    /// Before a row delete.
    RowDeleting,
    /// Before a connection attempt.
    ConnectionOpening,
    /// Before an export begins.
    ExportStarting,
    /// Before an editor buffer is saved.
    EditorSaving,
    /// Before a `RECREATE` / `DROP` / `ALTER`.
    SchemaActionApplying,
}

impl ExtensionPoint {
    /// The wire name, matching the chain module names in
    /// `plamenix-ui/src/interceptors/` and the catalogue in
    /// `docs/plugin-interceptors.md`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QueryExecuting => "query.executing",
            Self::CellCommitting => "cell.committing",
            Self::RowInserting => "row.inserting",
            Self::RowDeleting => "row.deleting",
            Self::ConnectionOpening => "connection.opening",
            Self::ExportStarting => "export.starting",
            Self::EditorSaving => "editor.saving",
            Self::SchemaActionApplying => "schema.action-applying",
        }
    }

    /// Every point, in catalogue order.
    #[must_use]
    pub const fn all() -> [Self; 8] {
        [
            Self::QueryExecuting,
            Self::CellCommitting,
            Self::RowInserting,
            Self::RowDeleting,
            Self::ConnectionOpening,
            Self::ExportStarting,
            Self::EditorSaving,
            Self::SchemaActionApplying,
        ]
    }

    /// Resolves a wire name.
    ///
    /// # Errors
    ///
    /// [`PluginError::InvalidManifest`] listing the valid points, so an
    /// author who mistyped one can see the whole set.
    pub fn parse(token: &str) -> Result<Self, PluginError> {
        Self::all()
            .into_iter()
            .find(|point| point.as_str() == token)
            .ok_or_else(|| {
                let known = Self::all().map(Self::as_str).join(", ");
                PluginError::InvalidManifest(format!(
                    "contributions.interceptors extension_point `{token}` is not a known point (valid: {known})",
                ))
            })
    }
}

/// Lowest priority a plugin may claim. Everything below is reserved for
/// the host's own interceptors, which must be able to run first — the
/// read-only guard is not something a plugin gets to preempt.
pub const MIN_PLUGIN_PRIORITY: u16 = 100;

/// Highest priority value accepted at all.
pub const MAX_PRIORITY: u16 = 1000;

/// One plugin's registration for one extension point.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InterceptorRegistration {
    /// Plugin that registered.
    pub plugin_id: String,
    /// Point it registered for.
    pub point: ExtensionPoint,
    /// Lower runs earlier. Ties break on plugin id, so the resolved
    /// order does not depend on install order.
    pub priority: u16,
    /// Author's rationale, shown in the permissions panel.
    pub purpose: Option<String>,
}

/// Which plugins want to be consulted, for which points.
///
/// Populated from manifests at activation and drained at deactivation,
/// mirroring how [`crate::event_bus::EventBus`] handles subscriptions.
#[derive(Debug, Default)]
pub struct InterceptorRegistry {
    /// Keyed by point so a dispatch does not scan every registration.
    /// `BTreeMap` for a deterministic snapshot in tests and in the UI.
    inner: Mutex<BTreeMap<ExtensionPoint, Vec<InterceptorRegistration>>>,
}

impl InterceptorRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one registration, keeping each point's list in resolved
    /// order.
    ///
    /// # Errors
    ///
    /// [`PluginError::Runtime`] if the lock was poisoned by a panic in
    /// another thread.
    pub fn register(&self, registration: InterceptorRegistration) -> Result<(), PluginError> {
        let mut guard = self.inner.lock().map_err(poisoned)?;
        let list = guard.entry(registration.point).or_default();
        list.push(registration);
        // Sorted on insert rather than on read: dispatch happens in
        // front of a user waiting for their query to run, and
        // registration happens once at activation.
        list.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.plugin_id.cmp(&right.plugin_id))
        });
        drop(guard);
        Ok(())
    }

    /// Drops every registration belonging to `plugin_id`.
    ///
    /// # Errors
    ///
    /// [`PluginError::Runtime`] if the lock was poisoned.
    pub fn unregister_by_plugin(&self, plugin_id: &str) -> Result<(), PluginError> {
        let mut guard = self.inner.lock().map_err(poisoned)?;
        for list in guard.values_mut() {
            list.retain(|registration| registration.plugin_id != plugin_id);
        }
        guard.retain(|_, list| !list.is_empty());
        drop(guard);
        Ok(())
    }

    /// Registrations for one point, in the order they should run.
    ///
    /// # Errors
    ///
    /// [`PluginError::Runtime`] if the lock was poisoned.
    pub fn for_point(
        &self,
        point: ExtensionPoint,
    ) -> Result<Vec<InterceptorRegistration>, PluginError> {
        let guard = self.inner.lock().map_err(poisoned)?;
        Ok(guard.get(&point).cloned().unwrap_or_default())
    }

    /// Every registration, for the permissions panel and for the shells
    /// to build their chain handlers from at boot.
    ///
    /// # Errors
    ///
    /// [`PluginError::Runtime`] if the lock was poisoned.
    pub fn all(&self) -> Result<Vec<InterceptorRegistration>, PluginError> {
        let guard = self.inner.lock().map_err(poisoned)?;
        Ok(guard.values().flatten().cloned().collect())
    }
}

fn poisoned<T>(_: PoisonError<T>) -> PluginError {
    PluginError::Runtime("interceptor registry lock poisoned".to_owned())
}

/// What one plugin decided about one operation.
///
/// The host's view, after the fail-open rules have been applied — a
/// plugin that trapped is already a [`Self::Proceed`] here, with the
/// failure recorded separately in [`Interception::failure`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Verdict {
    /// Nothing to change.
    Proceed,
    /// Continue against this context instead.
    Replace {
        /// The replacement context, as JSON.
        context_json: String,
    },
    /// Refuse, with a reason for the user.
    Cancel {
        /// Why. Never empty — an empty reason is downgraded to
        /// [`Self::Proceed`] before it gets here.
        reason: String,
    },
}

/// One plugin's contribution to a chain run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interception {
    /// Plugin consulted.
    pub plugin_id: String,
    /// What it decided, after fail-open handling.
    pub verdict: Verdict,
    /// Set when the call failed and the verdict was forced to
    /// `Proceed`. Carried rather than swallowed so the shell can
    /// attribute a slow or broken plugin instead of silently ignoring
    /// it.
    pub failure: Option<CallFailure>,
}

/// Consults one plugin about one operation.
///
/// Returns [`Verdict::Proceed`] with a populated `failure` whenever the
/// plugin could not be reached or did not behave — see the fail-open
/// note in the module docs. Never returns an error: there is no failure
/// mode here that should abort the user's operation.
pub async fn intercept_one(
    registry: &InstanceRegistry,
    plugin_id: &str,
    point: ExtensionPoint,
    context_json: &str,
) -> Interception {
    let failed = |failure: CallFailure| Interception {
        plugin_id: plugin_id.to_owned(),
        verdict: Verdict::Proceed,
        failure: Some(failure),
    };

    let instance = match registry.get(plugin_id) {
        Ok(Some(instance)) => instance,
        // Not instantiated is not a failure worth reporting: a UI-only
        // plugin legitimately has no wasm half to consult.
        Ok(None) => {
            return Interception {
                plugin_id: plugin_id.to_owned(),
                verdict: Verdict::Proceed,
                failure: None,
            };
        }
        Err(err) => {
            return failed(CallFailure {
                kind: FailureKind::Host,
                message: err.to_string(),
            });
        }
    };

    let _permit = match instance.acquire_call_permit().await {
        Ok(permit) => permit,
        Err(InFlightAcquireError::Closed) => {
            return Interception {
                plugin_id: plugin_id.to_owned(),
                verdict: Verdict::Proceed,
                failure: None,
            };
        }
    };

    let mut store = instance.lock_store().await;
    // Interactive, not background. An interceptor is by definition
    // standing between the user and something they asked for.
    store.set_epoch_deadline(CallClass::Interactive.deadline_ticks());

    let raw = instance
        .bindings()
        .plamenix_plugin_plugin()
        .call_intercept(&mut *store, point.as_str(), context_json)
        .await;
    // Released before the result is shaped: the next plugin in the
    // chain cannot be called while this one's store is still locked.
    drop(store);

    match raw {
        Ok(interception) => Interception {
            plugin_id: plugin_id.to_owned(),
            verdict: normalise(plugin_id, point, interception),
            failure: None,
        },
        Err(err) => {
            let failure = CallFailure::from_error(&err);
            tracing::warn!(
                plugin = %plugin_id,
                point = point.as_str(),
                kind = %failure.kind,
                message = %failure.message,
                "interceptor failed; proceeding without it",
            );
            failed(failure)
        }
    }
}

/// Applies the rules the wire type cannot express.
///
/// A cancel with an empty reason is the one worth spelling out. The
/// manifest can require a `purpose`, and the WIT can require the
/// payload to be present, but neither can stop a plugin returning `""`
/// at runtime — and "your query was blocked" with no explanation is a
/// worse outcome than letting it through, because the user has no way
/// to act on it and no way to know which plugin to blame.
fn normalise(
    plugin_id: &str,
    point: ExtensionPoint,
    interception: crate::bindings::exports::plamenix::plugin::plugin::Interception,
) -> Verdict {
    use crate::bindings::exports::plamenix::plugin::plugin::Interception as Wire;

    match interception {
        Wire::Proceed => Verdict::Proceed,
        Wire::Replace(context_json) => Verdict::Replace { context_json },
        Wire::Cancel(reason) if reason.trim().is_empty() => {
            tracing::warn!(
                plugin = %plugin_id,
                point = point.as_str(),
                "plugin cancelled without a reason; proceeding instead",
            );
            Verdict::Proceed
        }
        Wire::Cancel(reason) => Verdict::Cancel { reason },
    }
}

/// [`intercept_one`] plus crash-budget accounting.
///
/// A plugin that traps in an interceptor is charged exactly as one that
/// traps in an event handler. Without this the interceptor path would
/// be a way to trap indefinitely without ever being disabled, which is
/// the same hole [`crate::dispatch::dispatch_event_supervised`] closed
/// on the event side.
pub async fn intercept_one_supervised(
    registry: &InstanceRegistry,
    supervisor: &Supervisor,
    plugin_id: &str,
    point: ExtensionPoint,
    context_json: &str,
) -> (Interception, Option<RestartDecision>) {
    let interception = intercept_one(registry, plugin_id, point, context_json).await;

    let decision = match interception.failure {
        // A host-side failure is not the plugin's crash. Charging it
        // would let a poisoned lock disable an innocent plugin.
        None
        | Some(CallFailure {
            kind: FailureKind::Host,
            ..
        }) => None,
        Some(_) => {
            match supervisor.on_exit(plugin_id, ExitReason::Abnormal, std::time::Instant::now()) {
                Ok(decision) => Some(decision),
                Err(err) => {
                    tracing::warn!(
                        plugin = %plugin_id,
                        ?err,
                        "plugin failed an interception but is not registered with the supervisor",
                    );
                    None
                }
            }
        }
    };

    (interception, decision)
}

/// Runs every plugin registered for `point`, in resolved order, with
/// chain semantics.
///
/// A `Replace` propagates: later plugins see the replacement. A
/// `Cancel` short-circuits and later plugins do not run. This mirrors
/// `InterceptorChain.run` in `plamenix-ui` so a plugin behaves the same
/// whether the shell consults plugins one at a time or all at once.
///
/// Returns the final verdict plus every individual interception, so a
/// caller can both act on the outcome and attribute it.
pub async fn run_chain(
    instances: &InstanceRegistry,
    registrations: &[InterceptorRegistration],
    point: ExtensionPoint,
    context_json: &str,
) -> (Verdict, Vec<Interception>) {
    let mut current = context_json.to_owned();
    let mut replaced = false;
    let mut steps = Vec::with_capacity(registrations.len());

    for registration in registrations {
        let interception = intercept_one(instances, &registration.plugin_id, point, &current).await;
        match interception.verdict.clone() {
            Verdict::Proceed => {}
            Verdict::Replace { context_json } => {
                current = context_json;
                replaced = true;
            }
            Verdict::Cancel { reason } => {
                steps.push(interception);
                return (Verdict::Cancel { reason }, steps);
            }
        }
        steps.push(interception);
    }

    let verdict = if replaced {
        Verdict::Replace {
            context_json: current,
        }
    } else {
        Verdict::Proceed
    };
    (verdict, steps)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn registration(plugin_id: &str, priority: u16) -> InterceptorRegistration {
        InterceptorRegistration {
            plugin_id: plugin_id.to_owned(),
            point: ExtensionPoint::QueryExecuting,
            priority,
            purpose: None,
        }
    }

    #[test]
    fn every_point_round_trips_through_its_wire_name() {
        for point in ExtensionPoint::all() {
            assert_eq!(ExtensionPoint::parse(point.as_str()).unwrap(), point);
        }
    }

    #[test]
    fn a_mistyped_point_is_refused_with_the_valid_set() {
        let err = ExtensionPoint::parse("query.executed").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("query.executing"),
            "an author who typed the event name by mistake needs to see the right one: {message}",
        );
    }

    #[test]
    fn registrations_resolve_by_priority_then_plugin_id() {
        let registry = InterceptorRegistry::new();
        // Registered in an order that is wrong on both keys.
        for entry in [
            registration("zeta", 100),
            registration("alpha", 100),
            registration("early", 100),
            registration("last", 900),
        ] {
            registry.register(entry).unwrap();
        }

        let resolved: Vec<String> = registry
            .for_point(ExtensionPoint::QueryExecuting)
            .unwrap()
            .into_iter()
            .map(|entry| entry.plugin_id)
            .collect();

        assert_eq!(resolved, ["alpha", "early", "zeta", "last"]);
    }

    #[test]
    fn install_order_does_not_change_the_resolved_order() {
        // The tie-break exists so two users with the same plugins get
        // the same chain regardless of what they installed first.
        let forwards = InterceptorRegistry::new();
        let backwards = InterceptorRegistry::new();
        let entries = [
            registration("b", 100),
            registration("a", 100),
            registration("c", 100),
        ];
        for entry in entries.clone() {
            forwards.register(entry).unwrap();
        }
        for entry in entries.into_iter().rev() {
            backwards.register(entry).unwrap();
        }
        assert_eq!(
            forwards.for_point(ExtensionPoint::QueryExecuting).unwrap(),
            backwards.for_point(ExtensionPoint::QueryExecuting).unwrap(),
        );
    }

    #[test]
    fn unregistering_a_plugin_removes_it_from_every_point() {
        let registry = InterceptorRegistry::new();
        registry.register(registration("doomed", 100)).unwrap();
        registry
            .register(InterceptorRegistration {
                plugin_id: "doomed".to_owned(),
                point: ExtensionPoint::EditorSaving,
                priority: 100,
                purpose: None,
            })
            .unwrap();
        registry.register(registration("survivor", 100)).unwrap();

        registry.unregister_by_plugin("doomed").unwrap();

        let all = registry.all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].plugin_id, "survivor");
    }
}
