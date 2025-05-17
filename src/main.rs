use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::error::Error;
use byteorder::{BigEndian, ByteOrder};
use serde_json::{Value, json};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use futures::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use async_channel::{Sender, Receiver, bounded};
use std::net::SocketAddr;
use std::collections::HashMap;
use uuid::Uuid;
use tokio::sync::mpsc;

// 消息类型枚举
#[derive(Debug, Clone)]
enum MessageType {
    // 从TCP到WebSocket的数据消息
    TcpToWs(Value),
    // 从WebSocket到TCP的数据消息
    WsToTcp {
        target_id: String,
        data: Value,
    },
    // 控制消息
    Control(ControlMessage),
}

// 控制消息类型
#[derive(Debug, Clone)]
enum ControlMessage {
    // TCP客户端连接通知
    TcpClientConnected {
        client_id: String,
        addr: SocketAddr,
    },
    // TCP客户端断开连接通知
    TcpClientDisconnected {
        client_id: String,
    },
    // 请求TCP客户端列表
    RequestClientList,
    // TCP客户端列表响应
    ClientList(Vec<TcpClientInfo>),
}

// TCP客户端信息
#[derive(Debug, Clone)]
struct TcpClientInfo {
    client_id: String,
    addr: SocketAddr,
    connected_at: String,
}

// TCP客户端实例
struct TcpClient {
    client_id: String,
    addr: SocketAddr,
    sender: mpsc::Sender<Value>,
    connected_at: String,
}

// 创建消息队列，用于TCP Socket和WebSocket之间的通信
struct MessageQueue {
    // 从TCP到WebSocket的消息队列
    tcp_to_ws_sender: Sender<MessageType>,
    tcp_to_ws_receiver: Receiver<MessageType>,
    
    // 从WebSocket到TCP的消息队列
    ws_to_tcp_sender: Sender<MessageType>,
    ws_to_tcp_receiver: Receiver<MessageType>,
}

impl MessageQueue {
    fn new() -> Self {
        let (tcp_to_ws_sender, tcp_to_ws_receiver) = bounded(100);
        let (ws_to_tcp_sender, ws_to_tcp_receiver) = bounded(100);
        
        MessageQueue { 
            tcp_to_ws_sender, 
            tcp_to_ws_receiver,
            ws_to_tcp_sender,
            ws_to_tcp_receiver,
        }
    }
}

// TCP客户端管理器
struct TcpClientManager {
    clients: Mutex<HashMap<String, TcpClient>>,
}

impl TcpClientManager {
    fn new() -> Self {
        TcpClientManager {
            clients: Mutex::new(HashMap::new()),
        }
    }
    
    // 添加新的TCP客户端
    fn add_client(&self, addr: SocketAddr, sender: mpsc::Sender<Value>) -> String {
        let client_id = Uuid::new_v4().to_string();
        let connected_at = chrono::Local::now().to_rfc3339();
        
        let client = TcpClient {
            client_id: client_id.clone(),
            addr,
            sender,
            connected_at: connected_at.clone(),
        };
        
        let mut clients = self.clients.lock().unwrap();
        clients.insert(client_id.clone(), client);
        
        client_id
    }
    
    // 移除TCP客户端
    fn remove_client(&self, client_id: &str) -> bool {
        let mut clients = self.clients.lock().unwrap();
        clients.remove(client_id).is_some()
    }
    
    // 获取客户端列表
    fn get_client_list(&self) -> Vec<TcpClientInfo> {
        let clients = self.clients.lock().unwrap();
        clients.values().map(|client| {
            TcpClientInfo {
                client_id: client.client_id.clone(),
                addr: client.addr,
                connected_at: client.connected_at.clone(),
            }
        }).collect()
    }
    
    // 发送消息给指定的客户端
    async fn send_to_client(&self, client_id: &str, data: Value) -> Result<(), String> {
        // 先获取客户端信息，然后释放锁，再发送消息
        let sender = {
            let clients = self.clients.lock().unwrap();
            if let Some(client) = clients.get(client_id) {
                client.sender.clone()
            } else {
                return Err(format!("Client {} not found", client_id));
            }
        };
        
        // 发送消息
        match sender.send(data).await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Failed to send message to client {}: {}", client_id, e)),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // 创建消息队列
    let message_queue = Arc::new(MessageQueue::new());
    
    // 创建 TCP 客户端管理器
    let client_manager = Arc::new(TcpClientManager::new());
    
    // 设置TCP服务器监听地址和端口
    let tcp_addr = "0.0.0.0:8080".to_string();
    let tcp_listener = TcpListener::bind(&tcp_addr).await?;
    println!("TCP服务器正在监听 {}...", &tcp_addr);
    
    // 设置WebSocket服务器监听地址和端口
    let ws_addr = "127.0.0.1:8081".to_string();
    let ws_listener = TcpListener::bind(&ws_addr).await?;
    println!("WebSocket服务器正在监听 {}...", &ws_addr);
    
    // 启动从 WebSocket 到 TCP 的消息处理任务
    let ws_to_tcp_queue = message_queue.clone();
    let ws_to_tcp_manager = client_manager.clone();
    tokio::spawn(async move {
        process_ws_to_tcp_messages(ws_to_tcp_queue, ws_to_tcp_manager).await;
    });
    
    // 启动WebSocket服务器
    let ws_queue = message_queue.clone();
    tokio::spawn(async move {
        run_websocket_server(ws_listener, ws_queue).await;
    });
    
    // 接受并处理TCP客户端连接
    loop {
        let (socket, addr) = tcp_listener.accept().await?;
        println!("TCP客户端已连接: {}", addr);
        
        // 为每个TCP客户端创建一个新的任务处理连接
        let tcp_queue = message_queue.clone();
        let manager = client_manager.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_client(socket, tcp_queue, addr, manager).await {
                eprintln!("处理TCP客户端 {} 时出错: {}", addr, e);
            }
            println!("TCP客户端 {} 已断开连接", addr);
        });
    }
}

// 处理从 WebSocket 到 TCP 的消息
async fn process_ws_to_tcp_messages(queue: Arc<MessageQueue>, client_manager: Arc<TcpClientManager>) {
    println!("启动 WebSocket 到 TCP 的消息处理任务");
    
    loop {
        match queue.ws_to_tcp_receiver.recv().await {
            Ok(message) => {
                match message {
                    MessageType::WsToTcp { target_id, data } => {
                        println!("收到发送给 TCP 客户端 {} 的消息", target_id);
                        if let Err(e) = client_manager.send_to_client(&target_id, data).await {
                            eprintln!("发送消息到 TCP 客户端失败: {}", e);
                        }
                    },
                    MessageType::Control(ControlMessage::RequestClientList) => {
                        println!("收到请求客户端列表消息");
                        let client_list = client_manager.get_client_list();
                        
                        // 发送客户端列表响应
                        if let Err(e) = queue.tcp_to_ws_sender.send(MessageType::Control(
                            ControlMessage::ClientList(client_list)
                        )).await {
                            eprintln!("发送客户端列表失败: {}", e);
                        }
                    },
                    _ => {}
                }
            },
            Err(e) => {
                eprintln!("接收 WebSocket 到 TCP 消息失败: {}", e);
                break;
            }
        }
    }
}

// 处理TCP客户端连接的函数
async fn handle_tcp_client(mut socket: TcpStream, queue: Arc<MessageQueue>, addr: SocketAddr, client_manager: Arc<TcpClientManager>) -> Result<(), Box<dyn Error + Send + Sync>> {
    // 创建一个缓冲区用于读取数据
    let buffer = [0u8; 4096];
    
    // 创建一个通道，用于从WebSocket接收发送给该TCP客户端的消息
    let (tx, mut rx) = mpsc::channel::<Value>(32);
    
    // 将客户端添加到管理器
    let client_id = client_manager.add_client(addr, tx);
    println!("已添加TCP客户端: ID={}, 地址={}", client_id, addr);
    
    // 发送客户端连接通知到WebSocket
    let connect_notification = MessageType::Control(ControlMessage::TcpClientConnected {
        client_id: client_id.clone(),
        addr,
    });
    
    if let Err(e) = queue.tcp_to_ws_sender.send(connect_notification).await {
        eprintln!("发送客户端连接通知失败: {}", e);
    }
    
    // 使用tokio::select同时处理两个异步任务：
    // 1. 从客户端读取数据
    // 2. 从WebSocket接收发送给该客户端的消息
    let result = loop {
        tokio::select! {
            // 从 TCP 客户端读取数据
            read_result = read_from_tcp(&mut socket, &buffer) => {
                match read_result {
                    Ok(json_data) => {
                        println!("从TCP客户端 {} (ID={}) 收到数据: {}", addr, client_id, json_data);
                        
                        // 处理数据并发送响应
                        // let response = process_data(json_data.clone());
                        // if let Err(e) = send_response(&mut socket, &response).await {
                        //     break Err(e);
                        // }
                        
                        // 将数据发送到消息队列，以便WebSocket客户端可以接收
                        let mut enriched_data = json_data.clone();
                        if let Value::Object(ref mut map) = enriched_data {
                            map.insert("client_id".into(), Value::String(client_id.clone()));
                            map.insert("source_addr".into(), Value::String(addr.to_string()));
                            map.insert("timestamp".into(), Value::String(chrono::Local::now().to_rfc3339()));
                        }
                        
                        if let Err(e) = queue.tcp_to_ws_sender.send(MessageType::TcpToWs(enriched_data)).await {
                            eprintln!("发送到消息队列失败: {}", e);
                        }
                    },
                    Err(e) => {
                        break Err(e);
                    }
                }
            },
            
            // 从WebSocket接收发送给该客户端的消息
            ws_msg = rx.recv() => {
                match ws_msg {
                    Some(data) => {
                        println!("从WebSocket接收到发送给TCP客户端 {} (ID={}) 的消息: {}", addr, client_id, data);
                        if let Err(e) = send_response(&mut socket, &data).await {
                            break Err(e);
                        }
                    },
                    None => {
                        // 通道已关闭
                        break Ok(());
                    }
                }
            }
        }
    };
    
    // 客户端断开连接，从管理器中移除
    client_manager.remove_client(&client_id);
    
    // 发送客户端断开连接通知到WebSocket
    let disconnect_notification = MessageType::Control(ControlMessage::TcpClientDisconnected {
        client_id: client_id.clone(),
    });
    
    if let Err(e) = queue.tcp_to_ws_sender.send(disconnect_notification).await {
        eprintln!("发送客户端断开连接通知失败: {}", e);
    }
    
    result
}

// 从 TCP 客户端读取数据
async fn read_from_tcp(socket: &mut TcpStream, buffer: &[u8]) -> Result<Value, Box<dyn Error + Send + Sync>> {
    // 首先读取4字节的长度前缀
    let mut len_bytes = [0u8; 4];
    match socket.read_exact(&mut len_bytes).await {
        Ok(0) => {
            // 连接已关闭
            return Err("连接已关闭".into());
        }
        Ok(_) => {
            // 成功读取长度字段
            let data_len = (BigEndian::read_u32(&len_bytes) as usize) - 4;
            
            // 确保数据长度合理，防止恶意请求
            if data_len > buffer.len() {
                return Err("数据包太大".into());
            }
            
            println!("收到数据长度: {}", data_len);
            // 读取JSON数据
            let mut data_buffer = vec![0u8; data_len];
            socket.read_exact(&mut data_buffer).await?;
            
            // 尝试解析JSON数据
            match serde_json::from_slice::<Value>(&data_buffer) {
                Ok(json_data) => Ok(json_data),
                Err(e) => {
                    eprintln!("无效的JSON数据: {}", e);
                    Err(e.into())
                }
            }
        }
        Err(e) => {
            // 读取错误，可能是客户端断开连接
            println!("读取数据失败: {}", e);
            Err(e.into())
        }
    }
}

// 运行WebSocket服务器
async fn run_websocket_server(listener: TcpListener, queue: Arc<MessageQueue>) {
    println!("WebSocket服务器已启动");
    
    // 只接受一个WebSocket连接
    let mut ws_connection = None;
    
    loop {
        tokio::select! {
            // 接受新的WebSocket连接
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, addr)) => {
                        println!("WebSocket客户端已连接: {}", addr);
                        
                        // 如果已经有一个连接，关闭它
                        if ws_connection.is_some() {
                            println!("已存在WebSocket连接，关闭旧连接");
                            ws_connection = None;
                        }
                        
                        // 升级连接为WebSocket
                        match accept_async(stream).await {
                            Ok(ws_stream) => {
                                let (ws_sender, ws_receiver) = ws_stream.split();
                                
                                // 发送请求客户端列表消息
                                if let Err(e) = queue.ws_to_tcp_sender.send(MessageType::Control(
                                    ControlMessage::RequestClientList
                                )).await {
                                    eprintln!("发送请求客户端列表失败: {}", e);
                                }
                                
                                // 启动一个任务来处理从WebSocket接收的消息
                                let ws_queue = queue.clone();
                                tokio::spawn(async move {
                                    process_ws_messages(ws_receiver, ws_queue).await;
                                });
                                
                                ws_connection = Some(ws_sender);
                                println!("WebSocket连接已建立");
                            },
                            Err(e) => {
                                eprintln!("WebSocket握手失败: {}", e);
                            }
                        }
                    },
                    Err(e) => {
                        eprintln!("接受WebSocket连接失败: {}", e);
                    }
                }
            },
            
            // 从消息队列接收消息并发送到WebSocket
            msg_result = queue.tcp_to_ws_receiver.recv() => {
                if let Ok(msg) = msg_result {
                    if let Some(ref mut ws_sender) = ws_connection {
                        // 根据消息类型处理
                        let json_data = match &msg {
                            MessageType::TcpToWs(data) => {
                                // 数据消息
                                json!({
                                    "type": "data",
                                    "payload": data
                                })
                            },
                            MessageType::Control(control_msg) => {
                                // 控制消息
                                match control_msg {
                                    ControlMessage::TcpClientConnected { client_id, addr } => {
                                        json!({
                                            "type": "control",
                                            "action": "client_connected",
                                            "client_id": client_id,
                                            "addr": addr.to_string()
                                        })
                                    },
                                    ControlMessage::TcpClientDisconnected { client_id } => {
                                        json!({
                                            "type": "control",
                                            "action": "client_disconnected",
                                            "client_id": client_id
                                        })
                                    },
                                    ControlMessage::ClientList(clients) => {
                                        let client_list: Vec<Value> = clients.iter().map(|client| {
                                            json!({
                                                "client_id": client.client_id,
                                                "addr": client.addr.to_string(),
                                                "connected_at": client.connected_at
                                            })
                                        }).collect();
                                        
                                        json!({
                                            "type": "control",
                                            "action": "client_list",
                                            "clients": client_list
                                        })
                                    },
                                    _ => json!({"type": "control", "action": "unknown"})
                                }
                            },
                            _ => json!({"type": "unknown"})
                        };
                        
                        let json_str = serde_json::to_string(&json_data).unwrap_or_default();
                        println!("发送消息到WebSocket: {}", json_str);
                        
                        if let Err(e) = ws_sender.send(Message::Text(json_str.into())).await {
                            eprintln!("发送到WebSocket失败: {}", e);
                            ws_connection = None; // 连接可能已断开
                        }
                    } else {
                        println!("没有活跃的WebSocket连接，消息将被丢弃");
                    }
                }
            }
        }
    }
}

// 处理从WebSocket接收的消息
async fn process_ws_messages(mut ws_receiver: impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin, queue: Arc<MessageQueue>) {
    println!("开始处理WebSocket消息");
    
    while let Some(msg_result) = ws_receiver.next().await {
        match msg_result {
            Ok(msg) => {
                if let Message::Text(text) = msg {
                    println!("从WebSocket收到消息: {}", text);
                    
                    // 尝试解析JSON消息
                    match serde_json::from_str::<Value>(&text) {
                        Ok(json_data) => {
                            // 处理消息
                            if let Some(msg_type) = json_data.get("type").and_then(|v| v.as_str()) {
                                match msg_type {
                                    "data" => {
                                        // 数据消息
                                        if let (Some(target_id), Some(payload)) = (
                                            json_data.get("target_id").and_then(|v| v.as_str()),
                                            json_data.get("payload")
                                        ) {
                                            // 如果payload是字符串，尝试解析为JSON
                                            let data = if let Some(payload_str) = payload.as_str() {
                                                match serde_json::from_str::<Value>(payload_str) {
                                                    Ok(parsed) => parsed,
                                                    Err(_) => payload.clone(),
                                                }
                                            } else {
                                                payload.clone()
                                            };

                                            let ws_to_tcp_msg = MessageType::WsToTcp {
                                                target_id: target_id.to_string(),
                                                data,
                                            };
                                            
                                            if let Err(e) = queue.ws_to_tcp_sender.send(ws_to_tcp_msg).await {
                                                eprintln!("发送消息到TCP客户端失败: {}", e);
                                            }
                                        }
                                    },
                                    "control" => {
                                        // 控制消息
                                        if let Some(action) = json_data.get("action").and_then(|v| v.as_str()) {
                                            match action {
                                                "request_client_list" => {
                                                    let control_msg = MessageType::Control(ControlMessage::RequestClientList);
                                                    if let Err(e) = queue.ws_to_tcp_sender.send(control_msg).await {
                                                        eprintln!("发送请求客户端列表失败: {}", e);
                                                    }
                                                },
                                                _ => {}
                                            }
                                        }
                                    },
                                    _ => {}
                                }
                            }
                        },
                        Err(e) => {
                            eprintln!("无法解析WebSocket消息: {}", e);
                        }
                    }
                }
            },
            Err(e) => {
                eprintln!("WebSocket接收错误: {}", e);
                break;
            }
        }
    }
    
    println!("WebSocket消息处理结束");
}

// 处理接收到的数据
fn process_data(data: Value) -> Value {
    // 这里可以根据具体业务需求处理数据
    // 这里简单地返回一个包含原始数据的响应
    json!({
        "status": "success",
        "message": "数据已接收",
        "received_data": data
    })
}

// 发送响应给客户端
async fn send_response(socket: &mut TcpStream, response: &Value) -> Result<(), Box<dyn Error + Send + Sync>> {
    // 如果响应是字符串，直接发送字符串内容，不添加引号
    let json_data = if let Some(s) = response.as_str() {
        s.as_bytes().to_vec()
    } else {
        // 否则，将响应序列化为JSON字符串
        serde_json::to_vec(response)?
    };
    
    let data_len = json_data.len() as u32;
    
    // 创建长度前缀
    let mut len_bytes = [0u8; 4];
    BigEndian::write_u32(&mut len_bytes, data_len + 4);
    
    // 发送长度前缀
    socket.write_all(&len_bytes).await?;
    
    // 发送数据
    socket.write_all(&json_data).await?;
    socket.flush().await?;
    
    Ok(())
}
