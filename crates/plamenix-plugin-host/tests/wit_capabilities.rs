//! Every capability the WIT contract names must parse.
//!
//! The WIT is the contract external plugin authors read — it ships
//! verbatim in `plamenix-plugin-sdk` — so a capability spelled one way
//! there and another way in `Permission::parse` is not an
//! inconsistency, it is an author's first manifest being rejected by
//! the host while they follow our own documentation.
//!
//! That had happened. Four annotations used a colon form
//! (`db:schema.list`, `fs:read:plugin-data`, `command:invoke`) that the
//! parser, which splits on `.` only, could never accept — while the
//! other seven were already dotted and correct. This test is the reason
//! it cannot come back.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use plamenix_plugin_host::Permission;

/// The contract as shipped to plugin authors.
const WIT: &str = include_str!("../wit/plamenix.wit");

/// Pulls every ``Capability: `x`,`y`` annotation out of the WIT.
///
/// Deliberately parsed rather than listed: a list would be a second
/// place to update, and the whole point here is to have one.
fn capabilities_named_in_wit() -> Vec<String> {
    let mut found = Vec::new();
    for line in WIT.lines() {
        let Some(rest) = line.split_once("Capability: ") else {
            continue;
        };
        for token in rest.1.split('+') {
            let token = token.trim().trim_end_matches('.');
            // Each annotation is one or more backticked capabilities.
            for piece in token.split('`').skip(1).step_by(2) {
                let piece = piece.trim();
                if piece.is_empty() {
                    continue;
                }
                // Placeholders stand in for a value the author supplies.
                let concrete = piece
                    .replace("<host>", "example.com")
                    .replace("<port>", "443");
                found.push(concrete);
            }
        }
    }
    found
}

#[test]
fn the_wit_names_capabilities_at_all() {
    // Guards the guard: a change to the annotation format that made the
    // scan find nothing would otherwise turn this file green and
    // useless.
    let found = capabilities_named_in_wit();
    assert!(
        found.len() >= 10,
        "expected the WIT to annotate at least ten capabilities, found {}: {found:?}",
        found.len()
    );
}

#[test]
fn every_capability_in_the_wit_parses() {
    let mut rejected = Vec::new();
    for capability in capabilities_named_in_wit() {
        if Permission::parse(&capability).is_err() {
            rejected.push(capability);
        }
    }
    assert!(
        rejected.is_empty(),
        "the WIT names capabilities the host cannot parse, so a plugin \
         author following the contract would be refused: {rejected:?}"
    );
}

#[test]
fn a_capability_round_trips_through_display() {
    // The parser and `Display` are the two halves of the same grammar,
    // and the permissions dialog shows what `Display` produces. A
    // capability that parsed but rendered differently would show the
    // user a string their manifest does not contain.
    for capability in capabilities_named_in_wit() {
        let parsed = Permission::parse(&capability)
            .unwrap_or_else(|err| panic!("{capability} did not parse: {err}"));
        assert_eq!(
            parsed.to_string(),
            capability,
            "`{capability}` parses but renders as `{parsed}`",
        );
    }
}
