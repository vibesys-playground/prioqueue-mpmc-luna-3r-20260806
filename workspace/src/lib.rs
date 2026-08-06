#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

const ABI_VERSION: u32 = 1;
const STATUS_OK: u32 = 0;
const STATUS_FULL: u32 = 1;
const STATUS_EMPTY: u32 = 2;
const STATUS_INVALID: u32 = 3;
const STATUS_INTERNAL_ERROR: u32 = 4;

struct Entry {
    priority: u64,
    sequence: u64,
    slot: usize,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl Eq for Entry {}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

struct Values {
    next_sequence: u64,
    heap: BinaryHeap<Entry>,
}

struct ProducerHeap {
    values: Mutex<Values>,
}

struct Slot {
    value: Vec<u8>,
    length: usize,
}

struct QueueState {
    capacity: usize,
    max_value_size: usize,
    reserved: AtomicUsize,
    producers: Vec<ProducerHeap>,
    slots: Vec<Mutex<Slot>>,
    free_slots: Mutex<Vec<usize>>,
}

pub struct Queue {
    producer_count: u32,
    consumer_count: u32,
    state: Arc<QueueState>,
}

impl Queue {
    fn new(
        capacity: u64,
        max_value_size: u64,
        producer_count: u32,
        consumer_count: u32,
    ) -> Result<Self, u32> {
        if capacity == 0 || max_value_size == 0 || producer_count == 0 || consumer_count == 0 {
            return Err(STATUS_INVALID);
        }
        let capacity = usize::try_from(capacity).map_err(|_| STATUS_INVALID)?;
        let max_value_size = usize::try_from(max_value_size).map_err(|_| STATUS_INVALID)?;
        let producer_count_usize = producer_count as usize;
        let mut producers = Vec::with_capacity(producer_count_usize);
        for _ in 0..producer_count {
            let mut heap = BinaryHeap::new();
            // Heap nodes are bounded storage too. Reserving up front avoids
            // reallocations as items are published into a producer heap.
            heap.try_reserve_exact(capacity)
                .map_err(|_| STATUS_INTERNAL_ERROR)?;
            producers.push(ProducerHeap {
                values: Mutex::new(Values {
                    next_sequence: 0,
                    heap,
                }),
            });
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        let mut free_slots = Vec::new();
        free_slots
            .try_reserve_exact(capacity)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        for index in 0..capacity {
            slots.push(Mutex::new(Slot {
                value: Vec::new(),
                length: 0,
            }));
            free_slots.push(index);
        }
        Ok(Self {
            producer_count,
            consumer_count,
            state: Arc::new(QueueState {
                capacity,
                max_value_size,
                reserved: AtomicUsize::new(0),
                producers,
                slots,
                free_slots: Mutex::new(free_slots),
            }),
        })
    }

    fn producer(&self, id: u32) -> Result<Producer, u32> {
        (id < self.producer_count)
            .then(|| Producer {
                state: Arc::clone(&self.state),
                producer_id: id as usize,
            })
            .ok_or(STATUS_INVALID)
    }

    fn consumer(&self, id: u32) -> Result<Consumer, u32> {
        (id < self.consumer_count)
            .then(|| Consumer {
                state: Arc::clone(&self.state),
            })
            .ok_or(STATUS_INVALID)
    }
}

pub struct Producer {
    state: Arc<QueueState>,
    producer_id: usize,
}

impl Producer {
    fn try_enqueue(&self, priority: u64, data: &[u8]) -> u32 {
        if data.len() > self.state.max_value_size {
            return STATUS_INVALID;
        }

        // Reservation is the capacity linearization point. It remains held
        // while the value is copied and until publication into this producer's
        // heap, so FULL includes in-flight MPMC enqueues.
        let mut reserved = self.state.reserved.load(AtomicOrdering::Acquire);
        loop {
            if reserved >= self.state.capacity {
                return STATUS_FULL;
            }
            match self.state.reserved.compare_exchange_weak(
                reserved,
                reserved + 1,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => reserved = observed,
            }
        }

        let producer = &self.state.producers[self.producer_id];
        let Ok(mut values) = producer.values.lock() else {
            self.state.reserved.fetch_sub(1, AtomicOrdering::Release);
            return STATUS_INTERNAL_ERROR;
        };
        let slot_index = {
            let Ok(mut free_slots) = self.state.free_slots.lock() else {
                drop(values);
                self.state.reserved.fetch_sub(1, AtomicOrdering::Release);
                return STATUS_INTERNAL_ERROR;
            };
            let Some(slot_index) = free_slots.pop() else {
                drop(free_slots);
                drop(values);
                self.state.reserved.fetch_sub(1, AtomicOrdering::Release);
                return STATUS_INTERNAL_ERROR;
            };
            slot_index
        };
        let slot = &self.state.slots[slot_index];
        let Ok(mut slot) = slot.lock() else {
            if let Ok(mut free_slots) = self.state.free_slots.lock() {
                free_slots.push(slot_index);
            }
            drop(values);
            self.state.reserved.fetch_sub(1, AtomicOrdering::Release);
            return STATUS_INTERNAL_ERROR;
        };
        if slot.value.len() < data.len() {
            let missing = data.len() - slot.value.len();
            if slot.value.try_reserve_exact(missing).is_err() {
                slot.length = 0;
                drop(slot);
                if let Ok(mut free_slots) = self.state.free_slots.lock() {
                    free_slots.push(slot_index);
                }
                drop(values);
                self.state.reserved.fetch_sub(1, AtomicOrdering::Release);
                return STATUS_INTERNAL_ERROR;
            }
        }
        if slot.value.len() < data.len() {
            slot.value.resize(data.len(), 0);
        }
        slot.value[..data.len()].copy_from_slice(data);
        slot.length = data.len();
        drop(slot);
        let sequence = values.next_sequence;
        values.next_sequence = values.next_sequence.wrapping_add(1);
        values.heap.push(Entry {
            priority,
            sequence,
            slot: slot_index,
        });
        // The heap lock release publishes the entry to consumers. The
        // reservation remains counted until a consumer removes the entry.
        STATUS_OK
    }
}

pub struct Consumer {
    state: Arc<QueueState>,
}

impl Consumer {
    fn try_dequeue(&self, output: &mut [u8]) -> Result<usize, u32> {
        loop {
            // Inspect each producer heap independently. A producer can only
            // publish while holding its own heap lock, and a later
            // revalidation handles another consumer winning the candidate.
            let mut best: Option<(usize, u64, u64)> = None;
            for (index, producer) in self.state.producers.iter().enumerate() {
                let Ok(values) = producer.values.lock() else {
                    return Err(STATUS_INTERNAL_ERROR);
                };
                if let Some(entry) = values.heap.peek() {
                    let candidate = (index, entry.priority, entry.sequence);
                    let is_better = match best.as_ref() {
                        None => true,
                        Some(current) => {
                            (entry.priority, entry.sequence) < (current.1, current.2)
                        }
                    };
                    if is_better {
                        best = Some(candidate);
                    }
                }
            }

            let Some((index, priority, sequence)) = best else {
                return Err(STATUS_EMPTY);
            };
            let producer = &self.state.producers[index];
            let Ok(mut values) = producer.values.lock() else {
                return Err(STATUS_INTERNAL_ERROR);
            };
            let Some(entry) = values.heap.peek() else {
                continue;
            };
            if entry.priority != priority || entry.sequence != sequence {
                continue;
            }
            let slot_index = entry.slot;
            let slot = &self.state.slots[slot_index];
            let Ok(slot) = slot.lock() else {
                return Err(STATUS_INTERNAL_ERROR);
            };
            if slot.length > output.len() {
                return Err(STATUS_INVALID);
            }
            output[..slot.length].copy_from_slice(&slot.value[..slot.length]);
            let length = slot.length;
            values.heap.pop();
            drop(slot);
            if let Ok(mut free_slots) = self.state.free_slots.lock() {
                free_slots.push(slot_index);
            } else {
                return Err(STATUS_INTERNAL_ERROR);
            }
            self.state.reserved.fetch_sub(1, AtomicOrdering::Release);
            return Ok(length);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_core_is_bounded_priority_queue() {
        let queue = Queue::new(3, 8, 1, 1).unwrap();
        let producer = queue.producer(0).unwrap();
        let consumer = queue.consumer(0).unwrap();
        assert_eq!(producer.try_enqueue(7, b"later"), STATUS_OK);
        assert_eq!(producer.try_enqueue(2, b"first"), STATUS_OK);
        assert_eq!(producer.try_enqueue(2, b"second"), STATUS_OK);
        assert_eq!(producer.try_enqueue(0, b"full"), STATUS_FULL);

        let mut output = [0_u8; 8];
        for expected in [b"first".as_slice(), b"second", b"later"] {
            assert_eq!(consumer.try_dequeue(&mut output), Ok(expected.len()));
            assert_eq!(&output[..expected.len()], expected);
        }
        assert_eq!(consumer.try_dequeue(&mut output), Err(STATUS_EMPTY));
    }
}
