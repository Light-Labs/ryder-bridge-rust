//! Integration tests.

use futures::Future;
use mock::client::WSClient;
use mock::serial::TestPort;
use ryder_bridge::{
    RESPONSE_WAIT_IN_QUEUE,
    RESPONSE_BEING_SERVED,
    RESPONSE_DEVICE_DISCONNECTED,
    RESPONSE_DEVICE_NOT_CONNECTED,
    RESPONSE_BRIDGE_SHUTDOWN,
};
use serialport::SerialPort;
use tokio::time::error::Elapsed;
use tokio::{task, time};
use tungstenite::Message;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Launches the bridge with a mock serial port, runs `f` with its listening port, a handle to the
/// serial port, and a function to terminate the bridge, and then terminates the bridge if `f` did
/// not.
async fn run_test<F, Fut>(f: F)
where
    F: FnOnce(u16, TestPort, Box<dyn FnOnce() + Send>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    // Create a test serial port
    let test_port = TestPort::new().unwrap();
    let test_port_handle = test_port.clone();

    // Launch the bridge
    let addr = mock::get_bridge_test_addr();
    let (bridge_task_handle, bridge_handle) = ryder_bridge::launch_with_port_open_fn(
        addr,
        Path::new("./nonexistent").into(),
        move |_: &Path| test_port.try_clone(),
    );
    // Give the bridge some time to start before clients begin connecting
    time::sleep(Duration::from_millis(100)).await;

    // Set up bridge termination closure
    // The synchronization types and closure are necessary to avoid lifetime and panic safety
    // issues with passing the bridge handle to `f` directly
    let bridge_handle = Arc::new(Mutex::new(Some(bridge_handle)));
    let bridge_handle_clone = bridge_handle.clone();
    let terminate = Box::new(move || {
        bridge_handle_clone.lock().unwrap().take().unwrap().terminate()
    }) as _;

    // Run the test
    let result = task::spawn(f(addr.port(), test_port_handle, terminate)).await;

    // Close the bridge if the task did not already close it
    if let Some(h) = bridge_handle.lock().unwrap().take() {
        h.terminate();
    }

    // Wait for the bridge to close
    bridge_task_handle.await.unwrap();

    // Verify that the test succeeded
    assert!(result.is_ok());
}

/// Waits for the next response from the bridge with a timeout.
async fn next_response_timeout(client: &mut WSClient) -> Result<Message, Elapsed> {
    time::timeout(Duration::from_millis(500), client.next_response())
        .await
        .map(Option::unwrap)
        .map(Result::unwrap)
}

#[tokio::test]
async fn test_echo() {
    run_test(|bridge_port, _, _| async move {
        let mut client = WSClient::new(bridge_port).await.unwrap();

        // No response is available yet
        assert!(next_response_timeout(&mut client).await.is_err());

        for x in 0..3 {
            // Send some data
            client.send(Message::binary([x; 3])).await.unwrap();

            // The test serial port simply echoes any data written to it
            assert_eq!(
                next_response_timeout(&mut client).await.unwrap(),
                Message::binary([x; 3]),
            );
        }

        // No more responses are sent
        assert!(next_response_timeout(&mut client).await.is_err());
    }).await;
}

#[tokio::test]
async fn test_device_not_connected() {
    run_test(|bridge_port, test_port, _| async move {
        // Disconnect the device and give the bridge some time to notice
        test_port.set_has_error(true);
        time::sleep(Duration::from_millis(50)).await;

        let mut client = WSClient::new(bridge_port).await.unwrap();

        // The bridge notifies the client and disconnects it
        assert_eq!(
            next_response_timeout(&mut client).await.unwrap(),
            Message::text(RESPONSE_DEVICE_NOT_CONNECTED),
        );

        assert_eq!(
            next_response_timeout(&mut client).await.unwrap(),
            Message::Close(None),
        );
    }).await;
}

/// Runs a device disconnection test, using the provided function to disconnect the device.
async fn run_test_device_disconnected<F: FnOnce(&TestPort) + Send + 'static>(
    disconnect_device: F,
) {
    run_test(|bridge_port, test_port, _| async move {
        let mut client_1 = WSClient::new(bridge_port).await.unwrap();
        let mut client_2 = WSClient::new(bridge_port).await.unwrap();

        // The second client must wait in the queue
        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::text(RESPONSE_WAIT_IN_QUEUE),
        );

        // Disconnect the device and give the bridge some time to notice
        disconnect_device(&test_port);
        time::sleep(Duration::from_millis(50)).await;

        // The bridge notifies the clients and disconnects them
        assert_eq!(
            next_response_timeout(&mut client_1).await.unwrap(),
            Message::text(RESPONSE_DEVICE_DISCONNECTED),
        );

        assert_eq!(
            next_response_timeout(&mut client_1).await.unwrap(),
            Message::Close(None),
        );

        // The second client is in the queue, so it first receives `RESPONSE_DEVICE_READY` when the
        // first client disconnects
        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::text(RESPONSE_BEING_SERVED),
        );

        // A different response is sent for clients waiting in the queue when the device disconnects
        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::text(RESPONSE_DEVICE_NOT_CONNECTED),
        );

        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::Close(None),
        );
    }).await;
}

#[tokio::test]
async fn test_device_disconnected() {
    run_test_device_disconnected(|test_port| test_port.set_has_error(true)).await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn test_device_disconnected_simulator() {
    // Device disconnections are detected differently when using the simulator on Windows
    run_test_device_disconnected(|test_port| test_port.set_device_dsr(false)).await;
}

#[tokio::test]
async fn test_bridge_shutdown() {
    run_test(|bridge_port, _, terminate_bridge| async move {
        let mut client_1 = WSClient::new(bridge_port).await.unwrap();
        let mut client_2 = WSClient::new(bridge_port).await.unwrap();

        // The second client must wait in the queue
        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::text(RESPONSE_WAIT_IN_QUEUE),
        );

        // Terminate the bridge
        terminate_bridge();

        // The bridge notifies the clients and disconnects them
        assert_eq!(
            next_response_timeout(&mut client_1).await.unwrap(),
            Message::text(RESPONSE_BRIDGE_SHUTDOWN),
        );

        assert_eq!(
            next_response_timeout(&mut client_1).await.unwrap(),
            Message::Close(None),
        );

        // The second client is in the queue, but it is still notified
        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::text(RESPONSE_BRIDGE_SHUTDOWN),
        );

        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::Close(None),
        );
    }).await;
}

#[tokio::test]
async fn test_queue() {
    run_test(|bridge_port, _, _| async move {
        let mut client_1 = WSClient::new(bridge_port).await.unwrap();
        let mut client_2 = WSClient::new(bridge_port).await.unwrap();

        // The second client must wait in the queue
        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::text(RESPONSE_WAIT_IN_QUEUE),
        );

        // While waiting in the queue, the bridge ignores data received from the client
        client_2.send(Message::binary([1, 2, 3])).await.unwrap();
        assert!(next_response_timeout(&mut client_2).await.is_err());

        // The first client is able to send and receive data still
        client_1.send(Message::binary([1, 2, 3])).await.unwrap();
        assert_eq!(
            next_response_timeout(&mut client_1).await.unwrap(),
            Message::binary([1, 2, 3]),
        );

        // Disconnect the first client
        client_1.close().await.unwrap();

        // The bridge notifies the second client that it is now being served
        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::text(RESPONSE_BEING_SERVED),
        );

        // The second client can now send and receive data
        client_2.send(Message::binary([1, 2, 3])).await.unwrap();
        assert_eq!(
            next_response_timeout(&mut client_2).await.unwrap(),
            Message::binary([1, 2, 3]),
        );
    }).await;
}
