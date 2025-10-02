use crate::proto::proto::device_service_client::DeviceServiceClient;
use crate::proto::proto::stream_device_info_response::Event;
use crate::proto::proto::time_service_client::TimeServiceClient;
use crate::proto::proto::{LocationRssi, StreamDeviceInfoRequest, StreamTimeRequest};
use crate::DeviceInfo;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, info, instrument};

#[instrument(skip(client, rx, sound_map))]
async fn run_device_service_client(
    mut client: DeviceServiceClient<Channel>,
    rx: broadcast::Receiver<Arc<DeviceInfo>>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
    my_address: Arc<Mutex<Option<String>>>,
    current_points: Arc<Mutex<i32>>,
) {
    info!("Starting DeviceService client...");
    let sound_map_for_filter = Arc::clone(&sound_map);
    let device_info_stream = BroadcastStream::new(rx)
        .filter_map(move |result| {
            let sound_map = Arc::clone(&sound_map_for_filter);
            result.ok().and_then(|info| {
                let sound_map = sound_map.lock().unwrap();
                if sound_map.contains_key(&info.address) {
                    Some(info)
                } else {
                    None
                }
            })
        })
        .chunks_timeout(10, Duration::from_millis(100))
        .map(|infos| {
            let locations = infos
                .into_iter()
                .map(|info| LocationRssi {
                    address: info.address.clone(),
                    rssi: info.rssi as i32,
                })
                .collect();
            debug!(?locations, "Sending device info to server");
            StreamDeviceInfoRequest { locations }
        });

    match client.stream_device_info(device_info_stream).await {
        Ok(response) => {
            info!("DeviceService connected. Waiting for responses...");
            let mut stream = response.into_inner();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(res) => {
                        if let Some(event) = res.event {
                            match event {
                                Event::TimeUpdate(time_update) => {
                                    debug!(?time_update, "TimeUpdate received");
                                    // TODO: Handle TimeUpdate
                                }
                                Event::LocationUpdate(location_update) => {
                                    debug!(?location_update, "LocationUpdate received");
                                    let mut sound_map = sound_map.lock().unwrap();
                                    sound_map.clear();
                                    for loc in location_update.locations {
                                        // TODO: mp3を修正する
                                        // place_type に基づいてサウンドファイル名を決定
                                        let sound_file = match loc.place_type.as_str() {
                                            "projection_mapping" => "tsukimi-main.mp3",
                                            "buddhas_bowl" => "",
                                            "jeweled_branch" => "",
                                            "fire_rat_robe" => "",
                                            "dragons_jewel" => "",
                                            "swallows_cowry" => "",
                                            _ => "sound.mp3", // 未知の place_type のためのデフォルトサウンド
                                        };
                                        sound_map.insert(loc.address, sound_file.to_string());
                                    }
                                    info!(new_sound_map_size = sound_map.len(), "Updated sound_map");
                                }
                                Event::PointUpdate(point_update) => {
                                    debug!(?point_update, "PointUpdate received");
                                    let my_address_guard = my_address.lock().unwrap();
                                    if let Some(my_addr) = my_address_guard.as_ref() {
                                        if *my_addr == point_update.user_id {
                                            let mut points = current_points.lock().unwrap();
                                            *points = point_update.points;
                                            info!(user_id = %point_update.user_id, points = *points, "Updated my points");
                                        } else {
                                            debug!(
                                                my_addr,
                                                received_user_id = %point_update.user_id,
                                                "Received points for another user, ignoring."
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => error!("DeviceService stream error: {}", e),
                }
            }
        }
        Err(e) => {
            error!("Failed to connect to DeviceService: {}", e);
        }
    }
}

#[instrument(skip(client, time_sync_tx))]
async fn run_time_service_client(
    mut client: TimeServiceClient<Channel>,
    time_sync_tx: mpsc::Sender<u64>,
) {
    info!("Starting TimeService client...");
    let request = tonic::Request::new(StreamTimeRequest {});
    match client.stream_time(request).await {
        Ok(response) => {
            info!("TimeService connected. Waiting for responses...");
            let mut stream = response.into_inner();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(res) => {
                        debug!(?res, "Received time from server");
                        if let Err(e) = time_sync_tx.send(res.elapsed_nanoseconds as u64).await {
                            error!("Failed to send time sync data: {}", e);
                        }
                    }
                    Err(e) => error!("TimeService stream error: {}", e),
                }
            }
        }
        Err(e) => {
            error!("Failed to connect to TimeService: {}", e);
        }
    }
}

#[instrument(skip(rx, time_sync_tx, sound_map))]
pub async fn connect_main(
    rx: broadcast::Receiver<Arc<DeviceInfo>>,
    time_sync_tx: mpsc::Sender<u64>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
    my_address: Arc<Mutex<Option<String>>>,
    current_points: Arc<Mutex<i32>>,
) -> anyhow::Result<()> {
    let server_addr = "https://tsukimi-background-823736753003.asia-northeast1.run.app";
    info!("Connecting to gRPC server at {}", server_addr);

    // サーバーに接続できるまでリトライ
    let channel = loop {
        match Endpoint::from_static(server_addr).connect().await {
            Ok(channel) => {
                info!("Successfully connected to gRPC server.");
                break channel;
            }
            Err(e) => {
                error!(
                    "Failed to connect to server: {}. Retrying in 5 seconds...",
                    e
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    };

    // DeviceServiceクライアント
    let device_client = DeviceServiceClient::new(channel.clone());

    // TimeServiceクライアント
    let time_client = TimeServiceClient::new(channel);

    info!("Spawning gRPC client tasks...");
    let device_service_handle = {
        let sound_map_clone = Arc::clone(&sound_map);
        let my_address_clone = Arc::clone(&my_address);
        let current_points_clone = Arc::clone(&current_points);
        tokio::spawn(run_device_service_client(
            device_client,
            rx,
            sound_map_clone,
            my_address_clone,
            current_points_clone,
        ))
    };
    let time_service_handle = tokio::spawn(run_time_service_client(time_client, time_sync_tx));

    // 両方のタスクが終了するのを待つ
    if let Err(e) = tokio::try_join!(device_service_handle, time_service_handle) {
        error!("gRPC client task failed: {}", e);
    }

    info!("gRPC client tasks finished.");
    Ok(())
}