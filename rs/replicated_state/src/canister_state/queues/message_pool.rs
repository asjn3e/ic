#![allow(dead_code)]

use ic_types::messages::{
    Request, RequestOrResponse, Response, MAX_RESPONSE_COUNT_BYTES, NO_DEADLINE,
};
use ic_types::time::CoarseTime;
use ic_types::{CountBytes, Time};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::ops::{AddAssign, SubAssign};
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
mod tests;

/// The lifetime of a guaranteed response call request in an output queue, from
/// which its deadline is computed (as `now + REQUEST_LIFETIME ).
pub const REQUEST_LIFETIME: Duration = Duration::from_secs(300);

/// Bit encoding the message kind (request or response).
#[repr(u64)]
enum Kind {
    Request = 0,
    Response = Self::BIT,
}

impl Kind {
    // Message kind bit (request or response).
    const BIT: u64 = 1;
}

/// Bit encoding the message context (inbound or outbound).
#[repr(u64)]
enum Context {
    Inbound = 0,
    Outbound = Self::BIT,
}

impl Context {
    // Message context bit (inbound or outbound).
    const BIT: u64 = 1 << 1;
}

/// A unique generated identifier for a message held in a `MessagePool` that
/// also encodes the message kind (request or response) and context (incoming or
/// outgoing).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MessageId(u64);

impl MessageId {
    /// Number of `MessageId` bits used as flags.
    const BITMASK_LEN: u32 = 2;

    fn new(kind: Kind, context: Context, generator: u64) -> Self {
        Self(kind as u64 | context as u64 | generator << MessageId::BITMASK_LEN)
    }

    pub(super) fn is_response(&self) -> bool {
        self.0 & Kind::BIT == Kind::Response as u64
    }

    pub(super) fn is_outbound(&self) -> bool {
        self.0 & Context::BIT == Context::Outbound as u64
    }
}

/// A placeholder for a potential late inbound best-effort response.
///
/// Does not implement `Clone` or `Copy` to ensure that it can only be used
/// once.
pub(super) struct ResponsePlaceholder(MessageId);

impl ResponsePlaceholder {
    /// Returns the message ID within.
    pub(super) fn id(&self) -> MessageId {
        self.0
    }
}

/// A pool of canister messages, guaranteed response and best effort, with
/// built-in support for time-based expiration and load shedding.
///
/// Messages in the pool are identified by a `MessageId` generated by the pool.
/// The `MessageId` also encodes the message kind (request or response); and
/// context (inbound or outbound).
///
/// Messages are added to the deadline queue based on their class (best-effort
/// vs guaranteed response) and context: i.e. all best-effort messages except
/// responses in input queues; plus guaranteed response call requests in output
/// queues. All best-effort messages (and only best-effort messages) are added
/// to the load shedding queue.
///
/// All pool operations except `expire_messages()` and
/// `calculate_memory_usage_stats()` (only used during deserialization) execute
/// in at most `O(log(N))` time.
#[derive(Clone, Debug)]
pub struct MessagePool {
    /// Pool contents.
    messages: BTreeMap<MessageId, RequestOrResponse>,

    /// Running memory usage stats for the pool.
    memory_usage_stats: MemoryUsageStats,

    /// Deadline priority queue, earliest deadlines first.
    ///
    /// Message IDs break ties, ensuring deterministic representation across
    /// replicas.
    deadline_queue: BinaryHeap<(Reverse<CoarseTime>, MessageId)>,

    /// Load shedding priority queue: largest message first.
    ///
    /// Message IDs break ties, ensuring deterministic representation across
    /// replicas.
    size_queue: BinaryHeap<(usize, MessageId)>,

    /// A monotonically increasing counter used to generate unique message IDs.
    next_message_id_generator: u64,
}

impl MessagePool {
    /// Inserts an inbound message (one that is to be enqueued in an input queue)
    /// into the pool. Returns the ID assigned to the message.
    ///
    /// The message is added to the deadline queue iff it is a best-effort request
    /// (best effort responses that already made it into an input queue should not
    /// expire). It is added to the load shedding queue if it is a best-effort
    /// message.
    pub(crate) fn insert_inbound(&mut self, msg: RequestOrResponse) -> MessageId {
        let deadline = match &msg {
            RequestOrResponse::Request(request) => request.deadline,

            // Never expire responses already enqueued in an input queue.
            RequestOrResponse::Response(_) => NO_DEADLINE,
        };

        self.insert_impl(msg, deadline, Context::Inbound)
    }

    /// Inserts an outbound request (one that is to be enqueued in an output queue)
    /// into the pool. Returns the ID assigned to the request.
    ///
    /// The request is always added to the deadline queue: if it is a best-effort
    /// request, with its explicit deadline; if it is a guaranteed response call
    /// request, with a deadline of `now + REQUEST_LIFETIME`. It is added to the
    /// load shedding queue iff it is a best-effort request.
    pub(crate) fn insert_outbound_request(
        &mut self,
        request: Arc<Request>,
        now: Time,
    ) -> MessageId {
        let deadline = if request.deadline == NO_DEADLINE {
            // Guaranteed response call requests in canister output queues expire after
            // `REQUEST_LIFETIME`.
            CoarseTime::floor(now + REQUEST_LIFETIME)
        } else {
            // Best-effort requests expire as per their specified deadline.
            request.deadline
        };

        self.insert_impl(
            RequestOrResponse::Request(request),
            deadline,
            Context::Outbound,
        )
    }

    /// Inserts an outbound response (one that is to be enqueued in an output queue)
    /// into the pool. Returns the ID assigned to the response.
    ///
    /// The response is added to both the deadline queue and the load shedding queue
    /// iff it is a best-effort response.
    pub(crate) fn insert_outbound_response(&mut self, response: Arc<Response>) -> MessageId {
        let deadline = response.deadline;
        self.insert_impl(
            RequestOrResponse::Response(response),
            deadline,
            Context::Outbound,
        )
    }

    /// Inserts the given message into the pool with the provided `deadline` (rather
    /// than the message's actual deadline; this is so we can expire the outgoing
    /// requests of guaranteed response calls; and not expire incoming best-effort
    /// responses). Returns the ID assigned to the message.
    ///
    /// The message is recorded into the deadline queue with the provided `deadline`
    /// iff that is non-zero. It is recorded in the load shedding priority queue iff
    /// the message is a best-effort message.
    fn insert_impl(
        &mut self,
        msg: RequestOrResponse,
        deadline: CoarseTime,
        context: Context,
    ) -> MessageId {
        let kind = match &msg {
            RequestOrResponse::Request(_) => Kind::Request,
            RequestOrResponse::Response(_) => Kind::Response,
        };
        let id = self.next_message_id(kind, context);
        let size_bytes = msg.count_bytes();
        let is_best_effort = msg.is_best_effort();

        // Update memory usage stats.
        self.memory_usage_stats += MemoryUsageStats::stats_delta(&msg);

        // Insert.
        assert!(self.messages.insert(id, msg).is_none());
        debug_assert_eq!(self.calculate_memory_usage_stats(), self.memory_usage_stats);

        // Record in deadline queue iff a deadline was provided.
        if deadline != NO_DEADLINE {
            self.deadline_queue.push((Reverse(deadline), id));
        }

        // Record in load shedding queue iff it's a best-effort message.
        if is_best_effort {
            self.size_queue.push((size_bytes, id));
        }

        id
    }

    /// Prepares a placeholder for a potential late inbound best-effort response.
    pub(super) fn insert_inbound_timeout_response(&mut self) -> ResponsePlaceholder {
        ResponsePlaceholder(self.next_message_id(Kind::Response, Context::Inbound))
    }

    /// Inserts a late inbound best-effort response into a response placeholder.
    pub(super) fn replace_inbound_timeout_response(
        &mut self,
        placeholder: ResponsePlaceholder,
        msg: RequestOrResponse,
    ) {
        // Message must be a best-effort response.
        match &msg {
            RequestOrResponse::Response(rep) if rep.deadline != NO_DEADLINE => {}
            _ => panic!("Message must be a best-effort response"),
        }

        let id = placeholder.0;
        let size_bytes = msg.count_bytes();

        // Update memory usage stats.
        self.memory_usage_stats += MemoryUsageStats::stats_delta(&msg);

        // Insert. Cannot lead to a conflict because the placeholder is consumed on use.
        assert!(self.messages.insert(id, msg).is_none());
        debug_assert_eq!(self.calculate_memory_usage_stats(), self.memory_usage_stats);

        // Record in load shedding queue only.
        self.size_queue.push((size_bytes, id));
    }

    /// Reserves and returns a new message ID.
    fn next_message_id(&mut self, kind: Kind, context: Context) -> MessageId {
        let id = MessageId::new(kind, context, self.next_message_id_generator);
        self.next_message_id_generator += 1;
        id
    }

    /// Retrieves the request with the given `MessageId`.
    ///
    /// Panics if the provided ID was generated for a `Response`.
    pub(crate) fn get_request(&self, id: MessageId) -> Option<&RequestOrResponse> {
        assert!(!id.is_response());

        self.messages.get(&id)
    }

    /// Retrieves the response with the given `MessageId`.
    ///
    /// Panics if the provided ID was generated for a `Request`.
    pub(crate) fn get_response(&self, id: MessageId) -> Option<&RequestOrResponse> {
        assert!(id.is_response());

        self.messages.get(&id)
    }

    /// Retrieves the message identified by the given reference.
    pub(crate) fn get(&self, id: MessageId) -> Option<&RequestOrResponse> {
        self.messages.get(&id)
    }

    /// Removes the message identified by the given reference from the pool.
    ///
    /// Updates the stats; and prunes the priority queues if necessary.
    pub(crate) fn take(&mut self, id: MessageId) -> Option<RequestOrResponse> {
        let msg = self.messages.remove(&id)?;

        self.memory_usage_stats -= MemoryUsageStats::stats_delta(&msg);
        debug_assert_eq!(self.calculate_memory_usage_stats(), self.memory_usage_stats);

        self.maybe_trim_queues();

        Some(msg)
    }

    /// Queries whether the deadline at the head of the deadline queue has expired.
    ///
    /// This is a fast check, but it may produce false positives if the message at
    /// the head of the deadline queue has already been removed from the pool.
    ///
    /// Time complexity: `O(1)`.
    pub(crate) fn has_expired_deadlines(&self, now: Time) -> bool {
        if let Some((deadline, _)) = self.deadline_queue.peek() {
            let now = CoarseTime::floor(now);
            if deadline.0 < now {
                return true;
            }
        }
        false
    }

    /// Removes and returns all messages with expired deadlines (i.e. `deadline <
    /// now`).
    ///
    /// Amortized time complexity per expired message: `O(log(n))`.
    pub(crate) fn expire_messages(&mut self, now: Time) -> Vec<(MessageId, RequestOrResponse)> {
        if self.deadline_queue.is_empty() {
            return Vec::new();
        }

        let now = CoarseTime::floor(now);
        let mut expired = Vec::new();
        while let Some((deadline, id)) = self.deadline_queue.peek() {
            if deadline.0 >= now {
                break;
            }
            let id = *id;

            // Pop the deadline queue entry.
            self.deadline_queue.pop();

            // Drop the message, if present.
            if let Some(msg) = self.take(id) {
                expired.push((id, msg))
            }
        }

        expired
    }

    /// Removes and returns the largest message in the pool.
    pub(crate) fn shed_largest_message(&mut self) -> Option<(MessageId, RequestOrResponse)> {
        // Keep trying until we actually drop a message.
        while let Some((_, id)) = self.size_queue.pop() {
            if let Some(msg) = self.take(id) {
                return Some((id, msg));
            }
        }

        // Nothing to shed.
        None
    }

    /// Returns the number of messages in the pool.
    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    /// Returns the memory usage of the best-effort messages in the pool.
    pub(crate) fn best_effort_memory_usage(&self) -> usize {
        self.memory_usage_stats.best_effort_message_bytes
    }

    /// Returns the memory usage of the guaranteed response messages in the pool,
    /// excluding memory reservations for guaranteed responses.
    pub(crate) fn memory_usage(&self) -> usize {
        self.memory_usage_stats.memory_usage()
    }

    /// Returns the sum total of the byte size of all guaranteed responses in the
    /// pool.
    pub(crate) fn guaranteed_responses_size_bytes(&self) -> usize {
        self.memory_usage_stats.guaranteed_responses_size_bytes
    }

    /// Returns the sum total of bytes above `MAX_RESPONSE_COUNT_BYTES` per
    /// oversized guaranteed response call request.
    pub(crate) fn oversized_guaranteed_requests_extra_bytes(&self) -> usize {
        self.memory_usage_stats
            .oversized_guaranteed_requests_extra_bytes
    }

    /// Prunes stale entries from the priority queues if they make up more than half
    /// of the respective priority queue. This ensures amortized constant time for
    /// the priority queues.
    fn maybe_trim_queues(&mut self) {
        let len = self.messages.len();

        if self.deadline_queue.len() > 2 * len + 2 {
            self.deadline_queue
                .retain(|&(_, id)| self.messages.contains_key(&id));
        }
        if self.size_queue.len() > 2 * len + 2 {
            self.size_queue
                .retain(|&(_, id)| self.messages.contains_key(&id));
        }
    }

    /// Computes memory usage stats from scratch. Used when deserializing and in
    /// `debug_assert!()` checks.
    ///
    /// Time complexity: `O(n)`.
    fn calculate_memory_usage_stats(&self) -> MemoryUsageStats {
        let mut stats = MemoryUsageStats::default();
        for msg in self.messages.values() {
            stats += MemoryUsageStats::stats_delta(msg);
        }
        stats
    }
}

impl PartialEq for MessagePool {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            messages,
            memory_usage_stats: _,
            deadline_queue,
            size_queue,
            next_message_id_generator,
        } = self;
        let Self {
            messages: other_messages,
            memory_usage_stats: _,
            deadline_queue: other_deadline_queue,
            size_queue: other_size_queue,
            next_message_id_generator: other_next_message_id_generator,
        } = other;

        messages == other_messages
            // Memory usage stats are implied by the contents of the pool.
            // && memory_usage_stats == other_memory_usage_stats
            && deadline_queue.len() == other_deadline_queue.len()
            && deadline_queue
                .iter()
                .zip(other_deadline_queue.iter())
                .all(|(entry, other_entry)| entry == other_entry)
            && size_queue.len() == other_size_queue.len()
            && size_queue
                .iter()
                .zip(other_size_queue.iter())
                .all(|(entry, other_entry)| entry == other_entry)
            && next_message_id_generator == other_next_message_id_generator
    }
}
impl Eq for MessagePool {}

impl Default for MessagePool {
    fn default() -> Self {
        Self {
            messages: Default::default(),
            memory_usage_stats: Default::default(),
            deadline_queue: Default::default(),
            size_queue: Default::default(),
            next_message_id_generator: 0,
        }
    }
}

/// Running memory utilization stats for input and output queues: total byte
/// size of all responses in input and output queues; and total reservations in
/// input and output queues.
///
/// Memory allocation of output responses in streams is tracked separately, at
/// the replicated state level (as the canister may be migrated to a different
/// subnet with outstanding responses still left in this subnet's streams).
///
/// Separate from [`InputQueuesStats`] because the resulting `stats_delta()`
/// method would become quite cumbersome with an extra `QueueType` argument and
/// a `QueueOp` that only applied to memory usage stats; and would result in
/// adding lots of zeros in lots of places.
#[derive(Clone, Debug, Default, Eq)]
pub(super) struct MemoryUsageStats {
    /// Sum total of the byte size of all best-effort messages in the pool.
    best_effort_message_bytes: usize,

    /// Sum total of the byte size of all guaranteed responses in the pool.
    guaranteed_responses_size_bytes: usize,

    /// Sum total of bytes above `MAX_RESPONSE_COUNT_BYTES` per oversized guaranteed
    /// response call request. Execution allows local-subnet requests larger than
    /// `MAX_RESPONSE_COUNT_BYTES`.
    oversized_guaranteed_requests_extra_bytes: usize,

    /// Total size of all messages in the pool, in bytes.
    size_bytes: usize,
}

impl MemoryUsageStats {
    /// Returns the memory usage of the guaranteed response messages in the pool,
    /// excluding memory reservations for guaranteed responses.
    pub fn memory_usage(&self) -> usize {
        self.guaranteed_responses_size_bytes + self.oversized_guaranteed_requests_extra_bytes
    }

    /// Calculates the change in stats caused by pushing (+) or popping (-) the
    /// given message.
    fn stats_delta(msg: &RequestOrResponse) -> MemoryUsageStats {
        match msg {
            RequestOrResponse::Request(req) => Self::request_stats_delta(req),
            RequestOrResponse::Response(rep) => Self::response_stats_delta(rep),
        }
    }

    /// Calculates the change in stats caused by pushing (+) or popping (-) a
    /// request.
    fn request_stats_delta(req: &Request) -> MemoryUsageStats {
        let size_bytes = req.count_bytes();
        if req.deadline == NO_DEADLINE {
            // Adjust guaranteed response call request byte size by this request's byte size.
            MemoryUsageStats {
                oversized_guaranteed_requests_extra_bytes: size_bytes
                    .saturating_sub(MAX_RESPONSE_COUNT_BYTES),
                size_bytes,
                ..Default::default()
            }
        } else {
            // Adjust best-effort messages byte size by this request's byte size.
            MemoryUsageStats {
                best_effort_message_bytes: size_bytes,
                size_bytes,
                ..Default::default()
            }
        }
    }

    /// Calculates the change in stats caused by pushing (+) or popping (-) the
    /// given response.
    fn response_stats_delta(rep: &Response) -> MemoryUsageStats {
        let size_bytes = rep.count_bytes();
        if rep.deadline == NO_DEADLINE {
            // Adjust guaranteed responses byte size by this response's byte size.
            MemoryUsageStats {
                guaranteed_responses_size_bytes: size_bytes,
                size_bytes,
                ..Default::default()
            }
        } else {
            // Adjust best-effort messages byte size by this response's byte size.
            MemoryUsageStats {
                best_effort_message_bytes: size_bytes,
                size_bytes,
                ..Default::default()
            }
        }
    }
}

impl AddAssign<MemoryUsageStats> for MemoryUsageStats {
    fn add_assign(&mut self, rhs: MemoryUsageStats) {
        self.guaranteed_responses_size_bytes += rhs.guaranteed_responses_size_bytes;
        self.oversized_guaranteed_requests_extra_bytes +=
            rhs.oversized_guaranteed_requests_extra_bytes;
    }
}

impl SubAssign<MemoryUsageStats> for MemoryUsageStats {
    fn sub_assign(&mut self, rhs: MemoryUsageStats) {
        self.guaranteed_responses_size_bytes -= rhs.guaranteed_responses_size_bytes;
        self.oversized_guaranteed_requests_extra_bytes -=
            rhs.oversized_guaranteed_requests_extra_bytes;
    }
}

// Custom `PartialEq`, ignoring `transient_stream_responses_size_bytes`.
impl PartialEq for MemoryUsageStats {
    fn eq(&self, rhs: &Self) -> bool {
        self.guaranteed_responses_size_bytes == rhs.guaranteed_responses_size_bytes
            && self.oversized_guaranteed_requests_extra_bytes
                == rhs.oversized_guaranteed_requests_extra_bytes
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Generates a `MessageId` for a request.
    pub(crate) fn new_request_message_id(generator: u64) -> MessageId {
        MessageId::new(Kind::Request, Context::Inbound, generator)
    }

    /// Generates a `MessageId` for a response.
    pub(crate) fn new_response_message_id(generator: u64) -> MessageId {
        MessageId::new(Kind::Response, Context::Inbound, generator)
    }
}
