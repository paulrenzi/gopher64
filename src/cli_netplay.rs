use crate::device;
use crate::netplay;
use crate::ui;
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::Bytes;
use tokio_tungstenite::tungstenite::protocol::Message;

const NETPLAY_VERSION: i32 = 17;
const EMU_NAME: &str = "gopher64";

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct NetplayRoom {
    room_name: Option<String>,
    password: Option<String>,
    protected: Option<bool>,
    #[serde(rename = "MD5")]
    md5: Option<String>,
    game_name: Option<String>,
    port: Option<i32>,
    features: Option<std::collections::HashMap<String, String>>,
    buffer_target: Option<i32>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct NetplayMessage {
    #[serde(rename = "type")]
    message_type: String,
    player_name: Option<String>,
    client_sha: Option<String>,
    netplay_version: Option<i32>,
    emulator: Option<String>,
    room: Option<NetplayRoom>,
    accept: Option<i32>,
    message: Option<String>,
    auth: Option<String>,
    player_names: Option<[String; 4]>,
    #[serde(rename = "authTime")]
    auth_time: Option<String>,
    rooms: Option<Vec<NetplayRoom>>,
}

/// Run the CLI netplay join flow: connect to WebSocket server, join room, wait for game start, launch ROM.
pub async fn run(
    server_addr: &str,
    rom_path: &std::path::Path,
    fullscreen: bool,
    player_name: Option<String>,
    room_port: Option<i32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let player_name = player_name.unwrap_or_else(|| "Player".to_string());
    let room_port = room_port.unwrap_or(45001);

    // Load and hash the ROM
    let rom_contents = device::get_rom_contents(rom_path)
        .ok_or_else(|| format!("Could not read ROM file: {}", rom_path.display()))?;

    let game_hash = device::cart::rom::calculate_hash(&rom_contents);
    let game_name = ui::storage::get_game_name(&rom_contents);

    eprintln!(
        "[netplay] ROM: {} (hash: {})",
        game_name,
        &game_hash[..16]
    );

    // Build WebSocket URL
    let ws_url = if server_addr.contains("://") {
        server_addr.to_string()
    } else {
        format!("ws://{}", server_addr)
    };

    eprintln!("[netplay] Connecting to {}...", ws_url);

    // Connect WebSocket
    let (socket, _response) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio_tungstenite::connect_async(&ws_url),
    )
    .await
    .map_err(|_| "WebSocket connection timed out")?
    .map_err(|e| format!("WebSocket connection failed: {e}"))?;

    // Get peer address for later TCP/UDP netplay connection
    let peer_addr_str = match socket.get_ref() {
        tokio_tungstenite::MaybeTlsStream::Plain(stream) => {
            stream.peer_addr()?.to_string()
        }
        _ => return Err("TLS connections not supported in CLI mode".into()),
    };

    eprintln!("[netplay] Connected. Peer: {}", peer_addr_str);

    let (mut write, mut read) = socket.split();

    // Send join room request
    let now_utc = chrono::Utc::now().timestamp_millis().to_string();
    let hasher = Sha256::new()
        .chain_update(&now_utc)
        .chain_update(EMU_NAME);

    let join_room = NetplayMessage {
        message_type: "request_join_room".to_string(),
        player_name: Some(player_name.clone()),
        client_sha: Some(env!("GIT_HASH").to_string()),
        netplay_version: Some(NETPLAY_VERSION),
        emulator: Some(EMU_NAME.to_string()),
        accept: None,
        message: None,
        rooms: None,
        player_names: None,
        auth_time: Some(now_utc),
        auth: Some(format!("{:x}", hasher.finalize())),
        room: Some(NetplayRoom {
            room_name: None,
            password: Some(String::new()),
            game_name: None,
            md5: Some(game_hash),
            protected: None,
            port: Some(room_port),
            features: None,
            buffer_target: None,
        }),
    };

    write
        .send(Message::Binary(Bytes::from(
            serde_json::to_vec(&join_room).unwrap(),
        )))
        .await?;

    eprintln!("[netplay] Sent join request for room port {}...", room_port);

    // Wait for reply_join_room
    let join_reply = wait_for_message(&mut read, "reply_join_room", 10).await?;

    if join_reply.accept.unwrap_or(-1) != 0 {
        let err_msg = join_reply
            .message
            .unwrap_or_else(|| "Join rejected (unknown reason)".to_string());
        return Err(format!("Join rejected: {}", err_msg).into());
    }

    let session = join_reply
        .room
        .as_ref()
        .ok_or("reply_join_room missing room data")?;

    let game_port = session.port.ok_or("reply_join_room missing port")?;

    let features_default = "false".to_string();
    let cheats_default = "{}".to_string();
    let overclock: bool = session
        .features
        .as_ref()
        .and_then(|f| f.get("overclock"))
        .unwrap_or(&features_default)
        .parse()
        .unwrap_or(false);
    let disable_expansion_pak: bool = session
        .features
        .as_ref()
        .and_then(|f| f.get("disable_expansion_pak"))
        .unwrap_or(&features_default)
        .parse()
        .unwrap_or(false);
    let cheats_str = session
        .features
        .as_ref()
        .and_then(|f| f.get("cheats"))
        .unwrap_or(&cheats_default);
    let cheats: std::collections::HashMap<String, Option<String>> =
        serde_json::from_str(cheats_str).unwrap_or_default();

    eprintln!(
        "[netplay] Joined room '{}'. Game port: {}. Waiting for host to start...",
        session
            .room_name
            .as_deref()
            .unwrap_or("unknown"),
        game_port
    );

    // Wait for reply_begin_game
    let begin_reply = wait_for_message(&mut read, "reply_begin_game", 300).await?;

    if begin_reply.accept.unwrap_or(-1) != 0 {
        let err_msg = begin_reply
            .message
            .unwrap_or_else(|| "Game start rejected".to_string());
        return Err(format!("Begin game rejected: {}", err_msg).into());
    }

    // Determine player number from player_names
    let player_names = begin_reply
        .player_names
        .as_ref()
        .ok_or("reply_begin_game missing player_names")?;

    let mut player_number: Option<u8> = None;
    for (i, name) in player_names.iter().enumerate() {
        if name == &player_name {
            player_number = Some(i as u8);
            break;
        }
    }
    let player_number = player_number.ok_or("Could not determine player number from player_names")?;

    eprintln!(
        "[netplay] Game starting! Player number: {}",
        player_number + 1
    );

    // Build the socket address for the netplay TCP/UDP server
    let mut socket_addr: std::net::SocketAddr = peer_addr_str.parse()?;
    socket_addr.set_port(game_port as u16);

    // Close the WebSocket (no longer needed)
    drop(write);
    drop(read);

    // Now launch the game with netplay
    let mut device = device::Device::new();
    device.ui.config.rom_dir = rom_path.parent().unwrap().to_path_buf();

    // Initialize netplay connection (TCP + UDP to game server)
    device.netplay = Some(netplay::init(socket_addr, player_number));

    let game_settings = ui::gui::GameSettings {
        fullscreen,
        overclock,
        disable_expansion_pak,
        cheats,
    };

    device::run_game(&mut device, rom_contents, game_settings);

    if device.netplay.is_some() {
        netplay::close(&mut device);
    }

    Ok(())
}

/// Read WebSocket messages until we get one with the expected type, or timeout.
async fn wait_for_message<S>(
    read: &mut S,
    expected_type: &str,
    timeout_secs: u64,
) -> Result<NetplayMessage, Box<dyn std::error::Error>>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        match tokio::time::timeout_at(deadline, read.next()).await {
            Ok(Some(Ok(msg))) => {
                let data = msg.into_data();
                if let Ok(message) = serde_json::from_slice::<NetplayMessage>(&data) {
                    eprintln!("[netplay] Received: {}", message.message_type);
                    if message.message_type == expected_type {
                        return Ok(message);
                    }
                    // Continue waiting for the expected message type
                }
            }
            Ok(Some(Err(e))) => {
                return Err(format!("WebSocket error: {e}").into());
            }
            Ok(None) => {
                return Err("WebSocket connection closed".into());
            }
            Err(_) => {
                return Err(format!(
                    "Timed out waiting for {} ({}s)",
                    expected_type, timeout_secs
                )
                .into());
            }
        }
    }
}
