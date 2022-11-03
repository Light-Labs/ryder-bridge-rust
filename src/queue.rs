//! A queue for incoming WebSocket connections. See [`ConnectionQueue`].

use std::collections::VecDeque;

use futures_channel::oneshot::{self, Receiver, Sender};

/// An attempt was made to serve a ticket that was already served.
#[derive(Debug)]
pub struct TicketAlreadyServedError;

/// A ticket used to signal the next connection in line to proceed.
struct ConnectionTicket {
    /// The ticket's unique ID.
    id: usize,
    /// The channel to send the signal on, or `None` if it was already sent.
    sender: Option<Sender<()>>,
}

impl ConnectionTicket {
    /// Creates a new `ConnectionTicket` and returns the ticket and its corresponding [`Receiver`],
    /// which can be `await`ed to be notified when the ticket is ready to be served.
    fn new(id: usize) -> (Self, Receiver<()>) {
        let (tx, rx) = oneshot::channel();

        let ticket = ConnectionTicket {
            id,
            sender: Some(tx),
        };

        (ticket, rx)
    }

    /// Tries to serve this ticket. Returns `Ok` if it was served successfully or `Err` if it has
    /// already been served.
    fn try_serve(&mut self) -> Result<(), TicketAlreadyServedError> {
        self.sender
            .take()
            .map(|s| s.send(()).expect("Abandoned ticket was not removed from the queue"))
            .ok_or(TicketAlreadyServedError)
    }

    /// Returns the ID of this `ConnectionTicket`.
    fn id(&self) -> usize {
        self.id
    }
}

/// A queue for incoming WebSocket connections. Connections can be added to the queue and are served
/// in the order in which they were received.
pub struct ConnectionQueue {
    /// The ID of the next ticket to be added.
    next_id: usize,
    /// The queue of tickets to be served.
    queue: VecDeque<ConnectionTicket>,
}

impl ConnectionQueue {
    /// Returns a new empty `ConnectionQueue`.
    pub fn new() -> Self {
        ConnectionQueue {
            next_id: 0,
            queue: VecDeque::new(),
        }
    }

    /// Returns whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Adds a connection to the queue and returns its ID in the queue and a [`Receiver`], which can
    /// be `await`ed to be notified of when the connection is ready to be served.
    pub fn add_connection(&mut self) -> (usize, Receiver<()>) {
        let id = self.next_id;
        self.next_id += 1;

        let (ticket, rx) = ConnectionTicket::new(id);
        self.queue.push_back(ticket);
        (id, rx)
    }

    /// Serves the next connection in the queue if one exists. Returns whether a connection was
    /// served.
    ///
    /// # Panics
    ///
    /// Panics if the next connection in the queue was already served. This is prevented by always
    /// calling [`remove`][Self::remove] after each served connection completes its work.
    pub fn serve_next(&mut self) -> bool {
        self.queue
            .front_mut()
            .map(|c| c.try_serve().unwrap())
            .is_some()
    }

    /// Removes the connection with the specified ID. Returns whether a connection was removed.
    ///
    /// This must be called whenever a served connection completes its work, and may also be called
    /// to cancel a pending connection.
    pub fn remove(&mut self, id: usize) -> bool {
        (0..self.queue.len())
            .find(|i| self.queue[*i].id() == id)
            .map(|i| self.queue.remove(i))
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        thread,
        time::Duration,
    };

    use super::*;

    #[test]
    fn test_queue_add_connection() {
        let mut queue = ConnectionQueue::new();
        assert!(queue.is_empty());

        let (id_1, _) = queue.add_connection();
        let (id_2, _) = queue.add_connection();

        assert!(!queue.is_empty());
        assert_eq!(0, id_1);
        assert_eq!(1, id_2);
    }

    #[test]
    fn test_queue_remove() {
        let mut queue = ConnectionQueue::new();

        let (id_1, _) = queue.add_connection();

        assert!(queue.remove(id_1));
        assert!(queue.is_empty());
    }

    #[test]
    fn test_queue_serve_next() {
        let mut queue = ConnectionQueue::new();

        // Add some connections
        let (id_1, rx_1) = queue.add_connection();
        let (id_2, rx_2) = queue.add_connection();

        // Wait for them to be served in separate threads
        let handle_1 = thread::spawn(move || {
            futures::executor::block_on(rx_1).unwrap();
        });
        let handle_2 = thread::spawn(move || {
            futures::executor::block_on(rx_2).unwrap();
        });

        thread::sleep(Duration::from_millis(50));

        // The connections are still waiting to be served
        assert!(!handle_1.is_finished());
        assert!(!handle_2.is_finished());
        // Serve connection 1
        assert!(queue.serve_next());

        thread::sleep(Duration::from_millis(50));

        // Connection 1 has been served, but connection 2 is still waiting
        assert!(handle_1.is_finished());
        assert!(!handle_2.is_finished());
        // Serve connection 2
        queue.remove(id_1);
        assert!(queue.serve_next());

        thread::sleep(Duration::from_millis(50));

        // All connections have been served
        assert!(handle_2.is_finished());
        queue.remove(id_2);
        assert!(queue.is_empty());
    }

    #[test]
    #[should_panic(expected = "Abandoned ticket")]
    fn test_queue_serve_next_without_removing() {
        let mut queue = ConnectionQueue::new();

        let (_, _) = queue.add_connection();

        assert!(queue.serve_next());
        // Fails because the connection was not removed
        queue.serve_next();
    }
}
