const REMOTE: &str = "127.0.0.1:9999";

#[tokio::main]
async fn main() {
    //connect
    let connected = std::sync::Arc::new(loop {
        let socket = sctp::SctpSocketTokio::new().unwrap();
        socket
            .set_init(sctp::Init {
                sinit_num_ostreams: 5,
                sinit_max_instreams: 5,
                sinit_max_attempts: 0,
                sinit_max_init_timeo: 0,
            })
            .unwrap();
        let res = socket.connect(REMOTE.parse().unwrap()).await;
        match res {
            Err(e) => {
                println!("{:?}", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
            Ok(_) => break socket,
        }
    });
    let connected2 = connected.clone();

    // read all data and print number of read bytes
    let read_task = tokio::spawn(async move {
        loop {
            let mut buffer = [0; 1024 * 32];
            match connected2.recvmsg(&mut buffer).await {
                Err(e) => {
                    println!("read error {:?}", e);
                    break;
                }
                Ok(d) => {
                    println!("did read {:?}", d);
                    if d == 0 {
                        break;
                    }
                }
            }
        }
    });

    // send 10 messages
    for _ in 1..10 {
        connected
            .sendmsg_def("1234567890".as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    // then shutdown
    connected.shutdown(std::net::Shutdown::Both).unwrap();

    read_task.await.unwrap();
    println!("read done");
}
