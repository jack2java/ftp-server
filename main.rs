use std::env;
use std::path::PathBuf;
use tokio::fs::{self, File};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use std::sync::Arc;

struct ClientSession {
    root_dir: PathBuf, // Pin the server root directory
    cwd: PathBuf,      // Virtual relative path (always starts with /)
    pasv_listener: Option<TcpListener>,
    authenticated: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "0.0.0.0:2121";
    let listener = TcpListener::bind(addr).await?;
    println!("[SERVER] Rust Tokio FTP Server running on {}", addr);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        println!("[SERVER] New connection from {}", peer_addr);

        tokio::spawn(async move {
            if let Err(e) = handle_client(stream).await {
                eprintln!("[SERVER ERROR] [{}] Client disconnected with error: {}", peer_addr, e);
            } else {
                println!("[SERVER] [{}] Session closed gracefully", peer_addr);
            }
        });
    }
}

async fn handle_client(stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let local_ip = stream.local_addr()?.ip(); // Dynamically discover the hosting interface IP
    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    let base_dir = env::current_dir()?;
    
    // Initial session state
    let session = Arc::new(Mutex::new(ClientSession {
        root_dir: base_dir,
        cwd: PathBuf::from("/"), // Virtualize paths to keep File Explorer happy
        pasv_listener: None,
        authenticated: false,
    }));

    // Send 220 Welcome
    writer_half.write_all(b"220 Welcome to Rust Tokio FTP Server\r\n").await?;

    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break; // Client disconnected
        }

        let cmd_line = line.trim();
        if cmd_line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = cmd_line.splitn(2, ' ').collect();
        let command = parts[0].to_uppercase();
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");

        println!("[COMMAND] {} {}", command, arg);

        match command.as_str() {
            "USER" => {
                writer_half.write_all(b"331 Anonymous login allowed, send PASS\r\n").await?;
            }

            "PASS" => {
                let mut sess = session.lock().await;
                sess.authenticated = true;
                writer_half.write_all(b"230 User logged in, proceed\r\n").await?;
            }

            "SYST" => {
                writer_half.write_all(b"215 UNIX Type: L8\r\n").await?;
            }

            "FEAT" => {
                writer_half.write_all(b"211-Extensions supported:\r\n UTF8\r\n211 End\r\n").await?;
            }

            "OPTS" => {
                if arg.to_uppercase() == "UTF8 ON" {
                    writer_half.write_all(b"200 UTF8 mode enabled\r\n").await?;
                } else {
                    writer_half.write_all(b"200 OK\r\n").await?;
                }
            }

            "PWD" => {
                let sess = session.lock().await;
                let path_str = sess.cwd.to_string_lossy().replace('\\', "/");
                let resp = format!("257 \"{}\" is current directory\r\n", path_str);
                writer_half.write_all(resp.as_bytes()).await?;
            }

            "CWD" => {
                let mut sess = session.lock().await;
                
                let target_path = if arg.starts_with('/') {
                    PathBuf::from(arg)
                } else {
                    sess.cwd.join(arg)
                };

                let mut clean_path = PathBuf::new();
                for comp in target_path.components() {
                    match comp {
                        std::path::Component::RootDir => clean_path.push("/"),
                        std::path::Component::ParentDir => { clean_path.pop(); },
                        std::path::Component::Normal(c) => clean_path.push(c),
                        _ => {}
                    }
                }
                if clean_path.as_os_str().is_empty() {
                    clean_path.push("/");
                }

                let physical_path = sess.root_dir.join(clean_path.strip_prefix("/").unwrap_or(&clean_path));

                if physical_path.is_dir() {
                    sess.cwd = clean_path;
                    writer_half.write_all(b"250 Directory successfully changed.\r\n").await?;
                } else {
                    writer_half.write_all(b"550 Failed to change directory.\r\n").await?;
                }
            }

            "TYPE" => {
                writer_half.write_all(b"200 Type set to I\r\n").await?;
            }

            "PASV" => {
                let mut ip_str = local_ip.to_string();
                
                if ip_str == "::1" || ip_str == "::" {
                    ip_str = "127.0.0.1".to_string();
                }

                let bind_addr = format!("{}:0", ip_str);
                let pasv = TcpListener::bind(&bind_addr).await?;
                let local_addr = pasv.local_addr()?;
                let port = local_addr.port();

                let p1 = port >> 8;
                let p2 = port & 0xFF;

                let ip_parts: Vec<&str> = ip_str.split('.').collect();

                let mut sess = session.lock().await;
                sess.pasv_listener = Some(pasv);

                if ip_parts.len() == 4 {
                    let resp = format!(
                        "227 Entering Passive Mode ({},{},{},{},{},{})\r\n",
                        ip_parts[0], ip_parts[1], ip_parts[2], ip_parts[3], p1, p2
                    );
                    writer_half.write_all(resp.as_bytes()).await?;
                } else {
                    writer_half.write_all(b"425 Internal routing error\r\n").await?;
                }
            }

            "LIST" => {
                let pasv_listener = {
                    let mut sess = session.lock().await;
                    sess.pasv_listener.take()
                };

                if let Some(listener) = pasv_listener {
                    writer_half.write_all(b"150 Opening ASCII mode data connection for file list\r\n").await?;

                    if let Ok((mut data_stream, _)) = listener.accept().await {
                        let sess = session.lock().await;
                        let mut listing = String::new();

                        let physical_path = sess.root_dir.join(sess.cwd.strip_prefix("/").unwrap_or(&sess.cwd));

                        if let Ok(mut entries) = fs::read_dir(&physical_path).await {
                            while let Ok(Some(entry)) = entries.next_entry().await {
                                if let Ok(meta) = entry.metadata().await {
                                    let name = entry.file_name().to_string_lossy().to_string();
                                    if meta.is_dir() {
                                        listing.push_str(&format!("drwxr-xr-x 1 user group 0 Jan 1 00:00 {}\r\n", name));
                                    } else {
                                        listing.push_str(&format!("-rw-r--r-- 1 user group {} Jan 1 00:00 {}\r\n", meta.len(), name));
                                    }
                                }
                            }
                        }

                        data_stream.write_all(listing.as_bytes()).await?;
                        let _ = data_stream.shutdown().await;
                        writer_half.write_all(b"226 Transfer complete\r\n").await?;
                    } else {
                        writer_half.write_all(b"425 Can't open data connection\r\n").await?;
                    }
                } else {
                    writer_half.write_all(b"425 Use PASV first\r\n").await?;
                }
            }

            "NLST" => {
                let pasv_listener = {
                    let mut sess = session.lock().await;
                    sess.pasv_listener.take()
                };

                if let Some(listener) = pasv_listener {
                    writer_half.write_all(b"150 Opening data connection\r\n").await?;

                    if let Ok((mut data_stream, _)) = listener.accept().await {
                        let sess = session.lock().await;
                        let mut listing = String::new();

                        let physical_path = sess.root_dir.join(sess.cwd.strip_prefix("/").unwrap_or(&sess.cwd));

                        if let Ok(mut entries) = fs::read_dir(&physical_path).await {
                            while let Ok(Some(entry)) = entries.next_entry().await {
                                let name = entry.file_name().to_string_lossy().to_string();
                                listing.push_str(&format!("{}\r\n", name));
                            }
                        }

                        data_stream.write_all(listing.as_bytes()).await?;
                        let _ = data_stream.shutdown().await;
                        writer_half.write_all(b"226 Transfer complete\r\n").await?;
                    } else {
                        writer_half.write_all(b"425 Can't open data connection\r\n").await?;
                    }
                } else {
                    writer_half.write_all(b"425 Use PASV first\r\n").await?;
                }
            }

            "RETR" => {
                let pasv_listener = {
                    let mut sess = session.lock().await;
                    sess.pasv_listener.take()
                };
                if let Some(listener) = pasv_listener {
                    let file_path = {


let sess = session.lock().await;
sess.root_dir.join(sess.cwd.strip_prefix("/").unwrap_or(&sess.cwd)).join(arg)
};
if file_path.exists() && file_path.is_file() {
writer_half.write_all(b"150 Opening BINARY mode data connection\r\n").await?;
if let Ok((mut data_stream, _)) = listener.accept().await {
if let Ok(mut file) = File::open(file_path).await {
if let Err(e) = tokio::io::copy(&mut file, &mut data_stream).await {
eprintln!("[TRANSFER ERROR] File copy failed: {}", e);
}
let _ = data_stream.shutdown().await;
writer_half.write_all(b"226 Transfer complete\r\n").await?;
} else {
writer_half.write_all(b"550 Failed to open file\r\n").await?;
}
} else {
writer_half.write_all(b"425 Can't open data connection\r\n").await?;
}
} else {
writer_half.write_all(b"550 File not found\r\n").await?;
}
} else {
writer_half.write_all(b"425 Use PASV first\r\n").await?;
}
}
"STOR" => {
let pasv_listener = {
let mut sess = session.lock().await;
sess.pasv_listener.take()
};
if let Some(listener) = pasv_listener {
let file_path = {
let sess = session.lock().await;
sess.root_dir.join(sess.cwd.strip_prefix("/").unwrap_or(&sess.cwd)).join(arg)
};
writer_half.write_all(b"150 Opening BINARY mode data connection\r\n").await?;
if let Ok((mut data_stream, _)) = listener.accept().await {
if let Ok(mut file) = File::create(file_path).await {
if let Err(e) = tokio::io::copy(&mut data_stream, &mut file).await {
eprintln!("[TRANSFER ERROR] File write failed: {}", e);
}
let _ = data_stream.shutdown().await;
writer_half.write_all(b"226 Transfer complete\r\n").await?;
} else {
writer_half.write_all(b"550 Could not create file\r\n").await?;
}
} else {
writer_half.write_all(b"425 Can't open data connection\r\n").await?;
}
} else {
writer_half.write_all(b"425 Use PASV first\r\n").await?;
}
}
// SIMPLIFIED RESPONSE STRINGS TO AVOID PARSING ERRORS
"MKD" => {
if arg.is_empty() {
writer_half.write_all(b"501 Syntax error in parameters or arguments.\r\n").await?;
} else {
let sess = session.lock().await;
let target_dir = sess.root_dir.join(sess.cwd.strip_prefix("/").unwrap_or(&sess.cwd)).join(arg);
if fs::create_dir(&target_dir).await.is_ok() {
let resp = format!("257 {} created successfully.\r\n", arg);
writer_half.write_all(resp.as_bytes()).await?;
} else {
writer_half.write_all(b"550 Access denied or directory already exists.\r\n").await?;
}
}
}
"RMD" => {
if arg.is_empty() {
writer_half.write_all(b"501 Syntax error in parameters.\r\n").await?;
} else {
let sess = session.lock().await;
let target_dir = sess.root_dir.join(sess.cwd.strip_prefix("/").unwrap_or(&sess.cwd)).join(arg);
if fs::remove_dir(&target_dir).await.is_ok() {
writer_half.write_all(b"250 Directory removed successfully.\r\n").await?;
} else {
writer_half.write_all(b"550 Failed to remove directory. Ensure it is empty.\r\n").await?;
}
}
}
"DELE" => {
if arg.is_empty() {
writer_half.write_all(b"501 Syntax error in parameters.\r\n").await?;
} else {
let sess = session.lock().await;
let file_path = sess.root_dir.join(sess.cwd.strip_prefix("/").unwrap_or(&sess.cwd)).join(arg);
if fs::remove_file(&file_path).await.is_ok() {
writer_half.write_all(b"250 File deleted successfully.\r\n").await?;
} else {
writer_half.write_all(b"550 File not found or permission denied.\r\n").await?;
}
}
}
"QUIT" => {
writer_half.write_all(b"221 Goodbye.\r\n").await?;
break;
}
_ => {
writer_half.write_all(b"502 Command not implemented\r\n").await?;
}
}
}
Ok(())
}