use std::io::Read;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Notify;
use tokio::time::Duration;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::io::h264_reader::H264Reader;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use v4l::{Format, FourCC};
use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::Capture;

struct V4lReader<'a> {
    stream: MmapStream<'a>
}

impl<'a> Read for V4lReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error>{
        let (m_buf, meta) = self.stream.next().unwrap();
        buf[..meta.bytesused as usize].copy_from_slice(&m_buf[..meta.bytesused as usize]);
        Ok(meta.bytesused as usize)
    }
}

#[tokio::main]
async fn main() -> Result<()> {

    // Everything below is the WebRTC-rs API! Thanks for using it ❤️.

    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();

    m.register_default_codecs()?;

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // Prepare the configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    let notify_tx = Arc::new(Notify::new());

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
    let video_done_tx = done_tx.clone();

    // Create a video track
    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    // Add this newly created track to the PeerConnection
    let rtp_sender = peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // Read incoming RTCP packets
    // Before these packets are returned they are processed by interceptors. For things
    // like NACK this needs to be called.
    tokio::spawn(async move {
        let mut rtcp_buf = vec![0u8; 1500];
        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        Result::<()>::Ok(())
    });

    tokio::spawn(async move {
        let video_dev_path = "/dev/video4";
        let video_dev = Device::with_path(video_dev_path).unwrap();
        let format = Format::new(640, 360, FourCC::new(b"H264"));
        video_dev.set_format(&format)?;

        let mut stream = MmapStream::new(&video_dev, Type::VideoCapture)?;
        stream.next()?;
        let v4l_reader = V4lReader { stream };
        let mut h264 = H264Reader::new(v4l_reader, 1_048_576);

        loop {
            let nal = match h264.next_nal() {
                    Ok(nal) => nal,
                    Err(err) => {
                        println!("All video frames parsed and sent: {err}");
                        break;
                    }
            };
            video_track
                .write_sample(&Sample {
                    data: nal.data.freeze(),
                    duration: Duration::from_secs(1),
                    ..Default::default()
                })
                .await?;
        }

        #[allow(unreachable_code)]
        let _ = video_done_tx.try_send(());

        Result::<()>::Ok(())
    });

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_ice_connection_state_change(Box::new(
        move |connection_state: RTCIceConnectionState| {
            println!("Connection State has changed {connection_state}");
            if connection_state == RTCIceConnectionState::Connected {
                notify_tx.notify_waiters();
            }
            Box::pin(async {})
        },
    ));

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        println!("Peer Connection State has changed: {s}");

        if s == RTCPeerConnectionState::Failed {
            // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
            // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
            // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
            println!("Peer Connection has gone to failed exiting");
            let _ = done_tx.try_send(());
        }

        Box::pin(async {})
    }));

    // Wait for the offer to be pasted
    let line = signal::must_read_stdin()?;
    eprintln!("readline");
    let desc_data = signal::decode(line.as_str())?;
    eprintln!("decoded data");
    let offer = serde_json::from_str::<RTCSessionDescription>(&desc_data)?;
    eprintln!("decoded offer");

    // Set the remote SessionDescription
    peer_connection.set_remote_description(offer).await?;

    // Create an answer
    let answer = peer_connection.create_answer(None).await?;

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection.set_local_description(answer).await?;

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    // Output the answer in base64 so we can paste it in browser
    if let Some(local_desc) = peer_connection.local_description().await {
        let json_str = serde_json::to_string(&local_desc)?;
        let b64 = signal::encode(&json_str);
        println!("{b64}");
    } else {
        println!("generate local_description failed!");
    }

    println!("Press ctrl-c to stop");
    tokio::select! {
        _ = done_rx.recv() => {
            println!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            println!();
        }
    };

    peer_connection.close().await?;

    Ok(())
}

