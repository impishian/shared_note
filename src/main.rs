use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tokio::time::interval;
use warp::Filter;
use warp::ws::{WebSocket, Ws, Message};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

type SharedContent = Arc<RwLock<String>>;
type Connections = Arc<AtomicUsize>;

const MAX_CONNECTIONS: usize = 100;
const MAX_MESSAGE_SIZE: u64 = 500 * 1024; // 500 KB
const DATA_FILE: &str = "note_data.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NoteUpdate {
    content: String,
}

fn sanitize_html(content: &str) -> String {
    ammonia::clean(content)
}

async fn load_content(path: &PathBuf) -> String {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => {
            if let Ok(note) = serde_json::from_str::<NoteUpdate>(&s) {
                note.content
            } else {
                initial_content()
            }
        }
        Err(_) => initial_content(),
    }
}

fn initial_content() -> String {
    "Start editing your shared note...".to_string()
}

async fn save_content(path: &PathBuf, content: &str) -> Result<(), std::io::Error> {
    let note = NoteUpdate { content: content.to_string() };
    let json = serde_json::to_string(&note).map_err(|e| std::io::Error::other(e))?;
    tokio::fs::write(path, json).await
}

#[tokio::main]
async fn main() {
    let data_path = PathBuf::from(DATA_FILE);

    let initial = load_content(&data_path).await;
    let shared_content: SharedContent = Arc::new(RwLock::new(initial));
    let connections: Connections = Arc::new(AtomicUsize::new(0));
    let (tx, _) = broadcast::channel(100);

    let index = warp::path::end()
        .and(warp::get())
        .map(|| warp::reply::html(INDEX_HTML));

    let get_content = {
        let content = shared_content.clone();
        warp::path("content")
            .and(warp::get())
            .and_then(move || {
                let content = content.clone();
                async move {
                    let content_guard = content.read().await;
                    Ok::<warp::reply::Json, warp::Rejection>(
                        warp::reply::json(&NoteUpdate { content: content_guard.clone() })
                    )
                }
            })
    };

    let save_content_route = {
        let content = shared_content.clone();
        let tx = tx.clone();
        let data_path = data_path.clone();
        warp::path("save")
            .and(warp::post())
            .and(warp::body::content_length_limit(MAX_MESSAGE_SIZE))
            .and(warp::body::json())
            .and_then(move |mut update: NoteUpdate| {
                let content = content.clone();
                let tx = tx.clone();
                let data_path = data_path.clone();
                async move {
                    update.content = sanitize_html(&update.content);

                    {
                        let mut content_guard = content.write().await;
                        *content_guard = update.content.clone();
                    }

                    let _ = save_content(&data_path, &update.content).await;
                    let _ = tx.send(update);

                    Ok::<warp::reply::Json, warp::Rejection>(warp::reply::json(&"OK"))
                }
            })
    };

    let websocket = {
        let content = shared_content.clone();
        let tx = tx.clone();
        let connections = connections.clone();
        warp::path("ws")
            .and(warp::ws())
            .and_then(move |ws: Ws| {
                let content = content.clone();
                let tx = tx.clone();
                let connections = connections.clone();
                async move {
                    let current = connections.fetch_add(1, Ordering::SeqCst);
                    if current >= MAX_CONNECTIONS {
                        connections.fetch_sub(1, Ordering::SeqCst);
                        return Err(warp::reject::not_found());
                    }
                    Ok(ws.on_upgrade(move |websocket| {
                        handle_websocket(websocket, content, tx, connections)
                    }))
                }
            })
    };

    let routes = index
        .or(get_content)
        .or(save_content_route)
        .or(websocket)
        .with(warp::cors().allow_any_origin());

    println!("Shared note server running on http://localhost:3000");
    println!("Max connections: {}", MAX_CONNECTIONS);
    println!("Max message size: {} bytes", MAX_MESSAGE_SIZE);

    let sync_content = shared_content.clone();
    let sync_data_path = data_path.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(10));
        let mut last_saved: Option<String> = None;

        loop {
            ticker.tick().await;
            let current = {
                let content_guard = sync_content.read().await;
                content_guard.clone()
            };

            let needs_save = match &last_saved {
                Some(prev) => prev != &current,
                None => true,
            };

            if needs_save {
                if let Err(e) = save_content(&sync_data_path, &current).await {
                    eprintln!("Failed to sync content to file: {}", e);
                } else {
                    last_saved = Some(current);
                    println!("Content synced to file");
                }
            }
        }
    });

    warp::serve(routes)
        .run(([0, 0, 0, 0], 3000))
        .await;
}

async fn handle_websocket(
    websocket: WebSocket,
    content: SharedContent,
    broadcast_tx: broadcast::Sender<NoteUpdate>,
    connections: Connections,
) {
    println!("New WebSocket connection");

    let current_content = {
        let content_guard = content.read().await;
        content_guard.clone()
    };

    let initial_msg = match serde_json::to_string(&NoteUpdate { content: current_content }) {
        Ok(m) => Message::text(m),
        Err(_) => {
            connections.fetch_sub(1, Ordering::SeqCst);
            return;
        }
    };

    let mut websocket = websocket;
    if websocket.send(initial_msg).await.is_err() {
        connections.fetch_sub(1, Ordering::SeqCst);
        return;
    }

    let mut broadcast_rx = broadcast_tx.subscribe();

    loop {
        tokio::select! {
            msg = websocket.next() => {
                match msg {
                    Some(Ok(msg)) => {
                        if msg.as_bytes().len() > MAX_MESSAGE_SIZE as usize {
                            continue;
                        }
                        if let Ok(text) = msg.to_str() {
                            if let Ok(mut update) = serde_json::from_str::<NoteUpdate>(text) {
                                update.content = sanitize_html(&update.content);
                                {
                                    let mut content_guard = content.write().await;
                                    *content_guard = update.content.clone();
                                }
                                let _ = broadcast_tx.send(update);
                            }
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            result = broadcast_rx.recv() => {
                match result {
                    Ok(update) => {
                        let msg = match serde_json::to_string(&update) {
                            Ok(m) => Message::text(m),
                            Err(_) => break,
                        };
                        if websocket.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }

    connections.fetch_sub(1, Ordering::SeqCst);
    println!("WebSocket connection closed");
}

const INDEX_HTML: &str = r#"
<!DOCTYPE html>
<html>
<head>
    <title>Shared Note</title>
    <meta charset="UTF-8">
    <style>
        body {
            margin: 0;
            padding: 0;
            font-family: Arial, sans-serif;
        }
        h1 {
            margin: 10px;
            color: #333;
        }
        #note {
            height: 90vh;
            border: 1px solid #ccc;
            padding: 10px;
            margin: 10px;
            font-size: 16px;
            line-height: 1.5;
            overflow-y: auto;
            outline: none;
        }
        #status {
            margin: 10px;
            color: #666;
            font-size: 14px;
        }
        .connected { color: #4CAF50; }
        .disconnected { color: #F44336; }
        .connecting { color: #FF9800; }
    </style>
</head>
<body>
    <h1>Shared Note</h1>
    <div id="status" class="connecting">Connecting...</div>
    <div id="note" contenteditable="true"></div>

    <script>
        const noteElement = document.getElementById('note');
        const MAX_MESSAGE_SIZE = 500 * 1024;

        noteElement.addEventListener('beforeinput', (e) => {
            if (e.inputType === 'historyUndo' || e.inputType === 'historyRedo') {
                e.preventDefault();
            }
        });

        noteElement.addEventListener('keydown', (e) => {
            const key = e.key.toLowerCase();
            const isZ = key === 'z';
            const isY = key === 'y';
            const modifier = e.ctrlKey || e.metaKey;

            if (modifier && (isZ || isY)) {
                e.preventDefault();
            }
        });

        class SharedNote {
            constructor() {
                this.ws = null;
                this.reconnectTimeout = null;
                this.debounceTimeout = null;
                this.isConnected = false;

                this.connect();
                this.setupEventListeners();
            }

            connect() {
                const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
                const wsUrl = `${protocol}//${window.location.host}/ws`;

                this.updateStatus('Connecting...', 'connecting');
                this.ws = new WebSocket(wsUrl);

                this.ws.onopen = () => {
                    console.log('WebSocket connection established');
                    this.isConnected = true;
                    this.updateStatus('Connected', 'connected');
                };

                this.ws.onmessage = (event) => {
                    try {
                        const data = JSON.parse(event.data);
                        const noteElement = document.getElementById('note');
                        if (data.content !== noteElement.innerHTML) {
                            noteElement.innerHTML = data.content;
                        }
                    } catch (e) {
                        console.error('Failed to parse message:', e);
                    }
                };

                this.ws.onclose = () => {
                    console.log('WebSocket connection closed');
                    this.isConnected = false;
                    this.updateStatus('Connection lost, reconnecting...', 'disconnected');

                    clearTimeout(this.reconnectTimeout);
                    this.reconnectTimeout = setTimeout(() => this.connect(), 5000);
                };

                this.ws.onerror = (error) => {
                    console.error('WebSocket error:', error);
                    this.updateStatus('Connection error', 'disconnected');
                };
            }

            setupEventListeners() {
                const noteElement = document.getElementById('note');

                noteElement.addEventListener('input', () => {
                    clearTimeout(this.debounceTimeout);
                    this.debounceTimeout = setTimeout(() => {
                        this.saveContent();
                    }, 300);
                });
            }

            saveContent() {
                if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                    const content = document.getElementById('note').innerHTML;
                    const payload = JSON.stringify({ content });
                    if (payload.length <= MAX_MESSAGE_SIZE) {
                        this.ws.send(payload);
                    }
                }
            }

            updateStatus(message, className) {
                const statusElement = document.getElementById('status');
                statusElement.textContent = message;
                statusElement.className = className;
            }
        }

        document.addEventListener('DOMContentLoaded', () => {
            new SharedNote();
        });
    </script>
</body>
</html>
"#;
