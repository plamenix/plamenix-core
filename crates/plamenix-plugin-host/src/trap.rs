//! Why a call into a plugin ended, in terms the host can act on.
//!
//! wasmtime reports a failed call as an `anyhow::Error`, and the
//! interesting part is never the outermost layer. `Display` on that
//! error gives only the context wasmtime wrapped it in — `error while
//! executing at wasm backtrace: ...` — while the actual reason (a
//! deadline, an unreachable, an out-of-bounds access) is a *source*
//! further down the chain. So `err.to_string()` stringifies a runaway
//! loop, a panicking plugin, and a refused allocation identically, and
//! the supervisor cannot tell them apart.
//!
//! That is not hypothetical: it is what the adversarial fixture in
//! `tests/misbehaving_plugin.rs` found. A test asserting the looping
//! plugin had been *preempted* could only check that the call failed
//! somehow, which a plugin trapping for an unrelated reason would have
//! satisfied just as well.
//!
//! ## On the deadline mapping
//!
//! wasmtime's own documentation does not name the trap an exceeded
//! epoch deadline produces. `Store::set_epoch_deadline` says only that
//! "execution will terminate with a trap", and `Trap::Interrupt` says
//! only "execution has potentially run too long and may be
//! interrupted". Neither states they are the same event. The mapping
//! below is therefore load-bearing and unverifiable by reading: it is
//! held up by `a_looping_plugin_is_preempted_rather_than_hanging_the_host`,
//! which drives a real deadline and requires [`FailureKind::Deadline`]
//! to come back. If a wasmtime upgrade changes the variant, that test
//! fails rather than the classification silently degrading to
//! [`FailureKind::Trap`].

use std::fmt;

use wasmtime::Trap;

/// Why a call into a plugin ended early.
///
/// The host branches on this; [`CallFailure::message`] is for humans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The call exceeded its epoch deadline and the host interrupted
    /// it. The plugin did nothing illegal — it just took too long.
    Deadline,
    /// The guest trapped: an unreachable, an out-of-bounds access, a
    /// panic in the plugin's own code, or an allocation its memory cap
    /// refused.
    Trapped,
    /// The call failed outside the guest — a host function returned an
    /// error, or the call never reached wasm at all.
    Host,
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Deadline => "deadline exceeded",
            Self::Trapped => "trapped",
            Self::Host => "host error",
        };
        f.write_str(text)
    }
}

/// A failed call into a plugin: what kind of failure, and the whole
/// error chain that described it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallFailure {
    /// What sort of failure this was.
    pub kind: FailureKind,
    /// Every layer of the error chain, outermost first, joined with
    /// `": "`. Includes the wasm backtrace when wasmtime captured one.
    pub message: String,
}

impl CallFailure {
    /// Classifies and describes a failed call into a plugin.
    pub(crate) fn from_error(err: &wasmtime::Error) -> Self {
        let kind = match err.downcast_ref::<Trap>() {
            Some(Trap::Interrupt) => FailureKind::Deadline,
            Some(_) => FailureKind::Trapped,
            // No `Trap` anywhere in the chain means the call never
            // trapped: the failure came from the host side of the
            // boundary, or from wasmtime's own machinery.
            None => FailureKind::Host,
        };
        Self {
            kind,
            message: describe(err),
        }
    }
}

impl fmt::Display for CallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// Renders every layer of an error chain, outermost first.
///
/// `to_string` on an `anyhow::Error` prints one layer. For a wasm trap
/// that layer is the backtrace wrapper, so the reason is exactly the
/// part it omits.
fn describe(err: &wasmtime::Error) -> String {
    err.chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_error_is_not_attributed_to_the_guest() {
        let failure = CallFailure::from_error(&wasmtime::Error::msg("keyring unavailable"));
        assert_eq!(failure.kind, FailureKind::Host);
        assert_eq!(failure.message, "keyring unavailable");
    }

    #[test]
    fn an_interrupt_reads_as_a_deadline_and_anything_else_as_a_trap() {
        assert_eq!(
            CallFailure::from_error(&Trap::Interrupt.into()).kind,
            FailureKind::Deadline,
        );
        assert_eq!(
            CallFailure::from_error(&Trap::UnreachableCodeReached.into()).kind,
            FailureKind::Trapped,
        );
        assert_eq!(
            CallFailure::from_error(&Trap::MemoryOutOfBounds.into()).kind,
            FailureKind::Trapped,
        );
    }

    #[test]
    fn the_reason_survives_being_wrapped_in_context() {
        // The shape that motivated this module: wasmtime wraps the trap
        // in backtrace context, so the outermost layer is the part that
        // says nothing. Both layers must survive.
        let wrapped = wasmtime::Error::from(Trap::Interrupt)
            .context("error while executing at wasm backtrace");
        let failure = CallFailure::from_error(&wrapped);

        assert_eq!(failure.kind, FailureKind::Deadline);
        assert!(
            failure.message.contains("wasm backtrace") && failure.message.contains("interrupt"),
            "the chain lost a layer: {}",
            failure.message,
        );
    }
}
