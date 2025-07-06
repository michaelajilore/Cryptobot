use futures_util::{SinkExt, StreamExt};
use url::Url;
use serde_json::json;
use std::thread;
use core_affinity;

async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Erigon WebSocket URL (change if needed)
    let erigon_ws_url = "ws://localhost:8546";

    // Connect to the WebSocket
    let url = Url::parse(erigon_ws_url)?;
    let (mut ws_stream, _) = connect_async(url).await?;
    println!("Connected to Erigon WebSocket at {}", erigon_ws_url);
}

struct thrdmanip;

impl thrdmanip{
    fn ThreadSpawn(){
        let dexcount = 4
        let dexadd: [&str;4] = ["","","",""]
        let dexevadd: [&str;4] = ["","","",""]
        for i in 0..dexcount;{
            let handle = thread::spawn(|| {

            });


        }
    }
}
