use std::{
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

use serde_json;
use thiserror::Error;
#[cfg(feature = "tcp_client")]
use titan_types::{Event, TcpSubscriptionRequest};
use tracing::{error, info, warn};

#[derive(Debug, Error)]
pub enum TcpClientError {
    #[error("io error: {0}")]
    IOError(#[from] std::io::Error),
    #[error("serde error: {0}")]
    SerdeError(#[from] serde_json::Error),
}

/// Synchronous TCP subscription listener.
///
/// Connects to the TCP server at `addr` and sends the given subscription request
/// (encoded as JSON). It then spawns a dedicated thread that reads lines from the TCP
/// connection using non-blocking mode. If no data is available, it sleeps briefly and
/// then checks the shutdown flag again.
///
/// The listener will continue until either the TCP connection is closed or the provided
/// `shutdown_flag` is set to `true`.
///
/// # Arguments
///
/// * `addr` - The address of the TCP subscription server (e.g., "127.0.0.1:9000").
/// * `subscription_request` - The subscription request to send to the server.
/// * `shutdown_flag` - An `Arc<AtomicBool>` which, when set to `true`, signals the listener to shut down.
///
/// # Returns
///
/// A `Result` containing a `std::sync::mpsc::Receiver<Event>` that will receive events from the server,
/// or an error.
#[cfg(feature = "tcp_client_blocking")]
pub fn subscribe(
    addr: &str,
    subscription_request: TcpSubscriptionRequest,
    shutdown_flag: Arc<AtomicBool>,
) -> Result<mpsc::Receiver<Event>, TcpClientError> {
    // Connect to the TCP server.
    let mut stream = TcpStream::connect(addr)?;
    // Set the stream to non-blocking mode.
    stream.set_nonblocking(true)?;

    // Clone the stream for reading.
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);

    // Serialize the subscription request to JSON and send it.
    let req_json = serde_json::to_string(&subscription_request)?;
    stream.write_all(req_json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    // Create a standard mpsc channel to forward events.
    let (tx, rx) = mpsc::channel::<Event>();

    // Spawn a thread to read events from the TCP connection.
    thread::spawn(move || {
        let mut line = String::new();
        loop {
            // Check if shutdown has been signaled.
            if shutdown_flag.load(Ordering::SeqCst) {
                info!("Shutdown flag set. Exiting subscription thread.");
                break;
            }

            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    // Connection closed.
                    warn!("TCP connection closed by server.");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Deserialize the JSON line into an Event.
                    match serde_json::from_str::<Event>(trimmed) {
                        Ok(event) => {
                            if tx.send(event).is_err() {
                                error!("Receiver dropped. Exiting subscription thread.");
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Failed to parse event: {}. Line: {}", e, trimmed);
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data available right now.
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => {
                    error!("Error reading from TCP socket: {}", e);
                    break;
                }
            }
        }
        info!("Exiting TCP subscription thread.");
    });

    Ok(rx)
}
