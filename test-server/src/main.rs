#[tokio::main]
async fn main() {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    test_server::TestServer::spawn_blocking(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        8080,
    ))
    .await;
}
