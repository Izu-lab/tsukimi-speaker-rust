use crate::proto::proto::device_service_client::DeviceServiceClient;
use crate::proto::proto::time_service_client::TimeServiceClient;
use crate::proto::proto::{StreamDeviceInfoRequest, StreamTimeRequest};
use crate::DeviceInfo;
use futures::stream::StreamExt;
use log::{debug, error, info, warn};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use tonic::transport::Channel;

async fn run_device_service_client(
    mut client: DeviceServiceClient<Channel>,
    rx: broadcast::Receiver<DeviceInfo>,
) {
    info!("Starting DeviceService client...");
    let device_info_stream = BroadcastStream::new(rx).filter_map(|result| async move {
        result.ok().map(|info| {
            debug!("Sending device info to server: {:?}", info);
            StreamDeviceInfoRequest {
                id: info.address,
                rssi: info.rssi as i32,
            }
        })
    });

    match client.stream_device_info(device_info_stream).await {
        Ok(response) => {
            info!("DeviceService connected. Waiting for responses...");
            let mut stream = response.into_inner();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(res) => debug!("DeviceService Response: {}", res.current_time),
                    Err(e) => error!("DeviceService stream error: {}", e),
                }
            }
        }
        Err(e) => {
            error!("Failed to connect to DeviceService: {}", e);
        }
    }
}

async fn run_time_service_client(
    mut client: TimeServiceClient<Channel>,
    time_sync_tx: mpsc::Sender<String>,
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
                        debug!("Received time from server: {}", res.current_time);
                        if let Err(e) = time_sync_tx.send(res.current_time).await {
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

pub async fn connect_main(
    rx: broadcast::Receiver<DeviceInfo>,
    time_sync_tx: mpsc::Sender<String>,
) -> anyhow::Result<()> {
    let server_addr = "http://[::1]:50051";
    info!("Connecting to gRPC server at {}", server_addr);

    // サーバーに接続できるまでリトライ
    let channel = loop {
        match Channel::from_static(server_addr).connect().await {
            Ok(channel) => {
                info!("Successfully connected to gRPC server.");
                break channel;
            }
            Err(e) => {
                error!("Failed to connect to server: {}. Retrying in 5 seconds...", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    };

    // DeviceServiceクライアント
    let device_client = DeviceServiceClient::new(channel.clone());

    // TimeServiceクライアント
    let time_client = TimeServiceClient::new(channel);

    info!("Spawning gRPC client tasks...");
    let device_service_handle = tokio::spawn(run_device_service_client(device_client, rx));
    let time_service_handle = tokio::spawn(run_time_service_client(time_client, time_sync_tx));

    // 両方のタスクが終了するのを待つ
    if let Err(e) = tokio::try_join!(device_service_handle, time_service_handle) {
        error!("gRPC client task failed: {}", e);
    }

    info!("gRPC client tasks finished.");
    Ok(())
}