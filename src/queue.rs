//! A queue for incoming WebSocket connections. See [`ConnectionQueue`].

use std::collections::VecDeque;

use futures::channel::oneshot::{self, Receiver, Sender};

/// A type that can be `await`ed to to be notified of when a ticket in the queue is now being
/// served.
pub type TicketNotifier = Receiver<()>;

/// An error while serving a ticket.
#[derive(Debug)]
pub enum Error {
    /// An attempt was made to serve a ticket that was already served.
    TicketAlreadyServed,
    /// A ticket's notifier was dropped before the ticket was served.
    TicketAbandoned,
}

/// A ticket used to signal the next connection in line to proceed.
struct ConnectionTicket {
    /// The ticket's unique ID.
    id: usize,
    /// The channel to send the signal on, or `None` if it was already sent.
    sender: Option<Sender<()>>,
}

impl ConnectionTicket {
    /// Creates a new `ConnectionTicket` and returns the ticket and its corresponding
    /// [`TicketNotifier`], which can be `await`ed to be notified when the ticket is ready to be
    /// served.
    fn new(id: usize) -> (Self, TicketNotifier) {
        let (tx, rx) = oneshot::channel();

        let ticket = ConnectionTicket {
            id,
            sender: Some(tx),
        };

        (ticket, rx)
    }

    /// Tries to serve this ticket. Returns `Ok` if it was served successfully or `Err` if it has
    /// already been served.
    fn try_serve(&mut self) -> Result<(), Error> {
        self.sender
            .take()
            .ok_or(Error::TicketAlreadyServed)
            .and_then(|s| s.send(()).map_err(|_| Error::TicketAbandoned))
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

    /// Adds a connection to the queue and returns its ID in the queue and a [`TicketNotifier`],
    /// which can be `await`ed to be notified of when the connection is ready to be served.
    pub fn add_connection(&mut self) -> (usize, TicketNotifier) {
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
        while let Some(first) = self.queue.front_mut() {
            match first.try_serve() {
                Ok(_) => return true,
                // If the connection could not be served, remove it from the queue
                Err(Error::TicketAbandoned) => {
                    let _ = self.queue.pop_front();
                }
                Err(Error::TicketAlreadyServed) => panic!("Ticket was already served"),
            }
        }

        false
    }

    /// Removes the connection with the specified ID. Returns whether a connection was removed.
    ///
    /// This must be called whenever a served connection completes its work, and may also be called
    /// to cancel a pending connection.
    pub fn remove(&mut self, id: usize) -> bool {
        self.find(id)
            .map(|i| self.queue.remove(i))
            .is_some()
    }

    /// Removes the connection with the specified ID and, if it was at the head of the queue, calls
    /// [`serve_next`][Self::serve_next]. Returns whether `serve_next` was called.
    ///
    /// This may be called instead of [`remove`][Self::remove] to automatically advance the queue
    /// as needed.
    pub fn remove_and_serve_next(&mut self, id: usize) -> bool {
        let index = self.find(id);

        self.remove(id);

        if index == Some(0) {
            self.serve_next();
            true
        } else {
            false
        }
    }

    /// Returns the index of the connection with the specified ID if it exists.
    fn find(&self, id: usize) -> Option<usize> {
        (0..self.queue.len())
            .find(|i| self.queue[*i].id() == id)
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
    fn test_connection_try_serve() {
        let (mut ticket_1, _rx_1) = ConnectionTicket::new(0);

        assert!(ticket_1.try_serve().is_ok());
        assert!(matches!(ticket_1.try_serve(), Err(Error::TicketAlreadyServed)));

        let (mut ticket_2, rx_2) = ConnectionTicket::new(1);

        // Abandon the ticket by dropping the notification receiver
        drop(rx_2);

        assert!(matches!(ticket_2.try_serve(), Err(Error::TicketAbandoned)));
    }

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
    fn test_queue_find() {
        let mut queue = ConnectionQueue::new();

        let (id_1, _rx_1) = queue.add_connection();
        let (id_2, _rx_2) = queue.add_connection();
        let (id_3, _rx_3) = queue.add_connection();

        assert_eq!(None, queue.find(99));
        assert_eq!(Some(0), queue.find(id_1));
        assert_eq!(Some(1), queue.find(id_2));
        assert_eq!(Some(2), queue.find(id_3));
    }

    #[test]
    fn test_queue_remove() {
        let mut queue = ConnectionQueue::new();

        // Try to remove a nonexistent connection
        assert!(!queue.remove(0));

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
    #[should_panic(expected = "already served")]
    fn test_queue_serve_next_without_removing() {
        let mut queue = ConnectionQueue::new();

        let (_, _rx) = queue.add_connection();

        assert!(queue.serve_next());
        // Fails because the connection was not removed
        queue.serve_next();
    }

    #[test]
    fn test_queue_serve_next_abandoned() {
        let mut queue = ConnectionQueue::new();

        let (_, rx) = queue.add_connection();

        // The notification receiver is dropped, effectively abandoning the ticket
        drop(rx);

        // Another connection is added but not abandoned
        let (id_2, mut rx_2) = queue.add_connection();

        // The abandoned ticket is removed automatically and the next non-abandoned ticket is served
        assert!(queue.serve_next());
        assert!(rx_2.try_recv().is_ok());

        assert!(queue.remove(id_2));
        assert!(queue.is_empty());
    }

    #[test]
    fn test_queue_remove_and_serve_next() {
        let mut queue = ConnectionQueue::new();

        // Add some connections
        let (id_1, _rx_1) = queue.add_connection();
        let (id_2, _rx_2) = queue.add_connection();
        let (id_3, _rx_3) = queue.add_connection();

        // Serve connection 1
        queue.serve_next();

        // Remove connection 2 from the queue (does not serve next)
        assert!(!queue.remove_and_serve_next(id_2));
        // Remove connection 1 from the queue (serves connection 3)
        assert!(queue.remove_and_serve_next(id_1));
        assert_eq!(id_3, queue.queue[0].id());
        // Remove connection 3 from the queue (serves next but queue is empty)
        assert!(queue.remove_and_serve_next(id_3));

        // All connections have been served and removed
        assert!(queue.is_empty());
    }
}
