//! Unified CDP port lease foundation (Phase 6.3A.2, Foundation A).
//!
//! This module is the new authoritative lease abstraction that will replace the
//! split `port_bindings::PortBindings` + `QuestResource::CdpPort` ownership. It
//! is intentionally added *alongside* the legacy code and is not yet wired into
//! `AppState` or any call site; Integration C performs that migration.
//!
//! Model
//! -----
//! * [`CdpPortLeases`] owns a single `Arc<Mutex<Inner>>`; `Inner` holds a
//!   checked, non-wrapping id counter and a `port -> LeaseRecord` map.
//! * A [`CdpPortLease`] is non-cloneable and is the *only* object that may clear
//!   a record. Its `Drop` removes a record only when the exact `(port, id)`
//!   still matches, so a stale or cancelled lease can never delete a newer one
//!   or resurrect a record.
//! * Strict one-holder semantics: while any record exists for a port, a direct
//!   acquisition, a different account, and the *same* account all receive
//!   [`LeaseError::Busy`]. There is deliberately no idle persistent
//!   `port -> account` binding; `last_cdp_port` stays metadata.
//! * Account acquisition inserts `Verifying`, releases the map lock, awaits the
//!   caller's live verification *without holding any mutex*, then transitions
//!   only its exact id to `Active` and returns the same lease. A verification
//!   error or a dropped/cancelled future releases only that exact record.
//!
//! The map mutex is never held across an await; every critical section is a
//! short synchronous read/insert/remove. Locking is poison-safe.

use crate::account_runtime::OnlineAccountSession;
use crate::models::AccountId;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

/// Opaque, unforgeable lease identifier. The field is private and no public
/// constructor exists, so only this module can mint one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LeaseId(u64);

impl LeaseId {
    /// The raw id, for diagnostics only. It cannot be used to acquire a lease.
    #[allow(dead_code)] // Diagnostics/tests.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Who holds (or is requesting) a port lease. Secret-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdpLeaseHolder {
    Account(AccountId),
    Direct,
}

impl CdpLeaseHolder {
    fn describe(&self) -> String {
        match self {
            CdpLeaseHolder::Account(id) => format!("account {}", id.as_str()),
            CdpLeaseHolder::Direct => "a direct/scratch CDP operation".to_string(),
        }
    }
}

/// Where a record is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeasePhase {
    /// An account lease awaiting live identity verification.
    Verifying,
    /// A live lease that may drive the port.
    Active,
}

/// A read-only view of a port's record, for exit/diagnostics and tests.
#[allow(dead_code)] // Read-only diagnostics accessor surface used by tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseSnapshot {
    pub id: LeaseId,
    pub holder: CdpLeaseHolder,
    pub phase: LeasePhase,
}

/// Secret-free lease acquisition/lifecycle error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// Some holder already owns (or is verifying) the port. Includes the same
    /// account as the requester, because a port is globally exclusive.
    Busy {
        port: u16,
        holder: CdpLeaseHolder,
        requester: CdpLeaseHolder,
    },
    /// The record was removed or replaced before the lease could activate.
    Stale { port: u16 },
    /// Lease id space is exhausted; acquisition fails closed (never wraps).
    Exhausted,
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::Busy {
                port,
                holder,
                requester,
            } => write!(
                formatter,
                "cdp_port_conflict: CDP port {port} is held by {}; {} cannot acquire a lease. Another CDP operation must finish or release the port first.",
                holder.describe(),
                requester.describe()
            ),
            LeaseError::Stale { port } => write!(
                formatter,
                "cdp_port_lease_stale: CDP port {port} lease no longer matches the active record; activation was refused."
            ),
            LeaseError::Exhausted => formatter.write_str(
                "cdp_port_lease_exhausted: CDP port lease identifiers are exhausted; refusing to acquire a lease.",
            ),
        }
    }
}

impl std::error::Error for LeaseError {}

/// Result of an account acquisition: a secret-free lease failure, or the live
/// verification's own (already secret-free) error when it rejected the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountAcquireError<E> {
    Lease(LeaseError),
    Verification(E),
}

impl<E> From<LeaseError> for AccountAcquireError<E> {
    fn from(error: LeaseError) -> Self {
        AccountAcquireError::Lease(error)
    }
}

impl<E: std::fmt::Display> std::fmt::Display for AccountAcquireError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountAcquireError::Lease(error) => write!(formatter, "{error}"),
            AccountAcquireError::Verification(error) => {
                write!(formatter, "cdp_port_lease_verification_failed: {error}")
            }
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for AccountAcquireError<E> {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaseRecord {
    id: LeaseId,
    holder: CdpLeaseHolder,
    phase: LeasePhase,
}

/// The single all-ports direct maintenance record (e.g. `--restore-normal-all`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalRecord {
    id: LeaseId,
    holder: CdpLeaseHolder,
}

#[derive(Debug, Default)]
struct Inner {
    /// Last allocated id. Starts at 0; allocation is `checked_add(1)` so the id
    /// space can never wrap.
    next_id: u64,
    records: HashMap<u16, LeaseRecord>,
    global: Option<GlobalRecord>,
}

impl Inner {
    fn allocate_id(&mut self) -> Option<LeaseId> {
        let id = self.next_id.checked_add(1)?;
        self.next_id = id;
        Some(LeaseId(id))
    }
}

/// The authoritative in-memory registry of CDP port leases.
#[derive(Debug, Default)]
pub struct CdpPortLeases {
    inner: Arc<Mutex<Inner>>,
}

/// A live (or verifying) lease on one CDP port. Non-cloneable: it is the only
/// authority that may clear its own record.
#[allow(dead_code)] // Read-only accessors are used by diagnostics/tests.
#[derive(Debug)]
pub struct CdpPortLease {
    inner: Arc<Mutex<Inner>>,
    port: u16,
    id: LeaseId,
    holder: CdpLeaseHolder,
}

#[allow(dead_code)] // Read-only accessors are used by diagnostics/tests.
impl CdpPortLease {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn id(&self) -> LeaseId {
        self.id
    }

    pub fn holder(&self) -> &CdpLeaseHolder {
        &self.holder
    }
}

impl Drop for CdpPortLease {
    fn drop(&mut self) {
        let mut inner = lock(&self.inner);
        let matches = inner
            .records
            .get(&self.port)
            .is_some_and(|record| record.id == self.id);
        if matches {
            inner.records.remove(&self.port);
        }
    }
}

/// An all-ports direct maintenance lease (e.g. a global `--restore-normal-all`)
/// that atomically conflicts with every per-port record. Non-cloneable and
/// exact-ID: `Drop` clears the global record only while its own id still matches,
/// so a stale lease can never clear a newer one.
#[derive(Debug)]
pub struct GlobalMaintenanceLease {
    inner: Arc<Mutex<Inner>>,
    id: LeaseId,
}

impl Drop for GlobalMaintenanceLease {
    fn drop(&mut self) {
        let mut inner = lock(&self.inner);
        if inner
            .global
            .as_ref()
            .is_some_and(|record| record.id == self.id)
        {
            inner.global = None;
        }
    }
}

fn lock(inner: &Arc<Mutex<Inner>>) -> MutexGuard<'_, Inner> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl CdpPortLeases {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// Atomically acquire an active direct/scratch lease, only while the port is
    /// completely unheld.
    pub fn acquire_direct(&self, port: u16) -> Result<CdpPortLease, LeaseError> {
        let mut inner = lock(&self.inner);
        if let Some(global) = &inner.global {
            return Err(LeaseError::Busy {
                port,
                holder: global.holder.clone(),
                requester: CdpLeaseHolder::Direct,
            });
        }
        if let Some(record) = inner.records.get(&port) {
            return Err(LeaseError::Busy {
                port,
                holder: record.holder.clone(),
                requester: CdpLeaseHolder::Direct,
            });
        }
        let id = inner.allocate_id().ok_or(LeaseError::Exhausted)?;
        let holder = CdpLeaseHolder::Direct;
        inner.records.insert(
            port,
            LeaseRecord {
                id,
                holder: holder.clone(),
                phase: LeasePhase::Active,
            },
        );
        Ok(CdpPortLease {
            inner: Arc::clone(&self.inner),
            port,
            id,
            holder,
        })
    }

    /// Acquire an account lease through a live-verification transaction.
    ///
    /// An [`AccountId`] alone is deliberately not sufficient: the caller must
    /// present a coherent [`OnlineAccountSession`], which is evidence that a
    /// matching authenticated user *and* client were present on the same active
    /// runtime at snapshot time. The holder is derived from that session.
    ///
    /// The record is inserted as `Verifying` under the map lock, the lock is
    /// released, `verify` is awaited with no mutex held, and then the exact id
    /// is transitioned to `Active`. If verification fails, the future is
    /// cancelled/dropped, or the record no longer matches, only this
    /// transaction's own record is released and the error is returned.
    pub async fn acquire_account<F, Fut, E>(
        &self,
        port: u16,
        session: &OnlineAccountSession,
        verify: F,
    ) -> Result<CdpPortLease, AccountAcquireError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let holder = CdpLeaseHolder::Account(session.account_id().clone());
        let lease = {
            let mut inner = lock(&self.inner);
            if let Some(global) = &inner.global {
                return Err(AccountAcquireError::Lease(LeaseError::Busy {
                    port,
                    holder: global.holder.clone(),
                    requester: holder,
                }));
            }
            if let Some(record) = inner.records.get(&port) {
                return Err(AccountAcquireError::Lease(LeaseError::Busy {
                    port,
                    holder: record.holder.clone(),
                    requester: holder,
                }));
            }
            let id = inner
                .allocate_id()
                .ok_or(LeaseError::Exhausted)
                .map_err(AccountAcquireError::Lease)?;
            inner.records.insert(
                port,
                LeaseRecord {
                    id,
                    holder: holder.clone(),
                    phase: LeasePhase::Verifying,
                },
            );
            CdpPortLease {
                inner: Arc::clone(&self.inner),
                port,
                id,
                holder,
            }
        };

        match verify().await {
            Ok(()) => {
                let mut inner = lock(&self.inner);
                match inner.records.get_mut(&port) {
                    Some(record) if record.id == lease.id => {
                        record.phase = LeasePhase::Active;
                        Ok(lease)
                    }
                    // The record was removed or replaced while verifying: refuse
                    // to resurrect it. `lease` is dropped on return, and its Drop
                    // only removes its own exact id (which no longer matches).
                    _ => Err(AccountAcquireError::Lease(LeaseError::Stale { port })),
                }
            }
            // Returning here drops `lease`, which releases exactly its own record.
            Err(error) => Err(AccountAcquireError::Verification(error)),
        }
    }

    /// Atomically acquire the single all-ports direct maintenance lease. It
    /// conflicts with every per-port `Verifying`/`Active` record (and with an
    /// existing global lease), and every per-port acquisition conflicts while it
    /// is held. There is no force-release path; the lease releases on drop.
    pub fn acquire_global_maintenance(&self) -> Result<GlobalMaintenanceLease, LeaseError> {
        let mut inner = lock(&self.inner);
        if let Some(record) = &inner.global {
            return Err(LeaseError::Busy {
                port: 0,
                holder: record.holder.clone(),
                requester: CdpLeaseHolder::Direct,
            });
        }
        if let Some((port, record)) = inner.records.iter().min_by_key(|(port, _)| **port) {
            return Err(LeaseError::Busy {
                port: *port,
                holder: record.holder.clone(),
                requester: CdpLeaseHolder::Direct,
            });
        }
        let id = inner.allocate_id().ok_or(LeaseError::Exhausted)?;
        inner.global = Some(GlobalRecord {
            id,
            holder: CdpLeaseHolder::Direct,
        });
        Ok(GlobalMaintenanceLease {
            inner: Arc::clone(&self.inner),
            id,
        })
    }

    /// Read-only: the current global-maintenance holder, if any.
    #[allow(dead_code)] // Read-only diagnostics/tests.
    pub fn global_holder(&self) -> Option<CdpLeaseHolder> {
        lock(&self.inner)
            .global
            .as_ref()
            .map(|record| record.holder.clone())
    }

    /// Read-only: the current holder of `port`, if any.
    #[allow(dead_code)] // Read-only diagnostics/tests.
    pub fn holder(&self, port: u16) -> Option<CdpLeaseHolder> {
        lock(&self.inner)
            .records
            .get(&port)
            .map(|record| record.holder.clone())
    }

    /// Read-only view of `port`'s record, for exit/diagnostics and tests. There is
    /// deliberately no force-release API.
    #[allow(dead_code)] // Read-only diagnostics/tests.
    pub fn snapshot(&self, port: u16) -> Option<LeaseSnapshot> {
        lock(&self.inner)
            .records
            .get(&port)
            .map(|record| LeaseSnapshot {
                id: record.id,
                holder: record.holder.clone(),
                phase: record.phase,
            })
    }

    /// Whether any CDP port lease (verifying or active) is currently held. Exit
    /// uses this to refuse to complete while a live operation still owns a port;
    /// there is no force-release API.
    pub fn is_empty(&self) -> bool {
        let inner = lock(&self.inner);
        inner.records.is_empty() && inner.global.is_none()
    }

    /// Number of held leases (per-port verifying/active plus the global slot).
    pub fn len(&self) -> usize {
        let inner = lock(&self.inner);
        inner.records.len() + usize::from(inner.global.is_some())
    }

    /// Test-only: seed the id counter to exercise exhaustion without wrapping.
    #[cfg(test)]
    fn seed_next_id(&self, next_id: u64) {
        lock(&self.inner).next_id = next_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> AccountId {
        AccountId::parse(id).expect("valid test account id")
    }

    fn alice() -> AccountId {
        account("111111111111111111")
    }

    fn bob() -> AccountId {
        account("222222222222222222")
    }

    fn user(id: &str, name: &str) -> crate::models::DiscordUser {
        crate::models::DiscordUser {
            id: id.to_string(),
            username: name.to_string(),
            discriminator: "0".to_string(),
            avatar: None,
            global_name: Some(format!("{name} Display")),
            premium_type: None,
        }
    }

    /// Build a real coherent `OnlineAccountSession` through the registry, so the
    /// account-acquisition tests exercise the same authority path as production.
    fn online_session(id: &str, name: &str) -> OnlineAccountSession {
        use crate::account_runtime::AccountRegistry;
        use crate::discord_api::DiscordApiClient;
        use crate::proxy_settings::ProxyConfiguration;
        use crate::super_properties::SuperPropertiesHandle;

        let path = std::env::temp_dir().join(format!(
            "dqh-lease-session-{name}-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let registry = AccountRegistry::new(path.clone());
        let user = user(id, name);
        let runtime = registry.ensure_runtime(&user).unwrap();
        let client = DiscordApiClient::new_with_super_properties(
            "test-token".to_string(),
            ProxyConfiguration::Direct,
            SuperPropertiesHandle::new(),
        )
        .unwrap();
        runtime.publish_client(Some(client));
        runtime.mark_authenticated(&user, Some(9223), 1);
        registry
            .activate(AccountId::from_user(&user).unwrap(), runtime.profile())
            .unwrap();
        let session = registry
            .active_online_session()
            .expect("coherent online session");
        let _ = std::fs::remove_file(&path);
        session
    }

    fn alice_session() -> OnlineAccountSession {
        online_session("111111111111111111", "alice")
    }

    fn bob_session() -> OnlineAccountSession {
        online_session("222222222222222222", "bob")
    }

    #[tokio::test]
    async fn direct_lease_excludes_every_contender() {
        let leases = CdpPortLeases::new();
        let lease = leases.acquire_direct(9223).unwrap();
        assert_eq!(lease.holder(), &CdpLeaseHolder::Direct);
        assert_eq!(lease.port(), 9223);
        assert_eq!(leases.snapshot(9223).unwrap().phase, LeasePhase::Active);

        // Another direct contender is busy.
        assert_eq!(
            leases.acquire_direct(9223).unwrap_err(),
            LeaseError::Busy {
                port: 9223,
                holder: CdpLeaseHolder::Direct,
                requester: CdpLeaseHolder::Direct,
            }
        );
        // An account contender is busy against the direct holder.
        let error = leases
            .acquire_account(9223, &alice_session(), || async { Ok::<(), String>(()) })
            .await
            .unwrap_err();
        assert_eq!(
            error,
            AccountAcquireError::Lease(LeaseError::Busy {
                port: 9223,
                holder: CdpLeaseHolder::Direct,
                requester: CdpLeaseHolder::Account(alice()),
            })
        );

        drop(lease);
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.acquire_direct(9223).is_ok());
    }

    #[tokio::test]
    async fn account_lease_is_globally_exclusive_including_same_account() {
        let leases = CdpPortLeases::new();
        let lease = leases
            .acquire_account(9223, &alice_session(), || async { Ok::<(), String>(()) })
            .await
            .unwrap();
        assert_eq!(lease.holder(), &CdpLeaseHolder::Account(alice()));
        assert_eq!(leases.snapshot(9223).unwrap().phase, LeasePhase::Active);

        // The same account may not take a second lease on the same port.
        let same = leases
            .acquire_account(9223, &alice_session(), || async { Ok::<(), String>(()) })
            .await
            .unwrap_err();
        assert!(matches!(
            same,
            AccountAcquireError::Lease(LeaseError::Busy {
                holder: CdpLeaseHolder::Account(_),
                requester: CdpLeaseHolder::Account(_),
                ..
            })
        ));
        // A different account is busy.
        assert!(matches!(
            leases
                .acquire_account(9223, &bob_session(), || async { Ok::<(), String>(()) })
                .await
                .unwrap_err(),
            AccountAcquireError::Lease(LeaseError::Busy { .. })
        ));
        // A direct contender is busy.
        assert!(matches!(
            leases.acquire_direct(9223),
            Err(LeaseError::Busy { .. })
        ));

        drop(lease);
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.holder(9223).is_none());
    }

    #[tokio::test]
    async fn verification_window_blocks_all_contenders_until_activation() {
        let leases = Arc::new(CdpPortLeases::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel::<()>();

        let task = {
            let leases = Arc::clone(&leases);
            tokio::spawn(async move {
                leases
                    .acquire_account(9223, &alice_session(), || async move {
                        let _ = started_tx.send(());
                        let _ = proceed_rx.await;
                        Ok::<(), String>(())
                    })
                    .await
            })
        };

        started_rx.await.unwrap();
        assert_eq!(leases.snapshot(9223).unwrap().phase, LeasePhase::Verifying);

        // Every contender is busy while the record is only Verifying.
        assert!(matches!(
            leases.acquire_direct(9223),
            Err(LeaseError::Busy { .. })
        ));
        assert!(matches!(
            leases
                .acquire_account(9223, &alice_session(), || async { Ok::<(), String>(()) })
                .await
                .unwrap_err(),
            AccountAcquireError::Lease(LeaseError::Busy { .. })
        ));
        assert!(matches!(
            leases
                .acquire_account(9223, &bob_session(), || async { Ok::<(), String>(()) })
                .await
                .unwrap_err(),
            AccountAcquireError::Lease(LeaseError::Busy { .. })
        ));

        proceed_tx.send(()).unwrap();
        let lease = task.await.unwrap().unwrap();
        assert_eq!(lease.holder(), &CdpLeaseHolder::Account(alice()));
        assert_eq!(leases.snapshot(9223).unwrap().phase, LeasePhase::Active);

        drop(lease);
        assert!(leases.snapshot(9223).is_none());
    }

    #[tokio::test]
    async fn failed_verification_releases_only_its_own_record() {
        let leases = CdpPortLeases::new();
        let error = leases
            .acquire_account(9223, &alice_session(), || async {
                Err::<(), String>("mismatch".to_string())
            })
            .await
            .unwrap_err();
        assert_eq!(
            error,
            AccountAcquireError::Verification("mismatch".to_string())
        );
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.holder(9223).is_none());

        // The port is free for another account afterward.
        let lease = leases
            .acquire_account(9223, &bob_session(), || async { Ok::<(), String>(()) })
            .await
            .unwrap();
        assert_eq!(lease.holder(), &CdpLeaseHolder::Account(bob()));
    }

    #[tokio::test]
    async fn cancelled_verification_releases_only_its_own_record() {
        let leases = Arc::new(CdpPortLeases::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

        let task = {
            let leases = Arc::clone(&leases);
            tokio::spawn(async move {
                leases
                    .acquire_account(9223, &alice_session(), || async move {
                        let _ = started_tx.send(());
                        std::future::pending::<()>().await;
                        Ok::<(), String>(())
                    })
                    .await
            })
        };

        started_rx.await.unwrap();
        assert_eq!(leases.snapshot(9223).unwrap().phase, LeasePhase::Verifying);

        // Cancelling the acquisition drops the future, releasing its own record.
        task.abort();
        assert!(task.await.is_err());
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.acquire_direct(9223).is_ok());
    }

    #[test]
    fn stale_lease_drop_cannot_remove_a_newer_lease() {
        let leases = CdpPortLeases::new();
        let first = leases.acquire_direct(9223).unwrap();
        let first_id = first.id();
        let first_port = first.port();
        drop(first);
        assert!(leases.snapshot(9223).is_none());

        let second = leases.acquire_direct(9223).unwrap();
        let second_id = second.id();
        assert_ne!(first_id, second_id);

        // A stale handle for the old id must not touch the newer record.
        let stale = CdpPortLease {
            inner: Arc::clone(&leases.inner),
            port: first_port,
            id: first_id,
            holder: CdpLeaseHolder::Direct,
        };
        drop(stale);
        assert_eq!(leases.snapshot(9223).unwrap().id, second_id);

        drop(second);
        assert!(leases.snapshot(9223).is_none());
    }

    #[tokio::test]
    async fn normal_drop_releases_direct_and_account_leases() {
        let leases = CdpPortLeases::new();

        let direct = leases.acquire_direct(9223).unwrap();
        drop(direct);
        assert!(leases.snapshot(9223).is_none());

        let account_lease = leases
            .acquire_account(9224, &alice_session(), || async { Ok::<(), String>(()) })
            .await
            .unwrap();
        drop(account_lease);
        assert!(leases.snapshot(9224).is_none());
    }

    #[tokio::test]
    async fn activation_refuses_to_recreate_when_record_was_replaced() {
        let leases = Arc::new(CdpPortLeases::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel::<()>();

        let task = {
            let leases = Arc::clone(&leases);
            tokio::spawn(async move {
                leases
                    .acquire_account(9223, &alice_session(), || async move {
                        let _ = started_tx.send(());
                        let _ = proceed_rx.await;
                        Ok::<(), String>(())
                    })
                    .await
            })
        };
        started_rx.await.unwrap();

        // Replace the verifying record with a newer, unrelated one.
        let replacement_id = LeaseId(9_999);
        {
            let mut inner = lock(&leases.inner);
            inner.records.insert(
                9223,
                LeaseRecord {
                    id: replacement_id,
                    holder: CdpLeaseHolder::Account(bob()),
                    phase: LeasePhase::Active,
                },
            );
        }

        proceed_tx.send(()).unwrap();
        let result = task.await.unwrap();
        assert!(matches!(
            result,
            Err(AccountAcquireError::Lease(LeaseError::Stale { port: 9223 }))
        ));
        // The replacement survives; the stale activation neither recreated nor
        // removed it.
        let snapshot = leases.snapshot(9223).unwrap();
        assert_eq!(snapshot.id, replacement_id);
        assert_eq!(snapshot.holder, CdpLeaseHolder::Account(bob()));
    }

    #[tokio::test]
    async fn activation_refuses_when_record_was_removed() {
        let leases = Arc::new(CdpPortLeases::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel::<()>();

        let task = {
            let leases = Arc::clone(&leases);
            tokio::spawn(async move {
                leases
                    .acquire_account(9223, &alice_session(), || async move {
                        let _ = started_tx.send(());
                        let _ = proceed_rx.await;
                        Ok::<(), String>(())
                    })
                    .await
            })
        };
        started_rx.await.unwrap();

        lock(&leases.inner).records.remove(&9223);
        proceed_tx.send(()).unwrap();

        let result = task.await.unwrap();
        assert!(matches!(
            result,
            Err(AccountAcquireError::Lease(LeaseError::Stale { port: 9223 }))
        ));
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.acquire_direct(9223).is_ok());
    }

    #[tokio::test]
    async fn id_exhaustion_fails_closed_without_inserting() {
        let leases = CdpPortLeases::new();
        leases.seed_next_id(u64::MAX);
        assert_eq!(
            leases.acquire_direct(9223).unwrap_err(),
            LeaseError::Exhausted
        );
        assert!(leases.snapshot(9223).is_none());

        let error = leases
            .acquire_account(9224, &alice_session(), || async { Ok::<(), String>(()) })
            .await
            .unwrap_err();
        assert_eq!(error, AccountAcquireError::Lease(LeaseError::Exhausted));
        assert!(leases.snapshot(9224).is_none());

        // A near-exhausted counter yields the last id then fails closed; it never
        // wraps to a previously issued id.
        let leases = CdpPortLeases::new();
        leases.seed_next_id(u64::MAX - 1);
        let last = leases.acquire_direct(9223).unwrap();
        assert_eq!(last.id().as_u64(), u64::MAX);
        drop(last);
        assert_eq!(
            leases.acquire_direct(9223).unwrap_err(),
            LeaseError::Exhausted
        );
    }

    #[test]
    fn busy_message_is_stable_and_secret_free() {
        let busy = LeaseError::Busy {
            port: 9223,
            holder: CdpLeaseHolder::Account(alice()),
            requester: CdpLeaseHolder::Direct,
        };
        let message = busy.to_string();
        assert!(message.starts_with("cdp_port_conflict:"));
        assert!(message.contains("111111111111111111"));
        assert!(message.contains("direct/scratch CDP operation"));

        let direct_holder = LeaseError::Busy {
            port: 9224,
            holder: CdpLeaseHolder::Direct,
            requester: CdpLeaseHolder::Account(bob()),
        };
        assert!(direct_holder
            .to_string()
            .contains("a direct/scratch CDP operation"));
        assert!(direct_holder.to_string().contains("222222222222222222"));

        assert!(LeaseError::Exhausted
            .to_string()
            .starts_with("cdp_port_lease_exhausted:"));
        assert!(LeaseError::Stale { port: 9225 }
            .to_string()
            .starts_with("cdp_port_lease_stale:"));
    }

    #[tokio::test]
    async fn snapshot_and_holder_are_read_only_views() {
        let leases = CdpPortLeases::new();
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.holder(9223).is_none());

        let lease = leases
            .acquire_account(9223, &alice_session(), || async { Ok::<(), String>(()) })
            .await
            .unwrap();
        assert_eq!(leases.holder(9223), Some(CdpLeaseHolder::Account(alice())));
        let snapshot = leases.snapshot(9223).unwrap();
        assert_eq!(snapshot.phase, LeasePhase::Active);
        assert_eq!(snapshot.id, lease.id());

        // Taking a snapshot never changes ownership.
        assert_eq!(leases.snapshot(9223).unwrap(), snapshot);
    }

    // --- Global (all-ports) maintenance lease --------------------------------

    #[tokio::test]
    async fn global_maintenance_blocks_every_per_port_acquisition() {
        let leases = CdpPortLeases::new();
        let global = leases.acquire_global_maintenance().unwrap();
        assert!(leases.global_holder().is_some());
        assert!(!leases.is_empty());
        assert_eq!(leases.len(), 1);

        // Direct and account per-port acquisitions are busy while global holds.
        assert!(matches!(
            leases.acquire_direct(9223),
            Err(LeaseError::Busy { .. })
        ));
        assert!(matches!(
            leases
                .acquire_account(9223, &alice_session(), || async { Ok::<(), String>(()) })
                .await,
            Err(AccountAcquireError::Lease(LeaseError::Busy { .. }))
        ));
        // A second global is busy too.
        assert!(matches!(
            leases.acquire_global_maintenance(),
            Err(LeaseError::Busy { .. })
        ));

        drop(global);
        assert!(leases.global_holder().is_none());
        assert!(leases.is_empty());
        assert!(leases.acquire_direct(9223).is_ok());
    }

    #[tokio::test]
    async fn per_port_lease_blocks_global_maintenance() {
        let leases = CdpPortLeases::new();
        let port_lease = leases.acquire_direct(9223).unwrap();
        let error = leases.acquire_global_maintenance().unwrap_err();
        assert!(matches!(error, LeaseError::Busy { port: 9223, .. }));
        drop(port_lease);
        assert!(leases.acquire_global_maintenance().is_ok());
    }

    #[tokio::test]
    async fn verifying_account_lease_blocks_global_maintenance() {
        let leases = Arc::new(CdpPortLeases::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel::<()>();
        let task = {
            let leases = Arc::clone(&leases);
            tokio::spawn(async move {
                leases
                    .acquire_account(9223, &alice_session(), || async move {
                        let _ = started_tx.send(());
                        let _ = proceed_rx.await;
                        Ok::<(), String>(())
                    })
                    .await
            })
        };
        started_rx.await.unwrap();
        assert_eq!(leases.snapshot(9223).unwrap().phase, LeasePhase::Verifying);
        assert!(matches!(
            leases.acquire_global_maintenance(),
            Err(LeaseError::Busy { .. })
        ));

        proceed_tx.send(()).unwrap();
        let lease = task.await.unwrap().unwrap();
        assert!(leases.acquire_global_maintenance().is_err());
        drop(lease);
        assert!(leases.acquire_global_maintenance().is_ok());
    }

    #[test]
    fn stale_global_drop_cannot_clear_a_newer_global() {
        let leases = CdpPortLeases::new();
        let first = leases.acquire_global_maintenance().unwrap();
        let first_id = first.id;
        drop(first);
        assert!(leases.global_holder().is_none());

        let second = leases.acquire_global_maintenance().unwrap();
        let second_id = second.id;
        assert_ne!(first_id, second_id);

        // A stale handle for the old id must not clear the newer global.
        let stale = GlobalMaintenanceLease {
            inner: Arc::clone(&leases.inner),
            id: first_id,
        };
        drop(stale);
        assert!(leases.global_holder().is_some());
        assert!(leases.acquire_global_maintenance().is_err());

        drop(second);
        assert!(leases.global_holder().is_none());
        assert!(leases.is_empty());
    }
}
