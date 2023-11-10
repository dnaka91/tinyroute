use std::env::args;
use std::io::{stdin, Result};
use std::thread;
use std::time::Duration;

use flume::Receiver;
use log::debug;
use tinyroute::client::{connect, ClientMessage, UdsClient};
use tinyroute::frame::{Frame, FramedMessage};

fn input() -> Receiver<FramedMessage> {
    let (tx, rx) = flume::unbounded();

    thread::spawn(move || -> Result<()> {
        let mut buffer = String::new();
        let stdin = stdin();

        loop {
            buffer.clear();

            if let Err(e) = stdin.read_line(&mut buffer) {
                debug!("shutdown input");
                return Err(e);
            }

            buffer.pop();
            debug!("sending message: {buffer}");
            let framed_msg = Frame::frame_message(buffer.as_bytes());
            let _ = tx.send(framed_msg);
        }
    });

    rx
}

async fn run(rx: Receiver<FramedMessage>, addr: String) {
    let client = UdsClient::connect(addr).await.unwrap();
    let (write_tx, read_rx) = connect(client, Some(Duration::from_secs(30)));

    let read_handle = tokio::spawn(output(read_rx));

    while let Ok(bytes) = rx.recv() {
        if write_tx
            .send_async(ClientMessage::Payload(bytes))
            .await
            .is_err()
        {
            break;
        }
    }

    let _ = read_handle.await;
}

async fn output(read_rx: Receiver<Vec<u8>>) -> Option<()> {
    loop {
        let payload = read_rx.recv_async().await.ok()?;
        let data = String::from_utf8(payload).ok()?;
        println!("data: {}", data);
    }
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    let addr = args().nth(1).expect("provide an address");
    let rx = input();
    run(rx, addr).await;
}
