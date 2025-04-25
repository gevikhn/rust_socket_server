use tokio::net::TcpStream;
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use std::error::Error;
use byteorder::{BigEndian, ByteOrder};
use serde_json::Value;
use tokio::time::timeout;
use std::time::Duration;
use std::io::{self, BufRead};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // 连接到服务器
    let addr = "127.0.0.1:8080";
    println!("正在连接到服务器 {}...", addr);
    let socket = TcpStream::connect(addr).await?;
    println!("已连接到服务器！");
    
    // 将套接字分成两个部分，一个用于读取，一个用于写入
    let (reader, writer) = socket.into_split();
    let writer = Arc::new(Mutex::new(writer));
    
    // 创建一个通道用于终止接收线程
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    
    // 启动一个异步任务来接收消息
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            tokio::select! {
                // 检查是否收到停止信号
                _ = stop_rx.recv() => {
                    println!("接收线程收到停止信号");
                    break;
                }
                // 尝试接收消息
                result = receive_message(&mut reader) => {
                    match result {
                        Ok(response) => {
                            println!("收到消息: {}", response);
                        }
                        Err(e) => {
                            // 如果是超时错误，则继续循环，不退出
                            if e.to_string().contains("读取超时") {
                                // 超时是正常的，不需要打印错误
                                continue;
                            } else {
                                println!("接收消息时出错: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
        }
    });

    // 读取用户输入并发送消息
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    println!("请输入要发送的JSON消息（例如: {{\"message\": \"你好\", \"type\": \"greeting\"}}）");
    println!("输入'exit'退出");

    loop {
        println!("\n> ");
        let line = match lines.next() {
            Some(line) => line?,
            None => break,
        };

        if line.trim() == "exit" {
            break;
        }

        // 尝试解析JSON
        let json_data = match serde_json::from_str::<Value>(&line) {
            Ok(data) => data,
            Err(e) => {
                println!("无效的JSON格式: {}", e);
                continue;
            }
        };

        // 发送消息
        let writer_clone = writer.clone();
        if let Err(e) = send_message(writer_clone, &json_data).await {
            println!("发送消息时出错: {}", e);
            break;
        }
    }

    // 发送停止信号到接收线程
    let _ = stop_tx.send(()).await;
    
    // 等待接收线程结束
    let _ = reader_task.await;
    
    println!("已断开连接");
    Ok(())
}

// 发送消息到服务器
async fn send_message(socket: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>, data: &Value) -> Result<(), Box<dyn Error + Send + Sync>> {
    // 将数据序列化为JSON字符串
    let json_data = serde_json::to_vec(data)?;
    let data_len = json_data.len() as u32;
    
    // 创建长度前缀
    let mut len_bytes = [0u8; 4];
    BigEndian::write_u32(&mut len_bytes, data_len);
    
    // 获取写入锁
    let mut writer = socket.lock().await;
    
    // 发送长度前缀
    writer.write_all(&len_bytes).await?;
    
    // 发送JSON数据
    writer.write_all(&json_data).await?;
    writer.flush().await?;
    
    println!("已发送消息: {}", data);
    Ok(())
}

// 从服务器接收消息
async fn receive_message(socket: &mut tokio::net::tcp::OwnedReadHalf) -> Result<Value, Box<dyn Error + Send + Sync>> {
    // 使用超时读取长度前缀
    let len_bytes = match timeout(Duration::from_secs(2), async {
        let mut len_bytes = [0u8; 4];
        socket.read_exact(&mut len_bytes).await?;
        Ok::<[u8; 4], std::io::Error>(len_bytes)
    }).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => return Err(Box::new(e)),
        Err(_) => return Err("读取超时".into()),
    };
    
    let data_len = BigEndian::read_u32(&len_bytes) as usize;
    
    // 确保数据长度合理
    if data_len > 1024 * 1024 {  // 限制为1MB
        return Err("数据包太大".into());
    }
    
    // 使用超时读取JSON数据
    let data_buffer = match timeout(Duration::from_secs(2), async {
        let mut data_buffer = vec![0u8; data_len];
        socket.read_exact(&mut data_buffer).await?;
        Ok::<Vec<u8>, std::io::Error>(data_buffer)
    }).await {
        Ok(Ok(buffer)) => buffer,
        Ok(Err(e)) => return Err(Box::new(e)),
        Err(_) => return Err("读取超时".into()),
    };
    
    // 解析JSON数据
    let json_data = serde_json::from_slice::<Value>(&data_buffer)?;
    
    Ok(json_data)
}
