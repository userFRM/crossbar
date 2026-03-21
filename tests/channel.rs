use crossbar::*;
use std::time::Duration;

fn unique_name(suffix: &str) -> String {
    format!("test-ch-{}-{suffix}", std::process::id())
}

#[test]
fn channel_bidirectional() {
    let name = unique_name("bidir");

    let name2 = name.clone();
    let server = std::thread::spawn(move || {
        let mut srv = Channel::listen(&name2, Config::default(), Duration::from_secs(5)).unwrap();
        let msg = srv.recv().unwrap();
        assert_eq!(&*msg, b"ping");
        drop(msg);
        srv.send(b"pong").unwrap();
    });

    std::thread::sleep(Duration::from_millis(50));

    let mut cli = Channel::connect(&name, Config::default(), Duration::from_secs(5)).unwrap();
    cli.send(b"ping").unwrap();
    let reply = cli.recv().unwrap();
    assert_eq!(&*reply, b"pong");

    server.join().unwrap();
}

#[test]
fn channel_loan_pattern() {
    let name = unique_name("loan");

    let name2 = name.clone();
    let server = std::thread::spawn(move || {
        let srv = Channel::listen(&name2, Config::default(), Duration::from_secs(5)).unwrap();
        let msg = srv.recv().unwrap();
        assert_eq!(msg.len(), 8);
        let val = u64::from_le_bytes(msg[..8].try_into().unwrap());
        assert_eq!(val, 42);
    });

    std::thread::sleep(Duration::from_millis(50));

    let mut cli = Channel::connect(&name, Config::default(), Duration::from_secs(5)).unwrap();
    let mut loan = cli.loan().unwrap();
    loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
    loan.set_len(8).unwrap();
    loan.publish();

    server.join().unwrap();
}

#[test]
fn channel_multiple_messages() {
    let name = unique_name("multi");

    let name2 = name.clone();
    let server = std::thread::spawn(move || {
        let mut srv = Channel::listen(&name2, Config::default(), Duration::from_secs(5)).unwrap();
        for i in 0u32..10 {
            let msg = srv.recv().unwrap();
            let val = u32::from_le_bytes(msg[..4].try_into().unwrap());
            assert_eq!(val, i);
            drop(msg);
            srv.send(&(i * 10).to_le_bytes()).unwrap();
        }
    });

    std::thread::sleep(Duration::from_millis(50));

    let mut cli = Channel::connect(&name, Config::default(), Duration::from_secs(5)).unwrap();
    for i in 0u32..10 {
        cli.send(&i.to_le_bytes()).unwrap();
        let reply = cli.recv().unwrap();
        let val = u32::from_le_bytes(reply[..4].try_into().unwrap());
        assert_eq!(val, i * 10);
    }

    server.join().unwrap();
}
