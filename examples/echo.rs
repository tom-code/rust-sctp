const BND: &str = "0.0.0.0:9999";

#[tokio::main]
async fn main() {
    let socket = sctp::SctpSocketTokio::new().unwrap();
    socket
        .set_init(sctp::Init {
            sinit_num_ostreams: 5,
            sinit_max_instreams: 5,
            sinit_max_attempts: 0,
            sinit_max_init_timeo: 0,
        })
        .unwrap();
    socket.bind(BND.parse().unwrap()).unwrap();
    println!("listening on {}", BND);
    socket.listen(8).unwrap();
    loop {
        let mut client = socket.accept().await.unwrap();
        client.set_ppid(3);
        println!("incoming connection");
        tokio::spawn(async move {
            loop {
                let mut buffer = [0; 1024 * 10];
                let received = client.recvmsg(&mut buffer).await;
                match received {
                    Err(e) => {
                        println!("receive error = closing {}", e);
                        break;
                    }
                    Ok(0) => {
                        println!("got 0 bytes - closing");
                        break;
                    }
                    Ok(len) => {
                        println!("got data {}", len);
                        let res = client.sendmsg_def(&buffer[..len]).await;
                        println!("send result: {:?}", res)
                    }
                }
            }
        });
    }
}
