use axum::{
    Router,
    response::sse::{Event, Sse},
    routing::get,
};
use block2::StackBlock;
use embed_plist;
use futures::stream::Stream;
use objc2_core_motion::CMHeadphoneMotionManager;
use objc2_foundation::NSOperationQueue;
use serde_json::json;
use std::error::Error;
use std::{
    convert::Infallible,
    fmt,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver},
    },
    time::Duration,
};
use tokio::sync::broadcast;

embed_plist::embed_info_plist!("../Info.plist");

// top 10 errors
#[derive(Debug)]
struct MotionError {
    details: String,
}

// im so good at rust
impl MotionError {
    fn new(msg: &str) -> MotionError {
        MotionError {
            details: msg.to_string(),
        }
    }
}

impl fmt::Display for MotionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

impl Error for MotionError {
    fn description(&self) -> &str {
        &self.details
    }
}

fn exit_error(err: &dyn Error, extra_help: Option<&str>) -> ! {
    match extra_help {
        Some(help) => eprintln!("ERROR: {}; {}", err, help),
        None => eprintln!("ERROR: {}", err),
    }
    std::process::exit(1);
}

async fn health_check() -> &'static str {
    "Server is running"
}

async fn sse_handler(
    axum::extract::State(tx): axum::extract::State<broadcast::Sender<(f64, f64, f64)>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    println!("SSE client connected");
    let mut rx = tx.subscribe();

    let stream = async_stream::stream! {
        while let Ok(data) = rx.recv().await {
            // println!("Sending SSE data: roll={}, pitch={}, yaw={}", data.0, data.1, data.2);
            let json_data = json!({
                "roll": data.0,
                "pitch": data.1,
                "yaw": data.2
            });
            yield Ok(Event::default().data(json_data.to_string()))
        }
    };

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(1))
            .text("keep-alive-text"),
    )
}

#[tokio::main]
async fn main() {
    println!("Starting device motion updates...");
    println!("Server will be available at http://localhost:10340");
    println!("SSE endpoint: http://localhost:10340/motion_events");

    // let update_interval = Duration::from_millis(40);
    let manager = match MotionManager::new() {
        Ok(result) => result,
        Err(err) => exit_error(&err, None),
    };

    let (tx, _rx) = broadcast::channel::<(f64, f64, f64)>(16);
    // clone sender to send data to
    let tx_clone = tx.clone();

    // Clone the receiver before moving manager
    let motion_receiver = Arc::clone(&manager.receiver);
    // Keep manager alive by moving it into a variable that stays in scope
    let _manager = manager;

    tokio::spawn(async move {
        loop {
            let attitude = match tokio::task::spawn_blocking({
                let motion_receiver = Arc::clone(&motion_receiver);
                move || {
                    let receiver = motion_receiver.lock().unwrap();
                    receiver.recv_timeout(Duration::from_secs(1))
                }
            })
            .await
            {
                Ok(Ok(attitude)) => attitude,
                Ok(Err(err)) => {
                    eprintln!("Motion data timeout: {}; are your AirPods connected?", err);
                    continue;
                }
                Err(err) => exit_error(&err, Some("task join error")),
            };
            // println!(
            //     "Roll: {}, Pitch: {}, Yaw: {}",
            //     attitude.0, attitude.1, attitude.2
            // );
            tx_clone.send(attitude).unwrap();
        }
    });

    let app = Router::new()
        .route("/", get(health_check))
        .route("/motion_events", get(sse_handler))
        .with_state(tx);
    let listener = tokio::net::TcpListener::bind("localhost:10340")
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}

struct MotionManager {
    _motion_manager: objc2::rc::Retained<CMHeadphoneMotionManager>,
    // to keep handler alive, use box to store closure
    _handler: Box<dyn std::any::Any>,
    receiver: Arc<Mutex<Receiver<(f64, f64, f64)>>>,
}

impl MotionManager {
    fn new() -> Result<Self, MotionError> {
        // ):
        unsafe {
            let manager = CMHeadphoneMotionManager::new();
            let operation_queue = NSOperationQueue::new();

            if !manager.isDeviceMotionAvailable() {
                return Err(MotionError::new(
                    "Headphone motion data not available on this device.",
                ));
            }

            let (tx, rx) = mpsc::channel();

            let mut handler = StackBlock::new(
                move |motion: *mut objc2_core_motion::CMDeviceMotion,
                      _error: *mut objc2_foundation::NSError| {
                    if !motion.is_null() {
                        let motion_ref = &*motion;

                        // there are actually more supported APIs here
                        // see -- https://developer.apple.com/documentation/coremotion/cmheadphonemotionmanager
                        // but in theory, we should also have access to:

                        // attitude (quaternion)
                        // user acceleration
                        // rotation rate
                        // gravity vector

                        let attitude = motion_ref.attitude();

                        tx.send((
                            attitude.roll().to_degrees(),
                            attitude.pitch().to_degrees(),
                            attitude.yaw().to_degrees(),
                        ))
                        .unwrap();
                    }
                },
            );

            manager.startDeviceMotionUpdatesToQueue_withHandler(
                &operation_queue,
                std::ptr::addr_of_mut!(handler).cast(),
            );

            Ok(Self {
                _motion_manager: manager,
                // pass handler so it stays in scope and continues to live
                _handler: Box::new(handler),
                receiver: Arc::new(Mutex::new(rx)),
            })
        }
    }
}
