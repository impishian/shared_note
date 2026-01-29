use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use warp::Filter;
use warp::ws::{WebSocket, Ws, Message};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

type SharedContent = Arc<RwLock<String>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NoteUpdate {
    content: String,
}

#[tokio::main]
async fn main() {
    let shared_content: SharedContent = Arc::new(RwLock::new("Start editing your shared note...".to_string()));
    
    // Create broadcast channel for real-time messages
    let (tx, _) = broadcast::channel(100);
    
    // Static file service - serve HTML page
    let index = warp::path::end()
        .map(|| {
            warp::reply::html(INDEX_HTML)
        });
    
    // Get current content
    let get_content = {
        let content = shared_content.clone();
        warp::path("content")
            .map(move || {
                let content = content.blocking_read();
                warp::reply::json(&NoteUpdate { content: content.clone() })
            })
    };
    
    // Save content
    let save_content = {
        let content = shared_content.clone();
        let tx = tx.clone();
        warp::path("save")
            .and(warp::post())
            .and(warp::body::json())
            .map(move |update: NoteUpdate| {
                // Update content
                {
                    let mut content_guard = content.blocking_write();
                    *content_guard = update.content.clone();
                }
                
                // Broadcast update
                let _ = tx.send(update);
                warp::reply::json(&"OK")
            })
    };
    
    // WebSocket connection
    let websocket = {
        let content = shared_content.clone();
        let tx = tx.clone();
        warp::path("ws")
            .and(warp::ws())
            .map(move |ws: Ws| {
                let content = content.clone();
                let tx = tx.clone();
                ws.on_upgrade(move |websocket| handle_websocket(websocket, content, tx))
            })
    };
    
    // Combine all routes
    let routes = index
        .or(get_content)
        .or(save_content)
        .or(websocket)
        .with(warp::cors().allow_any_origin());
    
    println!("Shared note server running on http://localhost:3000");
    warp::serve(routes)
        .run(([0, 0, 0, 0], 3000))
        .await;
}

async fn handle_websocket(
    websocket: WebSocket,
    content: SharedContent,
    broadcast_tx: broadcast::Sender<NoteUpdate>,
) {
    // Don't split the WebSocket, use it directly
    let mut websocket = websocket;
    
    println!("New WebSocket connection");
    
    // Send current content to new client
    let current_content = {
        let content_guard = content.read().await;
        content_guard.clone()
    };
    
    // Send initial content
    let initial_msg = Message::text(
        serde_json::to_string(&NoteUpdate { content: current_content }).unwrap()
    );
    
    if websocket.send(initial_msg).await.is_err() {
        return;
    }
    
    // Create a broadcast receiver for this connection
    let mut broadcast_rx = broadcast_tx.subscribe();
    
    // Use a loop to handle both receiving and sending
    loop {
        tokio::select! {
            // Handle messages from client
            msg = websocket.next() => {
                match msg {
                    Some(Ok(msg)) => {
                        if let Ok(text) = msg.to_str() {
                            if let Ok(update) = serde_json::from_str::<NoteUpdate>(text) {
                                // Update shared content
                                {
                                    let mut content_guard = content.write().await;
                                    *content_guard = update.content.clone();
                                }
                                
                                // Broadcast to all connected clients
                                let _ = broadcast_tx.send(update);
                            }
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            // Handle messages from broadcast channel
            result = broadcast_rx.recv() => {
                match result {
                    Ok(update) => {
                        let msg = Message::text(
                            serde_json::to_string(&update).unwrap()
                        );
                        if websocket.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    
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
                    
                    // Reconnect after 5 seconds
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
                    this.ws.send(JSON.stringify({ content }));
                }
            }
            
            updateStatus(message, className) {
                const statusElement = document.getElementById('status');
                statusElement.textContent = message;
                statusElement.className = className;
            }
        }
        
        // Initialize application
        document.addEventListener('DOMContentLoaded', () => {
            new SharedNote();
        });
    </script>
</body>
</html>
"#;
