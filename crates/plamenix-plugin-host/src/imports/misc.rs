//! The remaining imports: `event-bus`, `command`, `net`, `clipboard`,
//! `notify`, `keyring`.
//!
//! Grouped because each is small and they share nothing but the shape
//! every import in this crate has — gate, act, charge.
//!
//! Two of them are worth reading before changing:
//!
//! * **`event-bus.emit` never touches the bus.** It appends to
//!   [`crate::host_impl::HostState::outbox`] and returns. The host is
//!   holding this plugin's store for the duration of the call, so
//!   delivering inline would re-lock it — for a plugin subscribed to
//!   its own topic, its own lock — and `tokio::sync::Mutex` is not
//!   reentrant. The task would block forever with no epoch deadline to
//!   break it, because no wasm would be executing. The dispatcher
//!   drains the outbox after releasing the lock.
//! * **`net.fetch` decides here and transports there.** The allow-list
//!   check is [`crate::gate::check_net`], in this crate, and only a
//!   request that passed it reaches
//!   [`crate::services::HostServices::net_fetch`]. Putting the policy
//!   in the shell would make a shell that forgot to check an open proxy
//!   for every plugin.

use std::sync::Arc;

use crate::bindings::plamenix::plugin::clipboard::{ClipboardError, Host as ClipboardHost};
use crate::bindings::plamenix::plugin::command::{CommandError, Host as CommandHost};
use crate::bindings::plamenix::plugin::event_bus::{EventError, Host as EventBusHost};
use crate::bindings::plamenix::plugin::keyring::{Host as KeyringHost, KeyringError};
use crate::bindings::plamenix::plugin::net::{
    Host as NetHost, HttpHeader, HttpRequest, HttpResponse, NetError,
};
use crate::bindings::plamenix::plugin::notify::{Host as NotifyHost, NotifyError};
use crate::capability::Permission;
use crate::gate::{self, Guard};
use crate::host_impl::{HostState, PendingEmit};
use crate::imports::{KEYRING_IMPORT_BUDGET, NET_IMPORT_BUDGET, bounded};
use crate::services::{HostHttpRequest, ServiceError};

/// Most a plugin may queue from one call.
///
/// A plugin that emitted without bound would grow host memory inside a
/// call the host cannot interrupt, since the outbox is host-side and
/// invisible to the wasm `ResourceLimiter`.
pub const MAX_EMITS_PER_CALL: usize = 64;

/// Largest event payload a plugin may emit.
pub const MAX_EMIT_PAYLOAD_BYTES: usize = 64 * 1024;

const COMMAND_GUARD: Guard = Guard::Any(&[Permission::CommandInvoke]);
const CLIPBOARD_READ_GUARD: Guard = Guard::Any(&[Permission::ClipboardRead]);
const CLIPBOARD_WRITE_GUARD: Guard = Guard::Any(&[Permission::ClipboardWrite]);
const NOTIFY_GUARD: Guard = Guard::Any(&[Permission::OsNotify]);

/// The channel part of a topic — everything before the first `/`.
fn channel_of(topic: &str) -> String {
    topic.split('/').next().unwrap_or(topic).to_owned()
}

#[async_trait::async_trait]
impl EventBusHost for HostState {
    async fn emit(
        &mut self,
        topic: String,
        payload: String,
    ) -> wasmtime::Result<Result<(), EventError>> {
        // A plugin publishing under its own id needs no capability;
        // that namespace is its own by construction.
        let own_namespace = format!("{}:", self.plugin_id);
        if !topic.starts_with(&own_namespace) {
            let guard = Guard::All(vec![Permission::EventPublish(channel_of(&topic))]);
            if let Err(denial) = gate::check(self, &guard) {
                return Ok(Err(EventError::CapabilityDenied(denial.to_string())));
            }
        }

        if let Err(err) = crate::event_bus::tokenise_pattern(&topic) {
            return Ok(Err(EventError::InvalidTopic(err)));
        }
        if payload.len() > MAX_EMIT_PAYLOAD_BYTES {
            return Ok(Err(EventError::PayloadTooLarge(
                MAX_EMIT_PAYLOAD_BYTES as u32,
            )));
        }
        if self.outbox.len() >= MAX_EMITS_PER_CALL {
            return Ok(Err(EventError::Backpressure));
        }

        // Queued, not delivered. See the module docs.
        self.outbox.push(PendingEmit { topic, payload });
        Ok(Ok(()))
    }
}

#[async_trait::async_trait]
impl CommandHost for HostState {
    async fn invoke(
        &mut self,
        command_id: String,
        args_json: String,
    ) -> wasmtime::Result<Result<String, CommandError>> {
        if let Err(denial) = gate::check(self, &COMMAND_GUARD) {
            return Ok(Err(CommandError::CapabilityDenied(denial.to_string())));
        }

        let services = Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let outcome = bounded(
            self,
            NET_IMPORT_BUDGET,
            services.invoke_command(&ctx, &command_id, &args_json),
        )
        .await;

        Ok(outcome.map_err(|err| match err {
            ServiceError::NotFound(what) => CommandError::NotFound(what),
            ServiceError::Unsupported(what) => {
                CommandError::CapabilityDenied(format!("{what} is not available in this edition"))
            }
            other => CommandError::Invocation(other.to_string()),
        }))
    }
}

#[async_trait::async_trait]
impl NetHost for HostState {
    async fn fetch(
        &mut self,
        req: HttpRequest,
    ) -> wasmtime::Result<Result<HttpResponse, NetError>> {
        // The policy decision, made here and never delegated.
        if let Err(denial) = gate::check_net(self, &req.url) {
            return Ok(Err(NetError::NotAllowed(denial.to_string())));
        }

        let services = Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let request = HostHttpRequest {
            method: req.method,
            url: req.url,
            headers: req
                .headers
                .into_iter()
                .map(|header| (header.name, header.value))
                .collect(),
            body: req.body,
        };
        let outcome = bounded(self, NET_IMPORT_BUDGET, services.net_fetch(&ctx, request)).await;

        Ok(outcome
            .map(|response| HttpResponse {
                status: response.status,
                headers: response
                    .headers
                    .into_iter()
                    .map(|(name, value)| HttpHeader { name, value })
                    .collect(),
                body: response.body,
            })
            .map_err(|err| match err {
                ServiceError::TimedOut => NetError::Timeout,
                ServiceError::Unsupported(what) => {
                    NetError::NotAllowed(format!("{what} is not available in this edition"))
                }
                other => NetError::Transport(other.to_string()),
            }))
    }
}

#[async_trait::async_trait]
impl ClipboardHost for HostState {
    async fn read(&mut self) -> wasmtime::Result<Result<Option<String>, ClipboardError>> {
        if let Err(denial) = gate::check(self, &CLIPBOARD_READ_GUARD) {
            return Ok(Err(ClipboardError::CapabilityDenied(denial.to_string())));
        }
        let services = Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let outcome = bounded(self, NET_IMPORT_BUDGET, services.clipboard_read(&ctx)).await;
        Ok(outcome.map_err(clipboard_error))
    }

    async fn write(&mut self, text: String) -> wasmtime::Result<Result<(), ClipboardError>> {
        if let Err(denial) = gate::check(self, &CLIPBOARD_WRITE_GUARD) {
            return Ok(Err(ClipboardError::CapabilityDenied(denial.to_string())));
        }
        let services = Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let outcome = bounded(
            self,
            NET_IMPORT_BUDGET,
            services.clipboard_write(&ctx, &text),
        )
        .await;
        Ok(outcome.map_err(clipboard_error))
    }
}

fn clipboard_error(err: ServiceError) -> ClipboardError {
    match err {
        ServiceError::Unsupported(_) => ClipboardError::Unsupported,
        other => ClipboardError::Backend(other.to_string()),
    }
}

#[async_trait::async_trait]
impl NotifyHost for HostState {
    async fn display(
        &mut self,
        title: String,
        body: String,
    ) -> wasmtime::Result<Result<(), NotifyError>> {
        if let Err(denial) = gate::check(self, &NOTIFY_GUARD) {
            return Ok(Err(NotifyError::CapabilityDenied(denial.to_string())));
        }
        let services = Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let outcome = bounded(
            self,
            NET_IMPORT_BUDGET,
            services.notify_display(&ctx, &title, &body),
        )
        .await;
        Ok(outcome.map_err(|err| match err {
            ServiceError::Unsupported(_) => NotifyError::Unsupported,
            other => NotifyError::Backend(other.to_string()),
        }))
    }
}

/// Namespaces a plugin-supplied service name.
///
/// A plugin never names a bare keyring service. Without this, two
/// plugins asking for `"token"` would read each other's secrets, and a
/// plugin could name a service the *host* uses for its own connection
/// passwords.
fn namespaced(plugin_id: &str, service: &str) -> String {
    format!("dev.plamenix.plugins.{plugin_id}.{service}")
}

/// The keyring's two-axis check: may this plugin reach the OS keyring
/// at all, and may it touch this service.
fn keyring_guard(service: &str, write: bool) -> Guard {
    let scoped = if write {
        Permission::SecretsWriteService(service.to_owned())
    } else {
        Permission::SecretsReadService(service.to_owned())
    };
    Guard::All(vec![scoped])
}

#[async_trait::async_trait]
impl KeyringHost for HostState {
    async fn get(
        &mut self,
        service: String,
        account: String,
    ) -> wasmtime::Result<Result<Option<String>, KeyringError>> {
        if let Err(denial) = gate::check(self, &keyring_guard(&service, false)) {
            return Ok(Err(KeyringError::AccessDenied(denial.to_string())));
        }
        let services = Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let scoped = namespaced(&self.plugin_id, &service);
        let outcome = bounded(
            self,
            KEYRING_IMPORT_BUDGET,
            services.keyring_get(&ctx, &scoped, &account),
        )
        .await;
        Ok(outcome.map_err(keyring_error))
    }

    async fn set(
        &mut self,
        service: String,
        account: String,
        secret: String,
    ) -> wasmtime::Result<Result<(), KeyringError>> {
        if let Err(denial) = gate::check(self, &keyring_guard(&service, true)) {
            return Ok(Err(KeyringError::AccessDenied(denial.to_string())));
        }
        let services = Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let scoped = namespaced(&self.plugin_id, &service);
        let outcome = bounded(
            self,
            KEYRING_IMPORT_BUDGET,
            services.keyring_set(&ctx, &scoped, &account, &secret),
        )
        .await;
        Ok(outcome.map_err(keyring_error))
    }

    async fn delete(
        &mut self,
        service: String,
        account: String,
    ) -> wasmtime::Result<Result<(), KeyringError>> {
        if let Err(denial) = gate::check(self, &keyring_guard(&service, true)) {
            return Ok(Err(KeyringError::AccessDenied(denial.to_string())));
        }
        let services = Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let scoped = namespaced(&self.plugin_id, &service);
        let outcome = bounded(
            self,
            KEYRING_IMPORT_BUDGET,
            services.keyring_delete(&ctx, &scoped, &account),
        )
        .await;
        Ok(outcome.map_err(keyring_error))
    }
}

fn keyring_error(err: ServiceError) -> KeyringError {
    match err {
        ServiceError::NotFound(_) => KeyringError::NotFound,
        ServiceError::Unsupported(what) => {
            KeyringError::AccessDenied(format!("{what} is not available in this edition"))
        }
        other => KeyringError::Backend(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;

    use super::*;
    use crate::capability::{PermissionGrant, PermissionSet};
    use crate::services::{CallCtx, HostServices};
    use crate::world::PluginWorld;

    struct Grants(Vec<String>);
    #[async_trait::async_trait]
    impl HostServices for Grants {
        fn granted_for(&self, _: &str) -> HashSet<String> {
            self.0.iter().cloned().collect()
        }
    }

    fn state(declared: &[Permission], granted: &[&str]) -> HostState {
        let mut host = HostState::new("dev.plamenix.test", "1.0.0-beta")
            .with_world(PluginWorld::IntegratedDesktop)
            .with_declared_permissions(PermissionSet {
                required: declared.iter().cloned().map(PermissionGrant::new).collect(),
                optional: Vec::new(),
            })
            .with_services(Arc::new(Grants(
                granted.iter().map(|g| (*g).to_owned()).collect(),
            )));
        host.refresh_grants();
        host
    }

    #[tokio::test]
    async fn emitting_under_the_plugins_own_namespace_needs_no_capability() {
        let mut host = state(&[], &[]);
        let result = host
            .emit(
                "dev.plamenix.test:thing/happened".to_owned(),
                "{}".to_owned(),
            )
            .await
            .unwrap();

        assert!(result.is_ok(), "got {result:?}");
        assert_eq!(host.outbox.len(), 1);
    }

    #[tokio::test]
    async fn emitting_on_someone_elses_channel_needs_one() {
        let mut host = state(&[], &[]);
        let refused = host
            .emit("db/query/executed".to_owned(), "{}".to_owned())
            .await
            .unwrap();

        assert!(matches!(refused, Err(EventError::CapabilityDenied(_))));
        assert!(host.outbox.is_empty(), "a refused emit must not queue");
    }

    #[tokio::test]
    async fn emit_never_reaches_the_bus_from_inside_the_call() {
        // The deadlock guard. `emit` may only touch `&mut self`; if it
        // ever grew a dispatch, a plugin subscribed to its own topic
        // would re-lock its own store and hang forever.
        let mut host = state(&[], &[]);
        for index in 0..3 {
            host.emit(format!("dev.plamenix.test:n{index}"), "{}".to_owned())
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(host.outbox.len(), 3, "emits are queued, not delivered");
    }

    #[tokio::test]
    async fn a_plugin_cannot_queue_without_bound() {
        let mut host = state(&[], &[]);
        for index in 0..MAX_EMITS_PER_CALL {
            host.emit(format!("dev.plamenix.test:n{index}"), "{}".to_owned())
                .await
                .unwrap()
                .unwrap();
        }
        let refused = host
            .emit("dev.plamenix.test:one-too-many".to_owned(), "{}".to_owned())
            .await
            .unwrap();

        assert!(matches!(refused, Err(EventError::Backpressure)));
    }

    #[tokio::test]
    async fn an_oversized_payload_is_refused() {
        let mut host = state(&[], &[]);
        let refused = host
            .emit(
                "dev.plamenix.test:big".to_owned(),
                "x".repeat(MAX_EMIT_PAYLOAD_BYTES + 1),
            )
            .await
            .unwrap();

        assert!(matches!(refused, Err(EventError::PayloadTooLarge(_))));
    }

    #[tokio::test]
    async fn a_keyring_service_is_namespaced_to_the_plugin() {
        // Two plugins both asking for "token" must not collide, and
        // neither may name a service the host uses for its own
        // connection passwords.
        struct Recorder(std::sync::Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl HostServices for Recorder {
            fn granted_for(&self, _: &str) -> HashSet<String> {
                HashSet::from(["secrets.read.service.token".to_owned()])
            }
            async fn keyring_get(
                &self,
                _: &CallCtx,
                service: &str,
                _: &str,
            ) -> Result<Option<String>, ServiceError> {
                self.0.lock().unwrap().push(service.to_owned());
                Ok(None)
            }
        }

        let recorder = Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
        let mut host = state(
            &[Permission::SecretsReadService("token".into())],
            &["secrets.read.service.token"],
        )
        .with_services(recorder.clone());
        host.refresh_grants();

        host.get("token".to_owned(), "acct".to_owned())
            .await
            .unwrap()
            .unwrap();

        let seen = recorder.0.lock().unwrap();
        assert_eq!(seen[0], "dev.plamenix.plugins.dev.plamenix.test.token");
    }

    #[tokio::test]
    async fn an_edition_without_a_clipboard_says_unsupported_not_empty() {
        // `ok(none)` would be indistinguishable from an empty
        // clipboard, which is why the WIT gained an error channel.
        let mut host = state(&[Permission::ClipboardRead], &["clipboard.read"]);
        let refused = host.read().await.unwrap();
        assert!(matches!(refused, Err(ClipboardError::Unsupported)));
    }

    #[tokio::test]
    async fn a_url_outside_the_allow_list_never_reaches_the_shell() {
        struct WouldFetch;
        #[async_trait::async_trait]
        impl HostServices for WouldFetch {
            fn granted_for(&self, _: &str) -> HashSet<String> {
                HashSet::from(["net.https.api.example.com".to_owned()])
            }
            async fn net_fetch(
                &self,
                _: &CallCtx,
                _: HostHttpRequest,
            ) -> Result<crate::services::HostHttpResponse, ServiceError> {
                panic!("the host must refuse before the shell is asked");
            }
        }

        let mut host = state(
            &[Permission::NetHttpsHost("api.example.com".into())],
            &["net.https.api.example.com"],
        )
        .with_services(Arc::new(WouldFetch));
        host.refresh_grants();

        let refused = host
            .fetch(HttpRequest {
                method: "GET".to_owned(),
                url: "https://evil.example.com/exfiltrate".to_owned(),
                headers: Vec::new(),
                body: None,
            })
            .await
            .unwrap();

        assert!(matches!(refused, Err(NetError::NotAllowed(_))));
    }
}
