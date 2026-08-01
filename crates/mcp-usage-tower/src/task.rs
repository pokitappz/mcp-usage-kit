//! Durable-task attribution storage.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use mcp_usage_core::Call;
use thiserror::Error;

/// Boxed future returned by object-safe task stores.
pub type TaskStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, TaskStoreError>> + Send + 'a>>;

/// A sanitized durable-task store failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TaskStoreError {
    /// The configured backend could not complete the operation.
    #[error("task-attribution backend unavailable")]
    BackendUnavailable,
    /// Stored data was not a valid task origin.
    #[error("task-attribution record is invalid")]
    InvalidRecord,
}

/// Persists the original priced call under a durable task ID.
pub trait TaskAttributionStore: Send + Sync {
    /// Associate a task with the call that created it.
    fn insert<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
        call: Call,
    ) -> TaskStoreFuture<'a, ()>;
    /// Resolve an association without consuming it.
    fn get<'a>(&'a self, tenant_id: &'a str, task_id: &'a str)
    -> TaskStoreFuture<'a, Option<Call>>;
    /// Atomically consume and return an association.
    ///
    /// A completed durable task can be returned by many later polls. Using this
    /// operation before recording usage guarantees that only one process can
    /// account for that task.
    fn claim<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
    ) -> TaskStoreFuture<'a, Option<Call>>;
    /// Remove an association after a terminal task is accounted for.
    fn remove<'a>(&'a self, tenant_id: &'a str, task_id: &'a str) -> TaskStoreFuture<'a, ()>;
}

const DEFAULT_MAX_TASKS: usize = 100_000;

#[derive(Debug, Default)]
struct TaskState {
    tasks: HashMap<(String, String), TaskEntry>,
    insertion_order: VecDeque<((String, String), u128)>,
    next_generation: u128,
}

#[derive(Debug)]
struct TaskEntry {
    call: Call,
    generation: u128,
}

/// Process-local task attribution for an embedded server.
pub struct InMemoryTaskStore {
    state: Mutex<TaskState>,
    max_tasks: usize,
}

impl std::fmt::Debug for InMemoryTaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryTaskStore")
            .field("live_tasks", &self.len())
            .field("max_tasks", &self.max_tasks)
            .finish_non_exhaustive()
    }
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_MAX_TASKS)
    }
}

impl InMemoryTaskStore {
    /// Construct an empty task table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a task table retaining at most `max_tasks` associations.
    ///
    /// When full, the oldest association is removed. Zero disables process-local
    /// task attribution.
    #[must_use]
    pub fn with_capacity(max_tasks: usize) -> Self {
        Self {
            state: Mutex::new(TaskState::default()),
            max_tasks,
        }
    }

    /// Number of live task associations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .len()
    }

    /// Whether the task table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn compact_order_if_needed(&self, state: &mut TaskState) {
        if state.insertion_order.len() > self.max_tasks.saturating_mul(2) {
            let TaskState {
                tasks,
                insertion_order,
                ..
            } = state;
            insertion_order.retain(|(queued, generation)| {
                tasks
                    .get(queued)
                    .is_some_and(|entry| entry.generation == *generation)
            });
        }
    }
}

impl TaskAttributionStore for InMemoryTaskStore {
    fn insert<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
        call: Call,
    ) -> TaskStoreFuture<'a, ()> {
        Box::pin(async move {
            if self.max_tasks == 0 {
                return Ok(());
            }
            let key = (tenant_id.to_owned(), task_id.to_owned());
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A durable task's origin is immutable. A reused or hostile task ID
            // must not replace the first captured price attribution.
            if state.tasks.contains_key(&key) {
                return Ok(());
            }
            while state.tasks.len() >= self.max_tasks {
                if let Some((oldest, generation)) = state.insertion_order.pop_front() {
                    if state
                        .tasks
                        .get(&oldest)
                        .is_some_and(|entry| entry.generation == generation)
                    {
                        state.tasks.remove(&oldest);
                    }
                } else {
                    break;
                }
            }
            let generation = state.next_generation;
            state.next_generation = state.next_generation.wrapping_add(1);
            state.insertion_order.push_back((key.clone(), generation));
            state.tasks.insert(key, TaskEntry { call, generation });
            Ok(())
        })
    }

    fn get<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
    ) -> TaskStoreFuture<'a, Option<Call>> {
        Box::pin(async move {
            Ok(self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tasks
                .get(&(tenant_id.to_owned(), task_id.to_owned()))
                .map(|entry| entry.call.clone()))
        })
    }

    fn claim<'a>(
        &'a self,
        tenant_id: &'a str,
        task_id: &'a str,
    ) -> TaskStoreFuture<'a, Option<Call>> {
        Box::pin(async move {
            let key = (tenant_id.to_owned(), task_id.to_owned());
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let claimed = state.tasks.remove(&key).map(|entry| entry.call);
            if claimed.is_some() {
                self.compact_order_if_needed(&mut state);
            }
            Ok(claimed)
        })
    }

    fn remove<'a>(&'a self, tenant_id: &'a str, task_id: &'a str) -> TaskStoreFuture<'a, ()> {
        Box::pin(async move {
            let key = (tenant_id.to_owned(), task_id.to_owned());
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.tasks.remove(&key);
            self.compact_order_if_needed(&mut state);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_usage_core::Method;

    fn call(name: &str) -> Call {
        Call::new(Method::ToolsCall, Some(name.to_owned()))
    }

    #[tokio::test]
    async fn capacity_is_bounded_and_task_origins_cannot_be_replaced() {
        let store = InMemoryTaskStore::with_capacity(2);
        store.insert("tenant", "one", call("first")).await.unwrap();
        store
            .insert("tenant", "one", call("replacement"))
            .await
            .unwrap();
        assert_eq!(
            store.get("tenant", "one").await.unwrap(),
            Some(call("first"))
        );

        store.insert("tenant", "two", call("second")).await.unwrap();
        store
            .insert("tenant", "three", call("third"))
            .await
            .unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.get("tenant", "one").await.unwrap().is_none());
        assert_eq!(
            store.claim("tenant", "two").await.unwrap(),
            Some(call("second"))
        );
        assert!(store.claim("tenant", "two").await.unwrap().is_none());
        assert_eq!(
            store.get("tenant", "three").await.unwrap(),
            Some(call("third"))
        );
    }

    #[tokio::test]
    async fn zero_capacity_disables_storage() {
        let store = InMemoryTaskStore::with_capacity(0);
        store.insert("tenant", "task", call("tool")).await.unwrap();
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn repeated_insert_and_claim_cycles_bound_ordering_metadata() {
        let capacity = 4;
        let store = InMemoryTaskStore::with_capacity(capacity);
        for index in 0..100 {
            let task_id = format!("task-{index}");
            store
                .insert("tenant", &task_id, call("tool"))
                .await
                .unwrap();
            assert!(store.claim("tenant", &task_id).await.unwrap().is_some());
            let state = store.state.lock().unwrap();
            assert!(state.tasks.len() <= capacity);
            assert!(state.insertion_order.len() <= capacity * 2);
        }
    }
}
