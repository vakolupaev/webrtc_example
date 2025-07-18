use anyhow::Error;
use futures::{SinkExt, StreamExt};
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use gst::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use warp::Filter;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
enum SignalMessage {
    Sdp { sdp: String, kind: String },
    Ice { candidate: String, sdpMLineIndex: u32 },
}

type PeerMap = Arc<Mutex<HashMap<String, gst::Element>>>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    gst::init()?;

    let peers: PeerMap = Arc::new(Mutex::new(HashMap::new()));

    let peer_map = peers.clone();
    let routes = warp::path("ws")
        .and(warp::ws())
        .map(move |ws: warp::ws::Ws| {
            let peer_map = peer_map.clone();
            ws.on_upgrade(move |socket| handle_connection(socket, peer_map))
        });

    let static_files = warp::fs::dir("static");

    println!("Server running on http://localhost:3030");
    warp::serve(routes.or(static_files)).run(([0,0,0,0], 3030)).await;

    Ok(())
}

async fn handle_connection(ws: warp::ws::WebSocket, _peers: PeerMap) {
    let (mut tx_ws, mut rx_ws) = ws.split();

    // Канал для отправки из GStreamer обратно в браузер
    let (to_browser_tx, mut to_browser_rx) = mpsc::unbounded_channel::<String>();

    // Запускаем отправку сообщений в браузер
    tokio::spawn(async move {
        while let Some(msg) = to_browser_rx.recv().await {
            if tx_ws.send(warp::ws::Message::text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Создаём WebRTC pipeline
    let pipeline = create_webrtc_pipeline(to_browser_tx.clone()).expect("Failed to create pipeline");

    let webrtcbin = pipeline
        .clone()
        .dynamic_cast::<gst::Bin>()
        .unwrap()
        .by_name("webrtcbin")
        .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    while let Some(Ok(msg)) = rx_ws.next().await {
        if msg.is_text() {
            if let Ok(signal) = serde_json::from_str::<SignalMessage>(msg.to_str().unwrap()) {
                match signal {
                    SignalMessage::Sdp { sdp, kind } => {
                        let sdp = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()).unwrap();
                        let sdp = gst_webrtc::WebRTCSessionDescription::new(
                            if kind == "offer" {
                                gst_webrtc::WebRTCSDPType::Offer
                            } else {
                                gst_webrtc::WebRTCSDPType::Answer
                            },
                            sdp,
                        );
                        webrtcbin
                            .emit_by_name::<()>("set-remote-description", &[&sdp, &None::<gst::Promise>]);
                        let to_browser_tx2 = to_browser_tx.clone();
                        if kind == "offer" {
                            let promise = gst::Promise::with_change_func(move |reply| {
                                if let Ok(Some(reply)) = reply {
                                    let answer = reply
                                        .value("answer")
                                        .unwrap()
                                        .get::<gst_webrtc::WebRTCSessionDescription>()
                                        .unwrap();
                                    let sdp_str = answer.sdp().as_text().unwrap();
                                    let msg = serde_json::to_string(&SignalMessage::Sdp {
                                        sdp: sdp_str,
                                        kind: "answer".into(),
                                    })
                                    .unwrap();
                                    let _ = to_browser_tx2.send(msg);
                                }
                            });
                            webrtcbin.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
                        }
                    }
                    SignalMessage::Ice {
                        candidate,
                        sdpMLineIndex,
                    } => {
                        webrtcbin
                            .emit_by_name::<()>("add-ice-candidate", &[&sdpMLineIndex, &candidate]);
                    }
                }
            }
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
}

fn create_webrtc_pipeline(
    to_browser_tx: mpsc::UnboundedSender<String>,
) -> Result<gst::Pipeline, Error> {
    let pipeline = gst::Pipeline::new();

    // let src = gst::ElementFactory::make_with_name("d3d12screencapturesrc", None).unwrap();
    let src = gst::ElementFactory::make_with_name("videotestsrc", None).unwrap();
    src.set_property_from_str("is-live", "true");

    let capsfilter = gst::ElementFactory::make("capsfilter").build().unwrap();
    let caps = gst::Caps::builder("video/x-raw")
        .field("width", 1920)
        .field("height", 1080)
        .field("framerate", gst::Fraction::new(60, 1))
        .build();
    capsfilter.set_property("caps", &caps);

    let overlay = gstreamer::ElementFactory::make("clockoverlay").build().unwrap();
    let conv = gst::ElementFactory::make_with_name("videoconvert", None).unwrap();
    let enc = gst::ElementFactory::make_with_name("x264enc", None).unwrap();
    enc.set_property_from_str("tune", "zerolatency");
    enc.set_property_from_str("speed-preset", "ultrafast");
    enc.set_property("bitrate", 20000u32);

    let queue = gst::ElementFactory::make("queue").build().unwrap();
    queue.set_property("max-size-buffers", 2u32);

    let pay = gst::ElementFactory::make_with_name("rtph264pay", None).unwrap();
    pay.set_property("config-interval", 1);

    let src_pad = pay.static_pad("srcrtp").unwrap();
    let capsss = gst::Caps::builder("application/x-rtp")
    .field("media", "video")
    .field("encoding-name", "H264")
    .field("payload", 96i32)
    .build();
    src_pad.set_property("caps", &capsss);

    let webrtcbin = gst::ElementFactory::make_with_name("webrtcbin", Some("webrtcbin")).unwrap();
    webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
    webrtcbin.set_property("latency", 0u32);

    let sink_pad = webrtcbin
        .request_pad_simple("sink_%u")
        .expect("Failed to request sink pad from webrtcbin");

    if src_pad.link(&sink_pad).is_err() {
        eprintln!("Failed to link rtph264pay to webrtcbin");
    }

    pipeline.add_many(&[&src, &capsfilter, &overlay, &conv, &enc, &queue, &pay, &webrtcbin])?;
    gst::Element::link_many(&[&src, &capsfilter,&overlay, &conv, &enc, &queue, &pay])?;
    // pipeline.add_many(&[&src, &conv, &enc, &queue, &pay, &webrtcbin])?;
    // gst::Element::link_many(&[&src, &conv, &enc, &queue, &pay])?;
    pay.link(&webrtcbin)?;

    webrtcbin.connect("on-negotiation-needed", false, move |args| {
        let webrtc = args[0]
            .get::<gst::Element>()
            .expect("Invalid webrtcbin");

        let value = to_browser_tx.clone();
        let webrtc2 = webrtc.clone();
        let promise = gst::Promise::with_change_func(move |reply| {
            if let Ok(Some(reply)) = reply {
                let offer = reply
                    .value("offer")
                    .unwrap()
                    .get::<gst_webrtc::WebRTCSessionDescription>()
                    .unwrap();
                webrtc2
                    .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);

                let sdp_str = offer.sdp().as_text().unwrap();
                let msg = serde_json::to_string(&SignalMessage::Sdp {
                    sdp: sdp_str,
                    kind: "offer".into(),
                })
                .unwrap();
                let _ = value.send(msg);
            }
        });
        webrtc.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);

        None
    });

    Ok(pipeline)
}
