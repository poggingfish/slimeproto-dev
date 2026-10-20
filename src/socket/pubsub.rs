use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
};

use sockudo_ws::Message;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

const MAX_MESSAGES: usize = 256;
const MAX_BYTES: usize = 1024 * 1024;

pub(crate) type SubscriberId = u64;

pub(super) struct Publication {
    message: Message,
    budget: OwnedSemaphorePermit,
}

impl Publication {
    pub(super) fn into_parts(self) -> (Message, OwnedSemaphorePermit) {
        (self.message, self.budget)
    }
}

struct Subscriber {
    sender: mpsc::Sender<Publication>,
    budget: Arc<Semaphore>,
    overflow: oneshot::Sender<()>,
    topics: HashSet<String>,
}

#[derive(Default)]
struct Registry {
    next_id: SubscriberId,
    subscribers: HashMap<SubscriberId, Subscriber>,
    topics: HashMap<String, HashSet<SubscriberId>>,
}

impl Registry {
    fn remove(&mut self, id: SubscriberId) -> Option<Subscriber> {
        let subscriber = self.subscribers.remove(&id)?;
        for topic in &subscriber.topics {
            if let Some(members) = self.topics.get_mut(topic) {
                members.remove(&id);
                if members.is_empty() {
                    self.topics.remove(topic);
                }
            }
        }
        Some(subscriber)
    }
}

#[derive(Default)]
pub(crate) struct PubSub(Mutex<Registry>);

impl PubSub {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(super) fn create_subscriber(
        &self,
    ) -> (
        SubscriberId,
        mpsc::Receiver<Publication>,
        oneshot::Receiver<()>,
    ) {
        let (sender, receiver) = mpsc::channel(MAX_MESSAGES);
        let (overflow, overloaded) = oneshot::channel();
        let mut registry = self.registry();
        registry.next_id = registry
            .next_id
            .checked_add(1)
            .expect("subscriber IDs exhausted");
        let id = registry.next_id;
        registry.subscribers.insert(
            id,
            Subscriber {
                sender,
                budget: Arc::new(Semaphore::new(MAX_BYTES)),
                overflow,
                topics: HashSet::new(),
            },
        );
        (id, receiver, overloaded)
    }

    pub(super) fn remove_subscriber(&self, id: SubscriberId) {
        self.registry().remove(id);
    }

    pub(super) fn is_subscribed(&self, id: SubscriberId, topic: &str) -> bool {
        self.registry()
            .subscribers
            .get(&id)
            .is_some_and(|sub| sub.topics.contains(topic))
    }

    pub(super) fn subscribe(&self, id: SubscriberId, topic: &str) -> bool {
        let mut registry = self.registry();
        let Some(subscriber) = registry.subscribers.get_mut(&id) else {
            return false;
        };
        if !subscriber.topics.insert(topic.into()) {
            return false;
        }
        registry.topics.entry(topic.into()).or_default().insert(id);
        true
    }

    pub(super) fn unsubscribe(&self, id: SubscriberId, topic: &str) {
        let mut registry = self.registry();
        if let Some(subscriber) = registry.subscribers.get_mut(&id) {
            subscriber.topics.remove(topic);
        }
        if let Some(members) = registry.topics.get_mut(topic) {
            members.remove(&id);
            if members.is_empty() {
                registry.topics.remove(topic);
            }
        }
    }

    /// Returns the number of sockets that accepted the publication into their queue.
    pub(crate) fn publish(&self, topic: &str, message: Message) -> usize {
        let bytes = match &message {
            Message::Text(bytes)
            | Message::Binary(bytes)
            | Message::Ping(bytes)
            | Message::Pong(bytes) => bytes.len(),
            Message::Close(reason) => reason.as_ref().map_or(2, |reason| 2 + reason.reason.len()),
        };
        let mut registry = self.registry();
        let Some(members) = registry.topics.get(topic) else {
            return 0;
        };
        let mut failed = Vec::new();
        let mut count = 0;
        for id in members {
            let Some(subscriber) = registry.subscribers.get(id) else {
                continue;
            };
            let budget = u32::try_from(bytes)
                .ok()
                .and_then(|bytes| subscriber.budget.clone().try_acquire_many_owned(bytes).ok());
            let accepted = budget.is_some_and(|budget| {
                subscriber
                    .sender
                    .try_send(Publication {
                        message: message.clone(),
                        budget,
                    })
                    .is_ok()
            });
            if accepted {
                count += 1;
            } else {
                failed.push(*id);
            }
        }
        for id in failed {
            if let Some(subscriber) = registry.remove(id) {
                let _ = subscriber.overflow.send(());
            }
        }
        count
    }
}
