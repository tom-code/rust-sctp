use sctp::Notification;

const REMOTE: &str = "127.0.0.1:9999";

#[tokio::main]
async fn main() {
    //connect

    let (notification_queue_sender, mut notification_queue_receiver) = tokio::sync::mpsc::channel::<sctp::Notification>(32);
    let notification_task = tokio::spawn(async move {
        'lo: loop {
            let notification = notification_queue_receiver.recv().await;
            println!("notification {:?}", notification);
            match notification {
                Some(Notification::Shutdown()) => {
                    break 'lo
                },
                Some(_) => {},
                None => {},
            }
        }
    });

    let connected = std::sync::Arc::new(loop {
        let mut socket = sctp::SctpSocketTokio::new().unwrap();
        socket
            .set_init(sctp::Init {
                sinit_num_ostreams: 5,
                sinit_max_instreams: 5,
                sinit_max_attempts: 0,
                sinit_max_init_timeo: 0,
            })
            .unwrap();
        socket
            .set_rto(sctp::RtoInfo {
                assoc_id: 0,
                srto_initial: 222,
                srto_max: 333,
                srto_min: 111 })
                .unwrap();
        socket.set_rcvbuf(100000).unwrap();

        socket.subscribe_notifications(notification_queue_sender.clone());

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
        let mut info = sctp::SndRcvInfo::default();
        let mut buffer = [0; 1024 * 32];
        loop {
            match connected2.recvmsg_detailed(&mut buffer, &mut info).await {
                Err(e) => {
                    println!("read error {:?}", e);
                    break;
                }
                Ok(d) => {
                    println!("did read {:?} {:?}", d, info);
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
    notification_task.await.unwrap();
    println!("read done");
}
